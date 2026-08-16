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
    autopilot::{AutopilotWorker, TeamEmailDispatchWorker},
    bootstrap::{BootstrapSpec, bootstrap, bootstrap_admission_access, bootstrap_team_operations},
    draws::{WeightedDrawWorker, WeightedDrawWorkerConfig},
    event_sync::{EventSyncWorker, EventSyncWorkerConfig},
    ops_watchdog::OpsWatchdogWorker,
    outbox::{MapSecretProvider, OutboxWorker, OutboxWorkerConfig, SecretProvider, SecretValue},
    push_delivery::PushDeliveryWorker,
    reminders::EventReminderScheduler,
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
const WEBHOOK_SECRETS_FILE_KEY: &str = "CROWDRELAY_WEBHOOK_SECRETS_FILE";
const BOOTSTRAP_JSON_KEY: &str = "CROWDRELAY_BOOTSTRAP_JSON";
const BOOTSTRAP_FILE_KEY: &str = "CROWDRELAY_BOOTSTRAP_FILE";
const MAX_BOOTSTRAP_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WEBHOOK_SECRETS_FILE_BYTES: u64 = 1024 * 1024;
const MAX_WEBHOOK_SECRET_REFERENCES: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Run,
    Migrate,
    Bootstrap,
    Setup,
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
    let command = match args.next().as_deref() {
        None | Some("run") => Command::Run,
        Some("migrate") => Command::Migrate,
        Some("bootstrap") => Command::Bootstrap,
        Some("setup") => Command::Setup,
        Some(other) => bail!(
            "unknown worker command `{other}`; expected `run`, `migrate`, `bootstrap`, or `setup`"
        ),
    };

    if let Some(extra) = args.next() {
        bail!("unexpected worker argument `{extra}`");
    }

    Ok(command)
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
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let reminder_shutdown = shutdown_receiver.clone();
    let retention_shutdown = shutdown_receiver.clone();
    let event_sync_shutdown = shutdown_receiver.clone();
    let draw_shutdown = shutdown_receiver.clone();
    let autopilot_shutdown = shutdown_receiver.clone();
    let team_email_shutdown = shutdown_receiver.clone();
    let push_delivery_shutdown = shutdown_receiver.clone();
    let ops_watchdog_shutdown = shutdown_receiver.clone();
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
    use super::{Command, parse_command};

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
}
