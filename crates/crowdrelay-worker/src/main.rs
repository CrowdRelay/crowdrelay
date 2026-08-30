#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used,
    )
)]
#![deny(clippy::dbg_macro)]

use std::{
    collections::HashMap, env, fs::File, future::pending, io::Read, sync::Arc, time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{
    autopilot::PostgresAutopilotRepository, config::Config, database, observability,
};
use crowdrelay_worker::{
    ad_conversion::AdConversionWorker,
    agent_outcomes::AgentOutcomeWorker,
    attribution::AttributionWorker,
    audience_graph::AudienceGraphSweeper,
    autopilot::{AutopilotWorker, TeamEmailDispatchWorker},
    bootstrap::{BootstrapSpec, bootstrap, bootstrap_admission_access, bootstrap_team_operations},
    community_executor::CommunityExecutorWorker,
    discovery::{DiscoveryConfig, RedditDiscoveryWorker},
    draws::{WeightedDrawWorker, WeightedDrawWorkerConfig},
    event_sync::{EventSyncWorker, EventSyncWorkerConfig},
    growth_metric_sync::GrowthMetricSyncWorker,
    ops_watchdog::OpsWatchdogWorker,
    outbox::{MapSecretProvider, OutboxWorker, OutboxWorkerConfig, SecretProvider, SecretValue},
    push_delivery::PushDeliveryWorker,
    receipt_reconciliation::ReceiptReconciliationWorker,
    reminders::EventReminderScheduler,
    replay::{ReplayOptions, parse_replay_options, run_replay},
    retention::{RetentionWorker, RetentionWorkerConfig},
};
use sqlx::PgPool;
use tokio::{
    signal,
    sync::watch,
    task::JoinSet,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

const DATABASE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const OPS_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Receipt reconciliation cadence. Slower than the watchdog: each cycle
/// only needs to notice gaps older than hours, not minutes.
const RECEIPT_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Reddit public search tolerates slow, sparse polling.
const DISCOVERY_SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// The graph changes at human speed; an hour of decay lag is invisible.
const AUDIENCE_GRAPH_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Agent outcome polling cadence. Outcomes are not time-critical — the
/// operator approves them on the board — so 30s is plenty.
const AGENT_OUTCOME_POLL_INTERVAL: Duration = Duration::from_secs(30);
const WEBHOOK_SECRETS_FILE_KEY: &str = "CROWDRELAY_WEBHOOK_SECRETS_FILE";
const BOOTSTRAP_JSON_KEY: &str = "CROWDRELAY_BOOTSTRAP_JSON";
const BOOTSTRAP_FILE_KEY: &str = "CROWDRELAY_BOOTSTRAP_FILE";
const MAX_BOOTSTRAP_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WEBHOOK_SECRETS_FILE_BYTES: u64 = 1024 * 1024;
const MAX_WEBHOOK_SECRET_REFERENCES: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Run,
    Migrate,
    Bootstrap,
    Setup,
    Replay(ReplayOptions),
}

impl Command {
    const KNOWN: &'static str = "`run`, `migrate`, `bootstrap`, `setup`, or `replay`";
}

#[tokio::main]
async fn main() -> Result<()> {
    let command = parse_command(env::args().skip(1))?;
    let config = Config::from_env().context("invalid CrowdRelay configuration")?;
    observability::init("crowdrelay-worker").context("failed to initialize structured tracing")?;
    observability::install_panic_hook("crowdrelay-worker");

    let database = database::connect(&config.database)
        .await
        .context("failed to connect to PostgreSQL")?;

    match command {
        Command::Migrate => {
            run_migrations(&database).await?;
        }
        Command::Bootstrap => {
            run_bootstrap(&database, &config).await?;
        }
        Command::Setup => {
            run_migrations(&database).await?;
            run_bootstrap(&database, &config).await?;
        }
        Command::Replay(options) => {
            let workspace = trusted_workspace_id(&database, &config).await?;
            run_replay(
                &database,
                workspace,
                config.workspace_slug.as_str(),
                options,
            )
            .await?;
        }
        Command::Run => {
            tracing::info!(environment = %config.environment, "CrowdRelay worker started");
            run(database.clone(), &config).await?;
            tracing::info!("CrowdRelay worker stopped");
        }
    }

    database.close().await;

    Ok(())
}

fn parse_command(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut args = args.into_iter();
    let head = args.next();
    let rest: Vec<String> = args.collect();
    let command = match head.as_deref() {
        None | Some("run") => {
            reject_extras(&rest)?;
            Command::Run
        }
        Some("migrate") => {
            reject_extras(&rest)?;
            Command::Migrate
        }
        Some("bootstrap") => {
            reject_extras(&rest)?;
            Command::Bootstrap
        }
        Some("setup") => {
            reject_extras(&rest)?;
            Command::Setup
        }
        Some("replay") => Command::Replay(parse_replay_options(rest)?),
        Some(other) => bail!(
            "unknown worker command `{other}`; expected {}",
            Command::KNOWN
        ),
    };

    Ok(command)
}

fn reject_extras(rest: &[String]) -> Result<()> {
    if let Some(extra) = rest.first() {
        bail!("unexpected worker argument `{extra}`");
    }
    Ok(())
}

async fn run_migrations(database_pool: &PgPool) -> Result<()> {
    tracing::info!("running database migrations");
    database::migrate(database_pool)
        .await
        .context("database migration failed")?;
    tracing::info!("database migrations completed");
    Ok(())
}

async fn run_bootstrap(database_pool: &PgPool, config: &Config) -> Result<()> {
    let document = load_bootstrap_document()?;
    let spec = BootstrapSpec::parse(&document, config.environment.is_production())
        .context("invalid workspace bootstrap document")?;
    let result = bootstrap(
        database_pool,
        &config.workspace_slug,
        &config.database,
        &spec,
    )
    .await
    .context("workspace bootstrap failed")?;
    bootstrap_admission_access(
        database_pool,
        &config.workspace_slug,
        &config.database,
        &config.admission_security.admin_member_email,
        &config.admission_security.staff_member_email,
        config.admission_security.admin_api_key_sha256,
        config.admission_security.staff_api_key_sha256,
    )
    .await
    .context("admission access bootstrap failed")?;
    let team_profiles_changed = bootstrap_team_operations(
        database_pool,
        &config.workspace_slug,
        &config.database,
        &config.team_operations,
    )
    .await
    .context("team operations bootstrap failed")?;

    tracing::info!(
        workspace_id = %result.workspace_id,
        changed_rows = result.changes.total(),
        team_profiles_changed,
        audit_recorded = result.audit_recorded,
        "workspace bootstrap completed"
    );
    Ok(())
}

async fn run(database: PgPool, config: &Config) -> Result<()> {
    let secret_provider = load_secret_provider()?;
    timeout(
        config.database.operation_timeout,
        validate_active_endpoint_secrets(&database, &secret_provider),
    )
    .await
    .context("active webhook endpoint validation timed out")??;
    let outbox_config = OutboxWorkerConfig {
        database_operation_timeout: config.database.operation_timeout,
        allow_http_endpoints: !config.environment.is_production(),
        ..OutboxWorkerConfig::default()
    };
    let outbox_worker = OutboxWorker::new(
        database.clone(),
        Arc::clone(&secret_provider),
        outbox_config,
    )
    .context("invalid outbox worker configuration")?;
    let reminder_scheduler = EventReminderScheduler::new(
        database.clone(),
        config.event_reminder_poll_interval,
        config.database.operation_timeout,
        config.database.lock_timeout,
    )
    .context("invalid event reminder scheduler configuration")?;
    let retention_worker = RetentionWorker::new(
        database.clone(),
        RetentionWorkerConfig {
            operation_timeout: config.database.operation_timeout,
            lock_timeout: config.database.lock_timeout,
            ..RetentionWorkerConfig::default()
        },
    )
    .context("invalid retention worker configuration")?;
    let event_sync_worker = EventSyncWorker::new(
        database.clone(),
        EventSyncWorkerConfig::with_database_timeouts(
            config.database.operation_timeout,
            config.database.lock_timeout,
        ),
    )
    .context("invalid event sync worker configuration")?;
    let weighted_draw_worker = if config.random_draws_enabled {
        Some(
            WeightedDrawWorker::new(
                database.clone(),
                WeightedDrawWorkerConfig::with_database_timeouts(
                    config.database.operation_timeout,
                    config.database.lock_timeout,
                ),
            )
            .context("invalid weighted draw worker configuration")?,
        )
    } else {
        tracing::info!("weighted draws are disabled by configuration");
        None
    };
    let workspace_id = trusted_workspace_id(&database, config).await?;
    let push_delivery_worker = if config.push_delivery.runtime_enabled {
        Some(PushDeliveryWorker::from_env(
            database.clone(),
            workspace_id,
            config.database.operation_timeout,
        )?)
    } else {
        tracing::info!("fan push delivery is disabled by process configuration");
        None
    };
    let autopilot_repository = PostgresAutopilotRepository::new(database.clone(), &config.database);
    let team_email_worker = TeamEmailDispatchWorker::new(
        autopilot_repository.clone(),
        workspace_id,
        config.autopilot_poll_interval.min(Duration::from_secs(60)),
    );
    let attribution_worker = AttributionWorker::new(
        autopilot_repository.clone(),
        workspace_id,
        config.autopilot_poll_interval.min(Duration::from_secs(30)),
    );
    let autopilot_worker = if config.autopilot_enabled {
        Some(AutopilotWorker::new(
            autopilot_repository,
            workspace_id,
            config.autopilot_poll_interval,
        ))
    } else {
        tracing::info!(
            "ViryaOS autonomous decisioning is disabled; team-email dispatch remains capability-gated"
        );
        None
    };
    let ops_watchdog = OpsWatchdogWorker::new(
        database.clone(),
        workspace_id,
        OPS_WATCHDOG_INTERVAL,
        config.database.operation_timeout,
    );
    // Receipt reconciliation: flags dispatched actions whose executor
    // receipts never arrived (transitions them to `unknown`) and resolves
    // existing `unknown` actions from late receipts or the community
    // executor's own post rows. Without it, a lost receipt strands the
    // action as a fake `succeeded` with no learning evidence.
    let receipt_reconciliation = ReceiptReconciliationWorker::new(
        database.clone(),
        workspace_id,
        RECEIPT_RECONCILIATION_INTERVAL,
        config.database.operation_timeout,
    );
    let agent_outcome_worker = if config.agent_outcomes_enabled {
        Some(AgentOutcomeWorker::new(
            database.clone(),
            workspace_id,
            AGENT_OUTCOME_POLL_INTERVAL,
            config.database.operation_timeout,
        ))
    } else {
        tracing::info!("agent outcome ingestion is disabled by process configuration");
        None
    };
    // Community engagement executor: posts approved community.engage.request
    // actions to Reddit via the agents service browser session. When the
    // agents service auth key is configured, posts automatically. When not
    // configured, runs in manual mode: creates `community_posts` rows marked
    // `awaiting_manual_post` — the operator posts manually and registers the
    // URL via the API. Metrics polling works in both modes.
    let agent_service_auth_key = std::env::var("CROWDRELAY_AGENT_SERVICE_AUTH_KEY").ok();
    let manual_mode = agent_service_auth_key.is_none();
    let community_executor = match CommunityExecutorWorker::new(
        database.clone(),
        workspace_id,
        config.database.operation_timeout,
        manual_mode,
        config.reddit_proxy_url.clone(),
        config.agent_service_url.clone(),
        agent_service_auth_key,
    ) {
        Ok(worker) => {
            if manual_mode {
                tracing::info!(
                    "community executor running in MANUAL MODE — posts will be drafted but not posted automatically; operator must post manually and register the URL via the API"
                );
            }
            Some(worker)
        }
        Err(error) => {
            tracing::warn!(error = %error, "community executor disabled: HTTP client build failed");
            None
        }
    };
    let audience_graph_sweeper = AudienceGraphSweeper::new(
        database.clone(),
        workspace_id,
        AUDIENCE_GRAPH_SWEEP_INTERVAL,
        config.database.operation_timeout,
    );
    // Discovery sweeps stay dark until an operator configures queries; the
    // adapter then runs on the same polite cadence as every other worker.
    let discovery_config = DiscoveryConfig::from_env();
    let reddit_discovery = if discovery_config.enabled() {
        Some(RedditDiscoveryWorker::new(
            database.clone(),
            workspace_id,
            discovery_config,
            DISCOVERY_SWEEP_INTERVAL,
            config.database.operation_timeout,
            config.agent_service_url.clone(),
            std::env::var("CROWDRELAY_AGENT_SERVICE_AUTH_KEY").ok(),
        )?)
    } else {
        tracing::info!("reddit discovery disabled; CROWDRELAY_DISCOVERY_REDDIT_QUERIES not set");
        None
    };
    let ad_conversion_worker = if config.ad_conversion.any_enabled() {
        Some(AdConversionWorker::new(
            database.clone(),
            workspace_id,
            config.ad_conversion.clone(),
            config.database.operation_timeout,
        )?)
    } else {
        tracing::info!("ad conversion disabled; no platforms (Meta/Google/Bandsintown) enabled");
        None
    };
    // Growth metric sync: reactive worker that LISTENs on Postgres NOTIFY
    // for new YouTube/Spotify/Reddit connections and syncs subscriber/follower
    // counts into viryaos_growth_metric_series. No polling — wakes only on
    // NOTIFY or when the next scheduled sync time arrives.
    let youtube_api_key = std::env::var("CROWDRELAY_YOUTUBE_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let spotify_client_id = std::env::var("CROWDRELAY_SPOTIFY_CLIENT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let spotify_client_secret = std::env::var("CROWDRELAY_SPOTIFY_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let reddit_proxy_url = std::env::var("CROWDRELAY_REDDIT_PROXY_URL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let growth_metric_sync = GrowthMetricSyncWorker::new(
        database.clone(),
        youtube_api_key,
        spotify_client_id,
        spotify_client_secret,
        reddit_proxy_url,
        config.database.operation_timeout,
    )
    .context("invalid growth metric sync worker configuration")?;
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let reminder_shutdown = shutdown_receiver.clone();
    let retention_shutdown = shutdown_receiver.clone();
    let event_sync_shutdown = shutdown_receiver.clone();
    let draw_shutdown = shutdown_receiver.clone();
    let autopilot_shutdown = shutdown_receiver.clone();
    let team_email_shutdown = shutdown_receiver.clone();
    let push_delivery_shutdown = shutdown_receiver.clone();
    let ops_watchdog_shutdown = shutdown_receiver.clone();
    let receipt_reconciliation_shutdown = shutdown_receiver.clone();
    let discovery_shutdown = shutdown_receiver.clone();
    let audience_graph_shutdown = shutdown_receiver.clone();
    let ad_conversion_shutdown = shutdown_receiver.clone();
    let agent_outcome_shutdown = shutdown_receiver.clone();
    let community_executor_shutdown = shutdown_receiver.clone();
    let growth_metric_sync_shutdown = shutdown_receiver.clone();
    let attribution_shutdown = shutdown_receiver.clone();

    // Growth readiness summary: tells the operator exactly which growth
    // systems are active and what's missing. This is the single most
    // important log line for diagnosing "why isn't the system growing fans?"
    // Each component maps to a stage of the North Star loop:
    //   aggregate → grow → convert → learn
    let growth_readiness = GrowthReadiness {
        autopilot_enabled: autopilot_worker.is_some(),
        agent_outcomes_enabled: agent_outcome_worker.is_some(),
        push_delivery_enabled: push_delivery_worker.is_some(),
        community_executor_enabled: community_executor.is_some(),
        reddit_discovery_enabled: reddit_discovery.is_some(),
        ad_conversion_enabled: ad_conversion_worker.is_some(),
        random_draws_enabled: weighted_draw_worker.is_some(),
    };
    growth_readiness.log();

    let mut runtime_tasks = JoinSet::new();
    runtime_tasks.spawn(async move {
        outbox_worker.run(shutdown_receiver).await;
        "outbox worker"
    });
    runtime_tasks.spawn(async move {
        reminder_scheduler.run(reminder_shutdown).await;
        "event reminder scheduler"
    });
    runtime_tasks.spawn(async move {
        retention_worker.run(retention_shutdown).await;
        "retention worker"
    });
    runtime_tasks.spawn(async move {
        event_sync_worker.run(event_sync_shutdown).await;
        "event sync worker"
    });
    runtime_tasks.spawn(async move {
        match weighted_draw_worker {
            Some(worker) => worker.run(draw_shutdown).await,
            None => wait_for_shutdown(draw_shutdown).await,
        }
        "weighted draw worker"
    });
    runtime_tasks.spawn(async move {
        match autopilot_worker {
            Some(worker) => worker.run(autopilot_shutdown).await,
            None => wait_for_shutdown(autopilot_shutdown).await,
        }
        "ViryaOS Autopilot worker"
    });
    runtime_tasks.spawn(async move {
        team_email_worker.run(team_email_shutdown).await;
        "ViryaOS team-email worker"
    });
    runtime_tasks.spawn(async move {
        match push_delivery_worker {
            Some(worker) => worker.run(push_delivery_shutdown).await,
            None => wait_for_shutdown(push_delivery_shutdown).await,
        }
        "fan push delivery worker"
    });
    runtime_tasks.spawn(async move {
        ops_watchdog.run(ops_watchdog_shutdown).await;
        "CrowdRelay ops watchdog"
    });
    runtime_tasks.spawn(async move {
        receipt_reconciliation
            .run(receipt_reconciliation_shutdown)
            .await;
        "receipt reconciliation"
    });
    runtime_tasks.spawn(async move {
        audience_graph_sweeper.run(audience_graph_shutdown).await;
        "audience graph sweeper"
    });
    if let Some(worker) = reddit_discovery {
        runtime_tasks.spawn(async move {
            worker.run(discovery_shutdown).await;
            "reddit discovery"
        });
    }
    if let Some(worker) = ad_conversion_worker {
        runtime_tasks.spawn(async move {
            worker.run(ad_conversion_shutdown).await;
            "ad conversion worker"
        });
    }
    runtime_tasks.spawn(async move {
        match agent_outcome_worker {
            Some(worker) => worker.run(agent_outcome_shutdown).await,
            None => wait_for_shutdown(agent_outcome_shutdown).await,
        }
        "agent outcome worker"
    });
    if let Some(worker) = community_executor {
        runtime_tasks.spawn(async move {
            worker.run(community_executor_shutdown).await;
            "community executor"
        });
    }
    if let Some(worker) = growth_metric_sync {
        runtime_tasks.spawn(async move {
            let _ = worker.run(growth_metric_sync_shutdown).await;
            "growth metric sync"
        });
    }
    runtime_tasks.spawn(async move {
        attribution_worker.run(attribution_shutdown).await;
        "attribution worker"
    });

    let mut checks = interval(DATABASE_CHECK_INTERVAL);
    checks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut was_available = None;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let runtime_result = loop {
        tokio::select! {
            first_exit = runtime_tasks.join_next() => {
                break unexpected_worker_exit(first_exit);
            }
            () = &mut shutdown => {
                tracing::info!("shutdown requested");
                break Ok(());
            }
            _ = checks.tick() => {
                let is_available = database::ping(
                    &database,
                    config.database.ping_timeout,
                )
                .await
                .is_ok();
                if was_available != Some(is_available) {
                    if is_available {
                        tracing::info!("worker database connection is healthy");
                    } else {
                        tracing::error!("worker database connection is unavailable");
                    }
                    was_available = Some(is_available);
                }
            }
        }
    };

    let _ = shutdown_sender.send(true);
    let shutdown_result = drain_worker_tasks(
        &mut runtime_tasks,
        config
            .database
            .operation_timeout
            .saturating_mul(2)
            .saturating_add(Duration::from_secs(2)),
    )
    .await;

    runtime_result.and(shutdown_result)
}

async fn trusted_workspace_id(database: &PgPool, config: &Config) -> Result<WorkspaceId> {
    let id = timeout(
        config.database.operation_timeout,
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1")
            .bind(config.workspace_slug.as_str())
            .fetch_optional(database),
    )
    .await
    .context("workspace lookup timed out")??
    .with_context(|| format!("workspace `{}` does not exist", config.workspace_slug))?;
    Ok(WorkspaceId::from_uuid(id))
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            break;
        }
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

fn unexpected_worker_exit(
    exit: Option<std::result::Result<&'static str, tokio::task::JoinError>>,
) -> Result<()> {
    match exit {
        Some(Ok(task_name)) => Err(anyhow!("{task_name} stopped before shutdown was requested")),
        Some(Err(error)) => Err(anyhow!("critical worker task failed: {error}")),
        None => Err(anyhow!(
            "all worker runtime tasks stopped before shutdown was requested"
        )),
    }
}

async fn drain_worker_tasks(
    runtime_tasks: &mut JoinSet<&'static str>,
    deadline: Duration,
) -> Result<()> {
    match timeout(deadline, drain_worker_tasks_inner(runtime_tasks)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::error!("worker runtime tasks exceeded graceful shutdown deadline");
            runtime_tasks.abort_all();
            while let Some(result) = runtime_tasks.join_next().await {
                match result {
                    Ok(task_name) => {
                        tracing::debug!(task_name, "worker stopped after abort request")
                    }
                    Err(error) if error.is_cancelled() => {
                        tracing::debug!("worker task cancellation completed");
                    }
                    Err(error) => {
                        tracing::error!(%error, "worker task failed while aborting");
                    }
                }
            }
            Err(anyhow!(
                "worker runtime tasks exceeded graceful shutdown deadline"
            ))
        }
    }
}

async fn drain_worker_tasks_inner(runtime_tasks: &mut JoinSet<&'static str>) -> Result<()> {
    let mut first_error: Option<anyhow::Error> = None;
    while let Some(result) = runtime_tasks.join_next().await {
        match result {
            Ok(task_name) => tracing::debug!(task_name, "worker stopped cleanly"),
            Err(error) => {
                tracing::error!(%error, "worker task failed during shutdown");
                if first_error.is_none() {
                    first_error = Some(anyhow!("worker task failed during shutdown: {error}"));
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn load_bootstrap_document() -> Result<String> {
    if let Ok(document) = env::var(BOOTSTRAP_JSON_KEY) {
        ensure!(
            !document.trim().is_empty(),
            "{BOOTSTRAP_JSON_KEY} must not be empty"
        );
        return Ok(document);
    }

    let path = env::var(BOOTSTRAP_FILE_KEY).with_context(|| {
        format!("either {BOOTSTRAP_JSON_KEY} or {BOOTSTRAP_FILE_KEY} is required for workspace bootstrap")
    })?;
    ensure!(
        !path.trim().is_empty(),
        "{BOOTSTRAP_FILE_KEY} must not be empty"
    );
    let document = read_bounded_file(&path, MAX_BOOTSTRAP_FILE_BYTES, "bootstrap")?;
    String::from_utf8(document).context("bootstrap file is not valid UTF-8")
}

fn load_secret_provider() -> Result<Arc<dyn SecretProvider>> {
    let Some(path) = env::var_os(WEBHOOK_SECRETS_FILE_KEY) else {
        return Ok(Arc::new(MapSecretProvider::default()));
    };
    let path = path
        .into_string()
        .map_err(|_| anyhow::anyhow!("{WEBHOOK_SECRETS_FILE_KEY} is not valid Unicode"))?;
    ensure!(
        !path.trim().is_empty(),
        "{WEBHOOK_SECRETS_FILE_KEY} must not be empty"
    );
    let document = read_bounded_file(&path, MAX_WEBHOOK_SECRETS_FILE_BYTES, "webhook secrets")?;
    let raw: HashMap<String, String> =
        serde_json::from_slice(&document).context("webhook secrets file is not valid JSON")?;
    ensure!(
        raw.len() <= MAX_WEBHOOK_SECRET_REFERENCES,
        "webhook secrets file contains too many references"
    );
    let mut secrets = HashMap::with_capacity(raw.len());

    for (reference, value) in raw {
        ensure!(
            valid_secret_reference(&reference),
            "webhook secrets file contains an invalid reference"
        );
        let value = SecretValue::new(value.into_bytes())
            .context("webhook secrets file contains an invalid secret value")?;
        secrets.insert(reference, value);
    }

    Ok(Arc::new(MapSecretProvider::new(secrets)))
}

fn read_bounded_file(path: &str, maximum_bytes: u64, label: &str) -> Result<Vec<u8>> {
    let file =
        File::open(path).with_context(|| format!("failed to open {label} file at {path}"))?;
    let mut document = Vec::new();
    file.take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut document)
        .with_context(|| format!("failed to read {label} file at {path}"))?;
    ensure!(
        u64::try_from(document.len()).unwrap_or(u64::MAX) <= maximum_bytes,
        "{label} file exceeds the size limit"
    );
    Ok(document)
}

fn valid_secret_reference(reference: &str) -> bool {
    (1..=128).contains(&reference.len())
        && reference.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

async fn validate_active_endpoint_secrets(
    database: &PgPool,
    provider: &Arc<dyn SecretProvider>,
) -> Result<()> {
    let references = sqlx::query_scalar::<_, String>(
        r#"
        SELECT DISTINCT signing_secret_ref
        FROM webhook_endpoints
        WHERE active
        ORDER BY signing_secret_ref
        "#,
    )
    .fetch_all(database)
    .await
    .context("failed to inspect active webhook endpoint configuration")?;

    for reference in references {
        provider.resolve(&reference).await.with_context(|| {
            format!("secret for active webhook endpoint reference `{reference}` is unavailable")
        })?;
    }

    Ok(())
}

/// Growth readiness state: which fan-growth systems are active at startup.
/// Logged once at boot so the operator can immediately see what's running
/// and what needs configuration. Maps directly to the North Star loop:
///   aggregate → grow → convert → learn
struct GrowthReadiness {
    /// The deterministic brain. Without this, no growth decisions are made.
    /// Env: CROWDRELAY_AUTOPILOT_ENABLED=true
    autopilot_enabled: bool,
    /// LLM worker outcome ingestion. Without this, the brain can't see what
    /// the agents produced. Env: CROWDRELAY_AGENT_OUTCOMES_ENABLED (default: true)
    agent_outcomes_enabled: bool,
    /// Fan push notification delivery. Without this, Signal invites can't
    /// be sent. Env: CROWDRELAY_PUSH_DELIVERY_ENABLED=true
    push_delivery_enabled: bool,
    /// Reddit posting executor. Without this, community engagement posts are
    /// drafted but never posted. Env: CROWDRELAY_AGENT_SERVICE_AUTH_KEY
    community_executor_enabled: bool,
    /// Reddit subreddit discovery. Without this, the system can't find new
    /// communities to engage with. Env: CROWDRELAY_DISCOVERY_REDDIT_QUERIES
    reddit_discovery_enabled: bool,
    /// Ad conversion tracking (Meta/Google/Bandsintown). Attribution, not
    /// fan creation. Env: CROWDRELAY_META_CAPI_ENABLED, etc.
    ad_conversion_enabled: bool,
    /// Referral-weighted reward draws. Fan-led growth mechanic.
    /// Env: CROWDRELAY_RANDOM_DRAWS_ENABLED=true
    random_draws_enabled: bool,
}

impl GrowthReadiness {
    /// Logs a structured growth readiness summary. Each component is logged
    /// as a field so it can be searched/alerted on in log aggregation.
    fn log(&self) {
        let active = [
            self.autopilot_enabled,
            self.agent_outcomes_enabled,
            self.push_delivery_enabled,
            self.community_executor_enabled,
            self.reddit_discovery_enabled,
            self.ad_conversion_enabled,
            self.random_draws_enabled,
        ]
        .iter()
        .filter(|&&v| v)
        .count();

        tracing::info!(
            active_components = active,
            total_components = 7,
            autopilot = self.autopilot_enabled,
            agent_outcomes = self.agent_outcomes_enabled,
            push_delivery = self.push_delivery_enabled,
            community_executor = self.community_executor_enabled,
            reddit_discovery = self.reddit_discovery_enabled,
            ad_conversion = self.ad_conversion_enabled,
            random_draws = self.random_draws_enabled,
            "growth readiness: {}/7 fan-growth components active",
            active,
        );

        if !self.autopilot_enabled {
            tracing::warn!(
                "growth readiness: autopilot is OFF — set CROWDRELAY_AUTOPILOT_ENABLED=true to enable the deterministic brain"
            );
        }
        if !self.agent_outcomes_enabled {
            tracing::warn!(
                "growth readiness: agent outcomes are OFF — set CROWDRELAY_AGENT_OUTCOMES_ENABLED=true to feed LLM worker results to the brain"
            );
        }
        if !self.community_executor_enabled {
            tracing::warn!(
                "growth readiness: community executor is OFF — set CROWDRELAY_AGENT_SERVICE_AUTH_KEY for automatic posting via the agents service browser, or the executor will run in manual mode (operator posts manually)"
            );
        }
        if !self.reddit_discovery_enabled {
            tracing::warn!(
                "growth readiness: reddit discovery is OFF — set CROWDRELAY_DISCOVERY_REDDIT_QUERIES to find new communities to engage with"
            );
        }
        if !self.push_delivery_enabled {
            tracing::warn!(
                "growth readiness: push delivery is OFF — set CROWDRELAY_PUSH_DELIVERY_ENABLED=true to send Signal push notifications"
            );
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = signal::ctrl_c().await {
            tracing::error!(%error, "failed to install Ctrl+C signal handler");
            pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to install SIGTERM handler");
                pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, ReplayOptions, parse_command};

    #[test]
    fn defaults_to_run() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(parse_command(Vec::<String>::new())?, Command::Run);
        Ok(())
    }

    #[test]
    fn accepts_migrate() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(parse_command(["migrate".to_owned()])?, Command::Migrate);
        Ok(())
    }

    #[test]
    fn accepts_bootstrap_and_setup() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(parse_command(["bootstrap".to_owned()])?, Command::Bootstrap);
        assert_eq!(parse_command(["setup".to_owned()])?, Command::Setup);
        Ok(())
    }

    #[test]
    fn rejects_unknown_command() {
        assert!(parse_command(["raffle".to_owned()]).is_err());
    }

    #[test]
    fn accepts_replay_with_options() -> Result<(), Box<dyn std::error::Error>> {
        let default = parse_command(["replay".to_owned()])?;
        assert_eq!(
            default,
            Command::Replay(ReplayOptions {
                since_days: 30,
                json: false
            })
        );
        let tuned = parse_command([
            "replay".to_owned(),
            "--since-days".to_owned(),
            "90".to_owned(),
            "--json".to_owned(),
        ])?;
        assert_eq!(
            tuned,
            Command::Replay(ReplayOptions {
                since_days: 90,
                json: true
            })
        );
        assert!(parse_command(["replay".to_owned(), "--bogus".to_owned()]).is_err());
        Ok(())
    }
}
