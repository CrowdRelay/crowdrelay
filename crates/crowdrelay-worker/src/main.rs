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
    autopilot::PostgresAutopilotRepository,
    community_intelligence::PostgresCommunityIntelligenceRepository, config::Config, database,
    observability,
};
use crowdrelay_worker::{
    ad_conversion::AdConversionWorker,
    agent_outcomes::AgentOutcomeWorker,
    attribution::AttributionWorker,
    audience_graph::AudienceGraphSweeper,
    autopilot::{AutopilotWorker, TeamEmailDispatchWorker},
    bootstrap::{BootstrapSpec, bootstrap, bootstrap_admission_access, bootstrap_team_operations},
    city_geocoding::CityGeocodeWorker,
    community_executor::CommunityExecutorWorker,
    community_intelligence::{
        adapter::SourceAdapter, brutalland::BrutallandAdapter, reddit::RedditAdapter,
        worker::CommunityIntelligenceWorker,
    },
    community_join_executor::CommunityJoinExecutorWorker,
    discord_executor::DiscordExecutorWorker,
    discovery::{DiscoveryConfig, RedditDiscoveryWorker, XDiscoveryWorker},
    draws::{WeightedDrawWorker, WeightedDrawWorkerConfig},
    event_sync::{EventSyncWorker, EventSyncWorkerConfig},
    growth_metric_sync::GrowthMetricSyncWorker,
    growth_readiness::GrowthReadiness,
    leadership::acquire_leadership,
    nearby_gigs::{DEFAULT_POLL_INTERVAL as NEARBY_GIG_POLL_INTERVAL, NearbyGigScheduler},
    ops_watchdog::OpsWatchdogWorker,
    outbox::{MapSecretProvider, OutboxWorker, OutboxWorkerConfig, SecretProvider, SecretValue},
    push_delivery::PushDeliveryWorker,
    receipt_reconciliation::ReceiptReconciliationWorker,
    reminders::EventReminderScheduler,
    replay::{ReplayOptions, parse_replay_options, run_replay},
    retention::{RetentionWorker, RetentionWorkerConfig},
    social_post_executor::SocialPostExecutorWorker,
    telegram_executor::TelegramExecutorWorker,
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
    Run { standby: bool },
    Migrate,
    Bootstrap,
    Setup,
    Replay(ReplayOptions),
}

impl Command {
    const KNOWN: &'static str =
        "`run`, `run --standby`, `migrate`, `bootstrap`, `setup`, or `replay`";
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
        Command::Run { standby } => {
            tracing::info!(environment = %config.environment, standby, "CrowdRelay worker started");
            run(database.clone(), &config, standby).await?;
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
            let mut standby = parse_standby_flag(&rest)?;
            if !standby && std::env::var("CROWDRELAY_WORKER_STANDBY").as_deref() == Ok("true") {
                standby = true;
            }
            Command::Run { standby }
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

fn parse_standby_flag(rest: &[String]) -> Result<bool> {
    let mut standby = false;
    for arg in rest {
        match arg.as_str() {
            "--standby" => standby = true,
            other => bail!("unexpected `run` argument `{other}`; expected `--standby`"),
        }
    }
    Ok(standby)
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

async fn run(database: PgPool, config: &Config, standby: bool) -> Result<()> {
    let secret_provider = load_secret_provider()?;
    timeout(
        config.database.operation_timeout,
        validate_active_endpoint_secrets(&database, &secret_provider),
    )
    .await
    .context("active webhook endpoint validation timed out")??;

    // Acquire single-active worker leadership before starting background loops.
    // In standby mode (blue-green deploy), polls until the old worker releases.
    // In normal mode, waits briefly then proceeds best-effort.
    let worker_id = format!("{}-{}", config.environment, uuid::Uuid::now_v7().simple());
    let (leadership_shutdown_tx, leadership_shutdown_rx) = watch::channel(false);
    let leadership =
        acquire_leadership(database.clone(), worker_id, standby, leadership_shutdown_rx)
            .await
            .context("failed to acquire worker leadership")?;
    tracing::info!(
        worker_id = leadership.worker_id(),
        generation = leadership.generation(),
        "worker leadership acquired, starting background loops"
    );
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
    let nearby_gig_scheduler = NearbyGigScheduler::new(
        database.clone(),
        workspace_id,
        NEARBY_GIG_POLL_INTERVAL,
        config.database.operation_timeout,
    );
    // A city a fan requested is a fan nobody can reach until it has
    // coordinates, and nothing filled them in. Without a contact address the
    // public geocoder is not usable, so say what stays unresolved rather than
    // starting a loop that will be blocked.
    let city_geocode_worker =
        CityGeocodeWorker::from_env(database.clone(), config.database.operation_timeout)?;
    let city_geocoding_enabled = city_geocode_worker.is_some();
    if !city_geocoding_enabled {
        tracing::warn!(
            "city geocoding is OFF: set CROWDRELAY_CITY_GEOCODING_CONTACT to an address the \
             geocoding provider can reach. Until then, fan-requested cities keep no coordinates \
             and the fans in them get no nearby-show announcements."
        );
    }
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
    // actions to Reddit via the agents service browser session.
    //
    // Two independent questions decide the mode, and they used to be one:
    // whether the executor *can* post (does it hold the agents service key)
    // and whether it *may* (has an operator asked for automatic posting).
    // Deriving "may" from "can" meant configuring a credential silently
    // started publishing under the band's own Reddit account — a decision
    // nobody made explicitly.
    //
    // Manual mode still drafts: it writes `community_posts` rows marked
    // `awaiting_manual_post`, and an operator publishes and registers the URL.
    // Metrics polling runs in both modes, so measurement does not depend on
    // who pressed the button — but it does depend on a working Reddit
    // credential, because Reddit now requires authentication to read a post's
    // score. An operator who publishes by hand and registers the URL still
    // gets no engagement numbers until that credential works.
    // Blank is absent, not present.
    //
    // Compose writes `CROWDRELAY_AGENT_SERVICE_AUTH_KEY: "${...:-}"`, so an
    // unset variable arrives as an empty string and `env::var(..).ok()` hands
    // back `Some("")`. Every consumer then believes it has a key, derives an
    // HMAC from nothing, and gets 401 from the agents service — which reads as
    // a wrong key rather than a missing one. That is exactly how the Reddit
    // adapter reached production authenticating with an empty secret.
    let agent_service_auth_key = std::env::var("CROWDRELAY_AGENT_SERVICE_AUTH_KEY")
        .ok()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty());
    // The community-intelligence Reddit adapter needs the same key, and the
    // community executor takes ownership of it below.
    let community_intel_agent_key = agent_service_auth_key.clone();
    let auto_post_requested = std::env::var("CROWDRELAY_COMMUNITY_AUTO_POST")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false);
    let has_agent_key = agent_service_auth_key.is_some();
    let manual_mode = !auto_post_requested || !has_agent_key;
    let community_executor = match CommunityExecutorWorker::new(
        database.clone(),
        workspace_id,
        config.database.operation_timeout,
        manual_mode,
        config.reddit_proxy_url.clone(),
        config.agent_service_url.clone(),
        agent_service_auth_key.clone(),
    ) {
        Ok(worker) => {
            // Say which of the two reasons put it in manual mode. "Not
            // posting" with no cause is the kind of message an operator reads
            // once and cannot act on.
            if manual_mode {
                // Read-only is checked first because it overrides the other
                // two: an operator who set the env var deserves to be told
                // it had no effect rather than left to infer it.
                let reason = if CommunityExecutorWorker::reddit_is_read_only() {
                    "reddit is read-only by policy — the login session the growth loop reads through is not risked on automated posting"
                } else if !auto_post_requested {
                    "CROWDRELAY_COMMUNITY_AUTO_POST is not enabled"
                } else {
                    "CROWDRELAY_AGENT_SERVICE_AUTH_KEY is missing"
                };
                tracing::info!(
                    auto_post_requested,
                    has_agent_key,
                    reason,
                    "community executor running in MANUAL MODE — posts are drafted and wait for an operator to publish them and register the URL"
                );
            } else {
                tracing::info!(
                    "community executor running in AUTOMATIC MODE — approved posts publish through the agents service browser session, bounded to one post per subreddit per 7 days and three per workspace per 24 hours"
                );
            }
            Some(worker)
        }
        Err(error) => {
            tracing::warn!(error = %error, "community executor disabled: HTTP client build failed");
            None
        }
    };
    // Telegram executor: posts LLM-drafted content to the band's Telegram
    // channel via the Bot API. The bot token is stored encrypted on the
    // telegram fanbase_connections row. In manual mode (default), posts are
    // drafted and marked `awaiting_manual_post` — the operator posts manually.
    // Enable with CROWDRELAY_TELEGRAM_AUTO_POST=true.
    let telegram_auto_post = std::env::var("CROWDRELAY_TELEGRAM_AUTO_POST")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false);
    let telegram_executor = match TelegramExecutorWorker::new(
        database.clone(),
        workspace_id,
        !telegram_auto_post,
        config.response_encryption_key.clone(),
    ) {
        Ok(worker) => {
            if !telegram_auto_post {
                tracing::info!(
                    "telegram executor running in MANUAL MODE — posts are drafted and wait for an operator to publish them manually"
                );
            } else {
                tracing::info!(
                    "telegram executor running in AUTOMATIC MODE — approved posts publish via the Bot API, bounded to one post per channel per 12 hours and five per workspace per 24 hours"
                );
            }
            Some(worker)
        }
        Err(error) => {
            tracing::warn!(error = %error, "telegram executor disabled: HTTP client build failed");
            None
        }
    };
    // Discord executor: posts LLM-drafted content to the band's Discord
    // channel via the Bot API. The bot token is stored encrypted on the
    // discord fanbase_connections row. In manual mode (default), posts are
    // drafted and marked `awaiting_manual_post` — the operator posts manually.
    // Enable with CROWDRELAY_DISCORD_AUTO_POST=true.
    let discord_auto_post = std::env::var("CROWDRELAY_DISCORD_AUTO_POST")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false);
    let discord_executor = match DiscordExecutorWorker::new(
        database.clone(),
        workspace_id,
        !discord_auto_post,
        config.response_encryption_key.clone(),
    ) {
        Ok(worker) => {
            if !discord_auto_post {
                tracing::info!(
                    "discord executor running in MANUAL MODE — posts are drafted and wait for an operator to publish them manually"
                );
            } else {
                tracing::info!(
                    "discord executor running in AUTOMATIC MODE — approved posts publish via the Bot API, bounded to one post per channel per 12 hours and five per workspace per 24 hours"
                );
            }
            Some(worker)
        }
        Err(error) => {
            tracing::warn!(error = %error, "discord executor disabled: HTTP client build failed");
            None
        }
    };
    // Social post executor: tracks LLM-drafted social posts for Instagram,
    // Facebook, and X/Twitter. Currently runs in manual mode — posts are
    // marked `awaiting_manual_post` and the operator publishes them
    // manually. Auto-posting via Meta Graph API / X API will be added in a
    // future phase. Env: CROWDRELAY_SOCIAL_AUTO_POST=true (enables auto mode
    // when platform API integration is available).
    let social_auto_post = std::env::var("CROWDRELAY_SOCIAL_AUTO_POST")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false);
    let social_post_executor =
        SocialPostExecutorWorker::new(database.clone(), workspace_id, !social_auto_post);
    if social_auto_post {
        tracing::info!(
            "social post executor running in AUTOMATIC MODE — not yet implemented for Instagram/Facebook/X, posts will fall back to awaiting_manual_post"
        );
    } else {
        tracing::info!(
            "social post executor running in MANUAL MODE — posts are drafted and wait for an operator to publish them manually"
        );
    }
    // Community join executor: auto-joins (subscribes to) Reddit communities
    // that the discovery worker has found. Calls the agents service's
    // /reddit/join endpoint which drives the logged-in browser session.
    // In manual mode (default), places stay `not_joined` — the operator
    // joins manually and records the result via the API.
    // Enable with CROWDRELAY_COMMUNITY_AUTO_JOIN=true.
    let community_auto_join = std::env::var("CROWDRELAY_COMMUNITY_AUTO_JOIN")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false);
    let community_join_executor = match CommunityJoinExecutorWorker::new(
        database.clone(),
        workspace_id,
        config.agent_service_url.clone(),
        agent_service_auth_key.clone(),
        community_auto_join,
    ) {
        Ok(worker) => {
            if community_auto_join {
                tracing::info!(
                    "community join executor running in AUTOMATIC MODE — subscribes to eligible subreddits via the agents service browser, bounded to 10 joins per 24 hours"
                );
            } else {
                tracing::info!(
                    "community join executor running in MANUAL MODE — places stay not_joined, operator joins manually"
                );
            }
            Some(worker)
        }
        Err(error) => {
            tracing::warn!(error = %error, "community join executor disabled: HTTP client build failed");
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
            discovery_config.clone(),
            DISCOVERY_SWEEP_INTERVAL,
            config.database.operation_timeout,
            config.agent_service_url.clone(),
            agent_service_auth_key.clone(),
        )?)
    } else {
        tracing::info!("reddit discovery disabled; CROWDRELAY_DISCOVERY_REDDIT_QUERIES not set");
        None
    };
    let x_discovery = if discovery_config.x_enabled() {
        Some(XDiscoveryWorker::new(
            database.clone(),
            workspace_id,
            discovery_config,
            DISCOVERY_SWEEP_INTERVAL,
            config.database.operation_timeout,
            config.agent_service_url.clone(),
            agent_service_auth_key.clone(),
        )?)
    } else {
        tracing::info!("x discovery disabled; CROWDRELAY_DISCOVERY_X_QUERIES not set");
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
    let facebook_page_access_token = std::env::var("CROWDRELAY_FACEBOOK_PAGE_ACCESS_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let tiktok_client_key = std::env::var("CROWDRELAY_TIKTOK_CLIENT_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let tiktok_client_secret = std::env::var("CROWDRELAY_TIKTOK_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let lastfm_api_key = std::env::var("CROWDRELAY_LASTFM_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let discogs_token = std::env::var("CROWDRELAY_DISCOGS_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let growth_metric_sync = GrowthMetricSyncWorker::new(
        database.clone(),
        youtube_api_key,
        facebook_page_access_token,
        config.agent_service_url.clone(),
        agent_service_auth_key.clone(),
        tiktok_client_key,
        tiktok_client_secret,
        lastfm_api_key,
        discogs_token,
        config.response_encryption_key.clone(),
        config.database.operation_timeout,
    )
    .context("invalid growth metric sync worker configuration")?;

    // Community Intelligence worker — observation layer for community surfaces.
    //
    // Adapters claim places by `platform = adapter.id()`. Registering only
    // Brutalland meant the 28 active Reddit places matched nothing: every
    // sweep found no work, recorded a success, and left
    // `community_observations` empty while the worker looked healthy.
    let community_intel_repo = Arc::new(PostgresCommunityIntelligenceRepository::new(
        database.clone(),
    ));
    let mut community_intel_adapters: Vec<Arc<dyn SourceAdapter>> =
        vec![Arc::new(BrutallandAdapter::new())];
    match RedditAdapter::new(
        config.agent_service_url.clone(),
        community_intel_agent_key,
        workspace_id.into_uuid(),
    ) {
        Some(adapter) => community_intel_adapters.push(Arc::new(adapter)),
        // Say why rather than starting a source that 401s on every sweep.
        None => tracing::warn!(
            "Reddit community observation disabled: \
             CROWDRELAY_AGENT_SERVICE_AUTH_KEY is not set; \
             active Reddit discovery places will not be observed"
        ),
    }
    let community_intel_worker = CommunityIntelligenceWorker::new(
        community_intel_adapters,
        community_intel_repo,
        database.clone(),
        workspace_id.into_uuid(),
    );

    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let reminder_shutdown = shutdown_receiver.clone();
    let nearby_gig_shutdown = shutdown_receiver.clone();
    let city_geocode_shutdown = shutdown_receiver.clone();
    let retention_shutdown = shutdown_receiver.clone();
    let event_sync_shutdown = shutdown_receiver.clone();
    let draw_shutdown = shutdown_receiver.clone();
    let autopilot_shutdown = shutdown_receiver.clone();
    let team_email_shutdown = shutdown_receiver.clone();
    let push_delivery_shutdown = shutdown_receiver.clone();
    let ops_watchdog_shutdown = shutdown_receiver.clone();
    let receipt_reconciliation_shutdown = shutdown_receiver.clone();
    let discovery_shutdown = shutdown_receiver.clone();
    let x_discovery_shutdown = shutdown_receiver.clone();
    let audience_graph_shutdown = shutdown_receiver.clone();
    let ad_conversion_shutdown = shutdown_receiver.clone();
    let agent_outcome_shutdown = shutdown_receiver.clone();
    let community_executor_shutdown = shutdown_receiver.clone();
    let executor_registrar_shutdown = shutdown_receiver.clone();
    let telegram_executor_shutdown = shutdown_receiver.clone();
    let discord_executor_shutdown = shutdown_receiver.clone();
    let social_post_executor_shutdown = shutdown_receiver.clone();
    let community_join_executor_shutdown = shutdown_receiver.clone();
    let growth_metric_sync_shutdown = shutdown_receiver.clone();
    let attribution_shutdown = shutdown_receiver.clone();
    let community_intel_shutdown = shutdown_receiver.clone();

    // Growth readiness summary: tells the operator exactly which growth
    // systems are active and what's missing. This is the single most
    // important log line for diagnosing "why isn't the system growing fans?"
    // Each component maps to a stage of the North Star loop:
    //   aggregate → grow → convert → learn
    let growth_readiness = GrowthReadiness {
        autopilot_enabled: autopilot_worker.is_some(),
        agent_outcomes_enabled: agent_outcome_worker.is_some(),
        push_delivery_enabled: push_delivery_worker.is_some(),
        nearby_shows_enabled: true,
        city_geocoding_enabled,
        community_executor_enabled: community_executor.is_some(),
        telegram_executor_enabled: telegram_executor.is_some(),
        discord_executor_enabled: discord_executor.is_some(),
        social_post_executor_enabled: true,
        community_join_executor_enabled: community_join_executor.is_some(),
        reddit_discovery_enabled: reddit_discovery.is_some(),
        x_discovery_enabled: x_discovery.is_some(),
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
        nearby_gig_scheduler.run(nearby_gig_shutdown).await;
        "nearby gig scheduler"
    });
    runtime_tasks.spawn(async move {
        match city_geocode_worker {
            Some(worker) => worker.run(city_geocode_shutdown).await,
            None => wait_for_shutdown(city_geocode_shutdown).await,
        }
        "city geocoding worker"
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
    if let Some(worker) = x_discovery {
        runtime_tasks.spawn(async move {
            worker.run(x_discovery_shutdown).await;
            "x discovery"
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
    // Advertise what this process executes in-process, so the action
    // dispatcher stops parking work the executors a few threads away are
    // sitting idle waiting for. Only capabilities whose executor actually
    // started are advertised: unparking an action nothing will claim is worse
    // than leaving it visibly parked.
    let mut in_process_capabilities: Vec<&'static str> = Vec::new();
    if community_executor.is_some() {
        in_process_capabilities.push("community.engage");
    }
    // The telegram, discord and social-post executors all claim
    // `agent.content.request` actions by direct SQL, so `agent.content` needs
    // an advertiser for the same reason `community.engage` did. Nothing
    // advertised it — n8n registers `content.artifact`, which is a different
    // capability — so those actions parked with `awaiting_executor` while
    // three executors polled for work the dispatcher would not release, and
    // the older ones were eventually cancelled with `no_executor`.
    //
    // One capability covers all three because they share the action kind and
    // split on the draft's `platform` field. The social-post executor is
    // always constructed, so the capability always has at least one claimant
    // and is advertised unconditionally.
    in_process_capabilities.push("agent.content");
    if let Some(registrar) = crowdrelay_worker::executor_registry::ExecutorRegistrar::new(
        database.clone(),
        workspace_id,
        in_process_capabilities.clone(),
    ) {
        tracing::info!(
            capabilities = ?in_process_capabilities,
            "advertising in-process executor capabilities"
        );
        runtime_tasks.spawn(async move {
            registrar.run(executor_registrar_shutdown).await;
            "executor registrar"
        });
    } else {
        tracing::info!(
            "no in-process executor capabilities to advertise; actions needing one stay parked"
        );
    }
    if let Some(worker) = community_executor {
        runtime_tasks.spawn(async move {
            worker.run(community_executor_shutdown).await;
            "community executor"
        });
    }
    if let Some(worker) = telegram_executor {
        runtime_tasks.spawn(async move {
            worker.run(telegram_executor_shutdown).await;
            "telegram executor"
        });
    }
    if let Some(worker) = discord_executor {
        runtime_tasks.spawn(async move {
            worker.run(discord_executor_shutdown).await;
            "discord executor"
        });
    }
    runtime_tasks.spawn(async move {
        social_post_executor
            .run(social_post_executor_shutdown)
            .await;
        "social post executor"
    });
    if let Some(worker) = community_join_executor {
        runtime_tasks.spawn(async move {
            worker.run(community_join_executor_shutdown).await;
            "community join executor"
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
    runtime_tasks.spawn(async move {
        community_intel_worker.run(community_intel_shutdown).await;
        "community intelligence worker"
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
    let _ = leadership_shutdown_tx.send(true);
    let shutdown_result = drain_worker_tasks(
        &mut runtime_tasks,
        config
            .database
            .operation_timeout
            .saturating_mul(2)
            .saturating_add(Duration::from_secs(2)),
    )
    .await;

    // Release worker leadership so a standby candidate can immediately acquire.
    leadership.release().await;

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
    use super::{Command, ReplayOptions, parse_command};

    #[test]
    fn defaults_to_run() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            parse_command(Vec::<String>::new())?,
            Command::Run { standby: false }
        );
        Ok(())
    }

    #[test]
    fn accepts_run_standby() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            parse_command(["run".to_owned(), "--standby".to_owned()])?,
            Command::Run { standby: true }
        );
        Ok(())
    }

    #[test]
    fn rejects_run_with_unknown_flag() {
        assert!(parse_command(["run".to_owned(), "--bogus".to_owned()]).is_err());
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
