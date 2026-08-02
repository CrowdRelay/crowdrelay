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
    FanLifecycleState, HttpConfig, OpsState, ReferralState, TicketingState,
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
    config::Config,
    database,
    events::{EventActionBuffer, PostgresEventRepository},
    fan_lifecycle::PostgresFanLifecycleRepository,
    observability,
    referrals::PostgresReferralRepository,
    sensitive_response::SensitiveResponseCodec,
};
use tokio::{
    net::TcpListener,
    signal,
    sync::watch,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, timeout_at},
};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env().context("invalid CrowdRelay configuration")?;
    observability::init("crowdrelay-api").context("failed to initialize structured tracing")?;

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
    let workspace_id = postgres_repository
        .resolve_workspace(&config.workspace_slug)
        .await
        .context("failed to resolve configured workspace")?
        .ok_or_else(|| {
            anyhow!("configured workspace does not exist; run the worker bootstrap command first")
        })?;
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
        list_cities: ListCities::new(repository),
        click_submitter,
        click_metrics_reader,
        public_site_base_url: config.public_site_base_url.clone(),
        secure_cookies: config.environment.is_production(),
    });

    let referrals = ReferralState::new(
        workspace_id,
        ResolveReferralCode::new(Arc::clone(&referral_repository)),
        LoadReferralProgress::new(Arc::clone(&referral_repository)),
        RedeemCoupon::new(referral_repository),
        config.public_site_base_url.clone(),
        config.environment.is_production(),
        config.commerce_api_key_sha256,
        config.admission_security.staff_api_key_sha256,
        config.admission_security.admin_api_key_sha256,
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
        admin_api_key_sha256: config.admission_security.admin_api_key_sha256,
        staff_api_key_sha256: config.admission_security.staff_api_key_sha256,
        qr_signing_key: config.admission_security.qr_signing_key,
        qr_ttl: config.admission_security.qr_ttl,
        secure_cookies: config.environment.is_production(),
    });
    let concert_qr = ConcertQrState::new(
        workspace_id,
        database.clone(),
        config.admission_security.admin_api_key_sha256,
        config.admission_security.staff_api_key_sha256,
        config.admission_security.qr_signing_key,
    );
    let ticketing = TicketingState::new(
        workspace_id,
        database.clone(),
        config.database.operation_timeout,
        config.database.lock_timeout,
        config.admission_security.admin_api_key_sha256,
        config.admission_security.staff_api_key_sha256,
        config.commerce_api_key_sha256,
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
    let http_config = HttpConfig::new(config.allowed_origins.clone())
        .context("configured CORS origin is not a valid HTTP header value")?;
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
            ops,
        ),
        http_config,
    );
    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("failed to bind API listener to {}", config.bind_addr))?;
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let click_task = tokio::spawn(click_worker.run(shutdown_receiver.clone()));
    let event_action_task = tokio::spawn(event_action_worker.run(shutdown_receiver.clone()));
    let refresh_task = tokio::spawn(refresh_smart_links(
        load_smart_links,
        config.redirect_refresh_interval,
        shutdown_receiver.clone(),
    ));
    let event_refresh_task = tokio::spawn(refresh_events(
        load_events,
        config.redirect_refresh_interval,
        shutdown_receiver,
    ));

    tracing::info!(
        bind_addr = %config.bind_addr,
        environment = %config.environment,
        "CrowdRelay API started"
    );

    let signal_sender = shutdown_sender.clone();
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = signal_sender.send(true);
        })
        .await
        .context("API server failed");

    let _ = shutdown_sender.send(true);
    await_background_tasks(
        click_task,
        event_action_task,
        refresh_task,
        event_refresh_task,
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

    server_result
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

async fn await_background_tasks(
    click_task: JoinHandle<()>,
    event_action_task: JoinHandle<()>,
    refresh_task: JoinHandle<()>,
    event_refresh_task: JoinHandle<()>,
    deadline: Duration,
) {
    let shutdown_deadline = match Instant::now().checked_add(deadline) {
        Some(deadline) => deadline,
        None => {
            tracing::error!("graceful shutdown deadline overflowed; aborting background tasks");
            Instant::now()
        }
    };
    await_background_task(click_task, "click ingestion", shutdown_deadline).await;
    await_background_task(
        event_action_task,
        "event action ingestion",
        shutdown_deadline,
    )
    .await;
    await_background_task(refresh_task, "smart-link refresh", shutdown_deadline).await;
    await_background_task(event_refresh_task, "event refresh", shutdown_deadline).await;
}

async fn await_background_task(
    mut task: JoinHandle<()>,
    task_name: &'static str,
    shutdown_deadline: Instant,
) {
    match timeout_at(shutdown_deadline, &mut task).await {
        Ok(Ok(())) => tracing::debug!(task_name, "background task stopped cleanly"),
        Ok(Err(error)) => tracing::error!(task_name, %error, "background task failed"),
        Err(_) => {
            tracing::error!(
                task_name,
                "background task exceeded graceful shutdown deadline"
            );
            task.abort();
            match task.await {
                Ok(()) => tracing::debug!(task_name, "background task stopped after abort request"),
                Err(error) if error.is_cancelled() => {
                    tracing::debug!(task_name, "background task cancellation completed");
                }
                Err(error) => {
                    tracing::error!(task_name, %error, "background task failed while aborting");
                }
            }
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

    tracing::info!("shutdown requested");
}
