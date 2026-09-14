//! A scan at the door must yield a reachable fan, not a dead end.
//!
//! Until now `POST /v1/events/{slug}/check-in` required a live fan-session
//! cookie, which only exists after a signup — so the stranger who scans the
//! room QR, the exact person the whole ritual exists for, got a 401. The
//! endpoint now accepts an email claim in place of the cookie: the attendance
//! fact is recorded with `identity_source='email_claim'`, the fan lands
//! `pending` regardless of workspace double-opt-in policy (an unverified
//! address must not be reachable), and the follow-up is the same
//! `fan.confirmation_requested` / `fan.session_requested` outbox pair the
//! signup flow already emits — never a session, because an unverified email
//! must not authenticate as the fan behind it.

use crowdrelay_application::{
    CheckinCommand, CheckinConsent, CheckinIdentity, ConcertQrRepository,
    UpdateCampaignContextCommand,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::concert_qr::PostgresConcertQrRepository;
use sqlx::{PgPool, postgres::PgPoolOptions};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn pool() -> Result<PgPool> {
    let url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok(pool)
}

async fn workspace(pool: &PgPool, label: &str) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("{label}-{}", id.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(WorkspaceId::from_uuid(id))
}

struct Fixture {
    event_id: Uuid,
    campaign_id: Uuid,
    expires_at: i64,
}

async fn show(pool: &PgPool, workspace_id: WorkspaceId, label: &str) -> Result<Fixture> {
    let ws = workspace_id.into_uuid();
    let city_id = Uuid::now_v7();
    sqlx::query("INSERT INTO cities (id, slug, name, country_code) VALUES ($1, $2, $3, 'PL')")
        .bind(city_id)
        .bind(format!("{label}-{}", city_id.simple()))
        .bind(label)
        .execute(pool)
        .await?;

    let event_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO events (id, workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
        VALUES ($1, $2, $3, $4, 'The Show', 'Klub X', now() + interval '6 hours', 'published', now())
        "#,
    )
    .bind(event_id)
    .bind(ws)
    .bind(city_id)
    .bind(format!("{label}-{}", event_id.simple()))
    .execute(pool)
    .await?;

    let campaign_id = Uuid::now_v7();
    let valid_until = OffsetDateTime::now_utc() + Duration::hours(30);
    sqlx::query(
        r#"
        INSERT INTO concert_qr_campaigns (id, workspace_id, event_id, label, valid_from, valid_until)
        VALUES ($1, $2, $3, 'door', now() - interval '1 hour', $4)
        "#,
    )
    .bind(campaign_id)
    .bind(ws)
    .bind(event_id)
    .bind(valid_until)
    .execute(pool)
    .await?;

    Ok(Fixture {
        event_id,
        campaign_id,
        expires_at: valid_until.unix_timestamp(),
    })
}

fn command(
    fixture: &Fixture,
    workspace_id: WorkspaceId,
    session_token: Option<String>,
    email: Option<String>,
    consent: Option<CheckinConsent>,
) -> CheckinCommand {
    CheckinCommand {
        workspace_id: workspace_id.into_uuid(),
        event_slug: "unused".to_owned(),
        campaign_id: fixture.campaign_id,
        event_id: fixture.event_id,
        expires_at: fixture.expires_at,
        session_token,
        email,
        consent,
        now: OffsetDateTime::now_utc(),
        request_id: Some(format!("req-{}", Uuid::now_v7())),
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn email_claim_creates_pending_fan_checkin_and_confirmation() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "scan-claim").await?;
    let fixture = show(&pool, ws, "scan-claim").await?;
    let repo = PostgresConcertQrRepository::new(pool.clone());
    let slug: String =
        sqlx::query_scalar("SELECT slug FROM events WHERE workspace_id = $1 AND id = $2")
            .bind(ws.into_uuid())
            .bind(fixture.event_id)
            .fetch_one(&pool)
            .await?;

    let mut cmd = command(
        &fixture,
        ws,
        None,
        Some("stranger@example.com".to_owned()),
        Some(CheckinConsent {
            granted: true,
            policy_version: "v1".to_owned(),
        }),
    );
    cmd.event_slug = slug;
    let result = repo.check_in(&cmd).await?;
    assert!(result.created);
    assert_eq!(result.identity, CheckinIdentity::EmailClaim);

    let (status, identity_source): (String, String) = sqlx::query_as(
        r#"
        SELECT f.status, c.identity_source
        FROM concert_checkins c JOIN fans f ON f.id = c.fan_id AND f.workspace_id = c.workspace_id
        WHERE c.workspace_id = $1 AND c.event_id = $2
        "#,
    )
    .bind(ws.into_uuid())
    .bind(fixture.event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, "pending");
    assert_eq!(identity_source, "email_claim");

    let consent_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM fan_consents WHERE workspace_id = $1 AND purpose = 'marketing' AND granted AND source = 'concert_checkin'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(consent_count, 1);

    let outbox: Option<(String,)> = sqlx::query_as(
        "SELECT event_type FROM outbox_events WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(ws.into_uuid())
    .fetch_optional(&pool)
    .await?;
    assert!(
        outbox.is_some(),
        "pending fan must get a confirmation email"
    );

    // The welcome doubles as the T+1 recall, so it is scheduled for the next
    // morning rather than sent into the night of the scan.
    let delayed: bool = sqlx::query_scalar(
        "SELECT available_at > now() + interval '1 hour' FROM outbox_events WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(delayed, "the scan welcome must wait for the next morning");

    let city_interest: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM fan_city_interests WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(city_interest, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn email_claim_on_active_fan_sends_session_not_confirmation() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "scan-active").await?;
    let fixture = show(&pool, ws, "scan-active").await?;
    let repo = PostgresConcertQrRepository::new(pool.clone());

    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, 'regular@example.com', 'active')",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    let slug: String =
        sqlx::query_scalar("SELECT slug FROM events WHERE workspace_id = $1 AND id = $2")
            .bind(ws.into_uuid())
            .bind(fixture.event_id)
            .fetch_one(&pool)
            .await?;

    let mut cmd = command(
        &fixture,
        ws,
        None,
        Some("regular@example.com".to_owned()),
        None,
    );
    cmd.event_slug = slug;
    let result = repo.check_in(&cmd).await?;
    assert!(result.created);

    let session_outbox: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM outbox_events WHERE workspace_id = $1 AND event_type = 'fan.session_requested'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    let confirm_outbox: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM outbox_events WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(session_outbox, 1);
    assert_eq!(confirm_outbox, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn email_claim_rescan_dedupes_and_never_resends() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "scan-twice").await?;
    let fixture = show(&pool, ws, "scan-twice").await?;
    let repo = PostgresConcertQrRepository::new(pool.clone());
    let slug: String =
        sqlx::query_scalar("SELECT slug FROM events WHERE workspace_id = $1 AND id = $2")
            .bind(ws.into_uuid())
            .bind(fixture.event_id)
            .fetch_one(&pool)
            .await?;

    for expected_created in [true, false] {
        let mut cmd = command(
            &fixture,
            ws,
            None,
            Some("twice@example.com".to_owned()),
            None,
        );
        cmd.event_slug.clone_from(&slug);
        let result = repo.check_in(&cmd).await?;
        assert_eq!(result.created, expected_created);
    }

    let checkins: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM concert_checkins WHERE workspace_id = $1 AND event_id = $2",
    )
    .bind(ws.into_uuid())
    .bind(fixture.event_id)
    .fetch_one(&pool)
    .await?;
    let confirmations: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM outbox_events WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(checkins, 1);
    assert_eq!(confirmations, 1, "a rescan must not resend the follow-up");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn campaign_context_updates_in_place_and_refuses_revoked() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "scan-ctx").await?;
    let fixture = show(&pool, ws, "scan-ctx").await?;
    let repo = PostgresConcertQrRepository::new(pool.clone());

    let command = UpdateCampaignContextCommand {
        workspace_id: ws.into_uuid(),
        campaign_id: fixture.campaign_id,
        placement: Some("merch table".to_owned()),
        announced_from_stage: true,
        incentive: Some("setlist pdf".to_owned()),
        request_id: Some(format!("req-{}", Uuid::now_v7())),
    };
    repo.update_campaign_context(&command).await?;

    let (placement, announced, incentive): (Option<String>, bool, Option<String>) =
        sqlx::query_as(
            "SELECT placement, announced_from_stage, incentive FROM concert_qr_campaigns WHERE workspace_id = $1 AND id = $2",
        )
        .bind(ws.into_uuid())
        .bind(fixture.campaign_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(placement.as_deref(), Some("merch table"));
    assert!(announced);
    assert_eq!(incentive.as_deref(), Some("setlist pdf"));

    sqlx::query("UPDATE concert_qr_campaigns SET active = false, revoked_at = now() WHERE workspace_id = $1 AND id = $2")
        .bind(ws.into_uuid())
        .bind(fixture.campaign_id)
        .execute(&pool)
        .await?;
    let err = repo.update_campaign_context(&command).await.unwrap_err();
    assert_eq!(
        err,
        crowdrelay_application::ConcertQrError::NotFound,
        "a revoked campaign must refuse context writes"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn email_claim_never_mints_a_session() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "scan-nosession").await?;
    let fixture = show(&pool, ws, "scan-nosession").await?;
    let repo = PostgresConcertQrRepository::new(pool.clone());
    let slug: String =
        sqlx::query_scalar("SELECT slug FROM events WHERE workspace_id = $1 AND id = $2")
            .bind(ws.into_uuid())
            .bind(fixture.event_id)
            .fetch_one(&pool)
            .await?;

    let mut cmd = command(
        &fixture,
        ws,
        None,
        Some("noauth@example.com".to_owned()),
        None,
    );
    cmd.event_slug = slug;
    repo.check_in(&cmd).await?;

    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM fan_sessions WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(sessions, 0, "an unverified email must never authenticate");
    Ok(())
}
