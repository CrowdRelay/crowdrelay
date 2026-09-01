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

use std::{future::pending, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use crowdrelay_api::{
    AcquisitionState, AcquisitionStateArgs, AdmissionState, AdmissionStateArgs, AppState,
    ClickMetricsReader, ClickMetricsSnapshot, ClickSubmitter, ConcertQrState,
    EventActionMetricsReader, EventActionMetricsSnapshot, EventActionSubmitter, EventState,
    FanLifecycleState, HttpConfig, OpsState, PushPublicState, RateLimitPolicy, RateLimiter,
    ReferralState, TicketingState, tenant::TenantProfile,
};
use crowdrelay_application::{
    AcquisitionRepository, AdmissionRepository, ClaimAdmissionPass, ConfirmFan, EventCache,
    EventRepository, FanLifecycleRepository, IssueAdmissionPass, ListCities, ListFanEventInterests,
    LoadAdmissionPass, LoadEvents, LoadReferralProgress, LoadSmartLinks, RedeemAdmissionPass,
    RedeemCoupon, RedirectCache, ReferralRepository, RegisterEventInterest, ResolveReferralCode,
    RevokeAdmissionPass, SignupFan, UnsubscribeFan,
};
use crowdrelay_infra::{
    acquisition::{ClickBuffer, PostgresAcquisitionRepository},
    admission::PostgresAdmissionRepository,
    autopilot::PostgresAutopilotRepository,
    config::Config,
    database,
    events::{EventActionBuffer, PostgresEventRepository},
    fan_lifecycle::PostgresFanLifecycleRepository,
    observability,
    referrals::PostgresReferralRepository,
    sensitive_response::SensitiveResponseCodec,
    tenant_settings::TenantSettingsRepository,
};
use tokio::{
    net::TcpListener,
    signal,
    sync::watch,
    task::JoinSet,
    time::{MissedTickBehavior, interval, timeout},
};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env().context("invalid CrowdRelay configuration")?;
    observability::init("crowdrelay-api").context("failed to initialize structured tracing")?;
    observability::install_panic_hook("crowdrelay-api");

    let database = database::connect(&config.database)
        .await
        .context("failed to connect to PostgreSQL")?;
    let sensitive_response_codec = SensitiveResponseCodec::with_previous_key(
        config.response_encryption_key.clone(),
        config.previous_response_encryption_key.clone(),
    );
    let postgres_repository = Arc::new(PostgresAcquisitionRepository::new(
        database.clone(),
        config.workspace_slug.clone(),
        config.default_country_code.clone(),
        &config.database,
        config.require_double_opt_in,
        sensitive_response_codec.clone(),
    ));
    let mut tenant_profile = TenantProfile::from_process_env(&config.workspace_slug)
        .context("invalid tenant profile configuration")?;
    let workspace_id = postgres_repository
        .resolve_workspace(&config.workspace_slug)
        .await
        .context("failed to resolve configured workspace")?
        .ok_or_else(|| {
            anyhow!("configured workspace does not exist; run the worker bootstrap command first")
        })?;
    // Override product opt-ins from tenant_settings (database is authoritative
    // for multi-tenant deployments; env-var defaults preserve Virya's behavior).
    // A read failure is fatal rather than ignored: `signal_enabled` defaults to
    // true, so silently falling back would re-expose the beacon surface of a
    // tenant that had opted out.
    let settings_repo = TenantSettingsRepository::new(database.clone());
    let brand = settings_repo
        .brand_settings(workspace_id.into_uuid())
        .await
        .context("failed to load tenant product settings")?;
    tenant_profile.products.signal = brand.signal_enabled;
    tenant_profile.products.synesthesia = brand.synesthesia_enabled;
    let repository: Arc<dyn AcquisitionRepository> = postgres_repository;
    let referral_repository: Arc<dyn ReferralRepository> =
        Arc::new(PostgresReferralRepository::new(
            database.clone(),
            config.workspace_slug.clone(),
            &config.database,
        ));
    let event_repository: Arc<dyn EventRepository> = Arc::new(PostgresEventRepository::new(
        database.clone(),
        config.workspace_slug.clone(),
        &config.database,
        config.event_reminder_offsets_minutes.clone(),
    ));
    let admission_repository: Arc<dyn AdmissionRepository> =
        Arc::new(PostgresAdmissionRepository::new(
            database.clone(),
            config.workspace_slug.clone(),
            &config.database,
            config.admission_security.admin_member_email.clone(),
            config.admission_security.staff_member_email.clone(),
            config
                .admission_security
                .staff_api_key_sha256
                .unwrap_or([0_u8; 32]),
            sensitive_response_codec.clone(),
        ));
    let fan_lifecycle_repository: Arc<dyn FanLifecycleRepository> =
        Arc::new(PostgresFanLifecycleRepository::new(
            database.clone(),
            config.workspace_slug.clone(),
            &config.database,
            sensitive_response_codec,
        ));
    let redirect_cache = Arc::new(RedirectCache::new());
    let load_smart_links =
        LoadSmartLinks::new(Arc::clone(&repository), Arc::clone(&redirect_cache));
    let link_count = load_smart_links
        .execute()
        .await
        .context("failed to load the initial smart-link snapshot")?;
    tracing::info!(link_count, "initial smart-link snapshot loaded");

    let event_cache = Arc::new(EventCache::new());
    let load_events = LoadEvents::new(
        Arc::clone(&event_repository),
        Arc::clone(&event_cache),
        workspace_id,
    );
    let event_count = load_events
        .execute()
        .await
        .context("failed to load the initial event snapshot")?;
    tracing::info!(event_count, "initial event snapshot loaded");

    let (click_buffer, click_worker) =
        ClickBuffer::new(Arc::clone(&repository), config.click_buffer.clone())
            .context("invalid click buffer configuration")?;
    let click_metrics = click_buffer.metrics();
    let click_submitter: ClickSubmitter = Arc::new(move |event| {
        let _outcome = click_buffer.try_send(event);
    });
    let metrics_for_reader = Arc::clone(&click_metrics);
    let click_metrics_reader: ClickMetricsReader = Arc::new(move || {
        let snapshot = metrics_for_reader.snapshot();
        ClickMetricsSnapshot {
            queued: snapshot.queued,
            persisted: snapshot.persisted,
            dropped: snapshot.dropped,
            persistence_failed: snapshot.persistence_failed,
        }
    });
    let (event_action_buffer, event_action_worker) =
        EventActionBuffer::new(Arc::clone(&event_repository), config.click_buffer.clone())
            .context("invalid event action buffer configuration")?;
    let event_action_metrics = event_action_buffer.metrics();
    let event_action_submitter: EventActionSubmitter = Arc::new(move |action| {
        let _outcome = event_action_buffer.try_send(action);
    });
    let event_metrics_for_reader = Arc::clone(&event_action_metrics);
    let event_action_metrics_reader: EventActionMetricsReader = Arc::new(move || {
        let snapshot = event_metrics_for_reader.snapshot();
        EventActionMetricsSnapshot {
            queued: snapshot.queued,
            persisted: snapshot.persisted,
            dropped: snapshot.dropped,
            persistence_failed: snapshot.persistence_failed,
        }
    });

    let acquisition = AcquisitionState::new(AcquisitionStateArgs {
        workspace_id,
        redirect_cache,
        signup_fan: SignupFan::new(Arc::clone(&repository)),
        list_cities: ListCities::new(Arc::clone(&repository)),
        click_submitter,
        click_metrics_reader,
        public_site_base_url: config.public_site_base_url.clone(),
        secure_cookies: config.environment.is_production(),
        acquisition_repository: Arc::clone(&repository),
    });

    let referrals = ReferralState::new(
        workspace_id,
        ResolveReferralCode::new(Arc::clone(&referral_repository)),
        LoadReferralProgress::new(Arc::clone(&referral_repository)),
        RedeemCoupon::new(referral_repository),
        config.public_site_base_url.clone(),
        config.environment.is_production(),
    );
    let events = EventState::new(
        workspace_id,
        event_cache,
        RegisterEventInterest::new(Arc::clone(&event_repository)),
        ListFanEventInterests::new(event_repository),
        event_action_submitter,
        event_action_metrics_reader,
    );
    let admission = AdmissionState::new(AdmissionStateArgs {
        workspace_id,
        issue_pass: IssueAdmissionPass::new(Arc::clone(&admission_repository)),
        claim_pass: ClaimAdmissionPass::new(Arc::clone(&admission_repository)),
        load_pass: LoadAdmissionPass::new(Arc::clone(&admission_repository)),
        redeem_pass: RedeemAdmissionPass::new(Arc::clone(&admission_repository)),
        revoke_pass: RevokeAdmissionPass::new(admission_repository),
        qr_signing_key: config.admission_security.qr_signing_key,
        qr_ttl: config.admission_security.qr_ttl,
        secure_cookies: config.environment.is_production(),
    });
    let concert_qr = ConcertQrState::new(
        workspace_id,
        database.clone(),
        config.admission_security.qr_signing_key,
    );
    let ticketing = TicketingState::new(
        workspace_id,
        database.clone(),
        config.database.operation_timeout,
        config.admission_security.admin_api_key_sha256,
        config.admission_security.staff_api_key_sha256,
        config.admission_security.previous_admin_api_key_sha256,
        config.admission_security.previous_staff_api_key_sha256,
        config.commerce_api_key_sha256,
        config.previous_commerce_api_key_sha256,
        config.admission_security.qr_signing_key,
    );
    let fan_lifecycle = FanLifecycleState::new(
        workspace_id,
        ConfirmFan::new(Arc::clone(&fan_lifecycle_repository)),
        UnsubscribeFan::new(fan_lifecycle_repository),
        config.public_site_base_url.clone(),
        config.environment.is_production(),
    );
    let ops = OpsState::new(
        workspace_id,
        database.clone(),
        config.database.operation_timeout,
    );
    let autopilot = PostgresAutopilotRepository::new(database.clone(), &config.database);
    let rate_limiter = config.rate_limit.enabled.then(|| {
        Arc::new(RateLimiter::new(RateLimitPolicy {
            enabled: true,
            public_auth_per_minute: config.rate_limit.public_auth_per_minute,
            privileged_per_minute: config.rate_limit.privileged_per_minute,
            general_per_minute: config.rate_limit.general_per_minute,
        }))
    });
    let http_config = HttpConfig::new(config.allowed_origins.clone())
        .context("configured CORS origin is not a valid HTTP header value")?
        .with_rate_limit(rate_limiter);
    let app = crowdrelay_api::router(
        AppState::new(
            database.clone(),
            config.database.ping_timeout,
            acquisition,
            referrals,
            events,
            admission,
            concert_qr,
            fan_lifecycle,
            ticketing,
            config.control_plane_area_api_key_sha256,
            config.control_plane_api_key_sha256,
            config.previous_control_plane_area_api_key_sha256,
            config.previous_control_plane_api_key_sha256,
            ops,
            autopilot,
            config.autopilot_enabled,
            PushPublicState {
                runtime_enabled: config.push_delivery.runtime_enabled,
                web_push_vapid_public_key: config.push_delivery.web_push_vapid_public_key.clone(),
                fcm_project_id: config.push_delivery.fcm_project_id.clone(),
            },
            tenant_profile,
            config.response_encryption_key.clone(),
            crowdrelay_infra::provider_verification::ProviderVerifiers::new(
                config.youtube_api_key.clone(),
                config.facebook_page_access_token.clone(),
                config.reddit_proxy_url.clone(),
                reqwest::Client::new(),
            ),
        ),
        http_config,
    );
    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("failed to bind API listener to {}", config.bind_addr))?;
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let mut runtime_tasks = JoinSet::new();

    let refresh_interval = config.redirect_refresh_interval;
    let server_shutdown = shutdown_receiver.clone();
    runtime_tasks.spawn(async move {
        RuntimeTaskExit::Server(
            axum::serve(listener, app)
                .with_graceful_shutdown(wait_for_shutdown(server_shutdown))
                .await
                .context("API server failed"),
        )
    });
    let click_shutdown = shutdown_receiver.clone();
    runtime_tasks.spawn(async move {
        click_worker.run(click_shutdown).await;
        RuntimeTaskExit::Background("click ingestion")
    });
    let event_action_shutdown = shutdown_receiver.clone();
    runtime_tasks.spawn(async move {
        event_action_worker.run(event_action_shutdown).await;
        RuntimeTaskExit::Background("event action ingestion")
    });
    let smart_link_shutdown = shutdown_receiver.clone();
    runtime_tasks.spawn(async move {
        refresh_smart_links(load_smart_links, refresh_interval, smart_link_shutdown).await;
        RuntimeTaskExit::Background("smart-link refresh")
    });
    runtime_tasks.spawn(async move {
        refresh_events(load_events, refresh_interval, shutdown_receiver).await;
        RuntimeTaskExit::Background("event refresh")
    });

    tracing::info!(
        bind_addr = %config.bind_addr,
        environment = %config.environment,
        "CrowdRelay API started"
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let runtime_result = tokio::select! {
        () = &mut shutdown => {
            tracing::info!("shutdown requested");
            Ok(())
        }
        first_exit = runtime_tasks.join_next() => unexpected_runtime_exit(first_exit),
    };

    let _ = shutdown_sender.send(true);
    let shutdown_result = drain_runtime_tasks(
        &mut runtime_tasks,
        config
            .database
            .operation_timeout
            .saturating_add(Duration::from_secs(2)),
    )
    .await;
    let click_snapshot = click_metrics.snapshot();
    tracing::info!(
        click_queued = click_snapshot.queued,
        click_persisted = click_snapshot.persisted,
        click_dropped = click_snapshot.dropped,
        click_persistence_failed = click_snapshot.persistence_failed,
        "click ingestion stopped"
    );
    let event_snapshot = event_action_metrics.snapshot();
    tracing::info!(
        event_actions_queued = event_snapshot.queued,
        event_actions_persisted = event_snapshot.persisted,
        event_actions_dropped = event_snapshot.dropped,
        event_actions_persistence_failed = event_snapshot.persistence_failed,
        "event action ingestion stopped"
    );
    database.close().await;
    tracing::info!("CrowdRelay API stopped");

    runtime_result.and(shutdown_result)
}

#[derive(Debug)]
enum RuntimeTaskExit {
    Server(Result<()>),
    Background(&'static str),
}

fn unexpected_runtime_exit(
    exit: Option<std::result::Result<RuntimeTaskExit, tokio::task::JoinError>>,
) -> Result<()> {
    match exit {
        Some(Ok(RuntimeTaskExit::Server(Ok(())))) => {
            Err(anyhow!("API server stopped before shutdown was requested"))
        }
        Some(Ok(RuntimeTaskExit::Server(Err(error)))) => Err(error),
        Some(Ok(RuntimeTaskExit::Background(task_name))) => {
            Err(anyhow!("{task_name} stopped before shutdown was requested"))
        }
        Some(Err(error)) => Err(anyhow!("critical API runtime task failed: {error}")),
        None => Err(anyhow!(
            "all API runtime tasks stopped before shutdown was requested"
        )),
    }
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

async fn refresh_smart_links(
    loader: LoadSmartLinks,
    refresh_every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(refresh_every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            _ = ticker.tick() => {
                match loader.execute().await {
                    Ok(link_count) => {
                        tracing::debug!(link_count, "smart-link snapshot refreshed");
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "smart-link refresh failed; retaining previous snapshot"
                        );
                    }
                }
            }
        }
    }
}

async fn refresh_events(
    loader: LoadEvents,
    refresh_every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(refresh_every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
            }
            _ = ticker.tick() => {
                match loader.execute().await {
                    Ok(event_count) => tracing::debug!(event_count, "event snapshot refreshed"),
                    Err(error) => tracing::warn!(%error, "event refresh failed; retaining previous snapshot"),
                }
            }
        }
    }
}

async fn drain_runtime_tasks(
    runtime_tasks: &mut JoinSet<RuntimeTaskExit>,
    deadline: Duration,
) -> Result<()> {
    match timeout(deadline, drain_runtime_tasks_inner(runtime_tasks)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::error!("API runtime tasks exceeded graceful shutdown deadline");
            runtime_tasks.abort_all();
            while let Some(result) = runtime_tasks.join_next().await {
                match result {
                    Ok(exit) => log_runtime_task_exit(exit),
                    Err(error) if error.is_cancelled() => {
                        tracing::debug!("API runtime task cancellation completed");
                    }
                    Err(error) => {
                        tracing::error!(%error, "API runtime task failed while aborting");
                    }
                }
            }
            Err(anyhow!(
                "API runtime tasks exceeded graceful shutdown deadline"
            ))
        }
    }
}

async fn drain_runtime_tasks_inner(runtime_tasks: &mut JoinSet<RuntimeTaskExit>) -> Result<()> {
    let mut first_error: Option<anyhow::Error> = None;
    while let Some(result) = runtime_tasks.join_next().await {
        match result {
            Ok(RuntimeTaskExit::Server(Ok(()))) => {
                tracing::debug!("API server stopped cleanly");
            }
            Ok(RuntimeTaskExit::Server(Err(error))) => {
                tracing::error!(%error, "API server failed during shutdown");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Ok(RuntimeTaskExit::Background(task_name)) => {
                tracing::debug!(task_name, "background task stopped cleanly");
            }
            Err(error) => {
                tracing::error!(%error, "API runtime task failed during shutdown");
                if first_error.is_none() {
                    first_error = Some(anyhow!("API runtime task failed during shutdown: {error}"));
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn log_runtime_task_exit(exit: RuntimeTaskExit) {
    match exit {
        RuntimeTaskExit::Server(Ok(())) => {
            tracing::debug!("API server stopped after abort request");
        }
        RuntimeTaskExit::Server(Err(error)) => {
            tracing::error!(%error, "API server failed while aborting")
        }
        RuntimeTaskExit::Background(task_name) => {
            tracing::debug!(task_name, "background task stopped after abort request")
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
