use std::time::Duration;

use crowdrelay_application::{
    AdmissionRepository, ClaimAdmissionPassCommand, IdempotencyKey, IssueAdmissionPassCommand,
    RedeemAdmissionPassCommand, RepositoryError, RequestId, RevokeAdmissionPassCommand,
};
use crowdrelay_domain::{
    AdmissionPassStatus, AdmissionRedemptionStatus, EventSlug, NormalizedEmail, WorkspaceId,
    WorkspaceSlug,
};
use crowdrelay_infra::{
    admission::PostgresAdmissionRepository,
    config::DatabaseConfig,
    sensitive_response::{SensitiveResponseCodec, SensitiveResponseKey},
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_ADMISSION_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn limited_pass_claims_and_redeems_exactly_once() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_ADMISSION_TEST_DATABASE_URL")
        .map_err(|e| format!("CROWDRELAY_ADMISSION_TEST_DATABASE_URL must be configured: {e}"))?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    let workspace_slug = WorkspaceSlug::parse(format!("admission-{suffix}"))?;
    // workspace_member_sessions.session_token_hash is globally unique, so a
    // fixed key made this suite pass once and then fail on 23505 against any
    // reused database. Derive it from the same per-run suffix as the slug.
    let staff_key_hash: [u8; 32] = Sha256::digest(format!("integration-staff-key-{suffix}")).into();
    seed_fixture(&pool, workspace_id, &workspace_slug, staff_key_hash).await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAdmissionRepository::new(
        pool.clone(),
        workspace_slug,
        &database,
        "admin@example.test".to_owned(),
        "gate@example.test".to_owned(),
        staff_key_hash,
        SensitiveResponseCodec::new(SensitiveResponseKey::derive_from_secret(
            b"admission-integration-response-secret",
        )),
    );

    let issue = IssueAdmissionPassCommand {
        workspace_id,
        event_slug: EventSlug::parse("test-show")?,
        pool_slug: EventSlug::parse("guest-list")?,
        fan_email: NormalizedEmail::parse("winner@example.test")?,
        claim_expires_hours: 24,
        idempotency_key: IdempotencyKey::parse("issue-pass-0001")?,
        request_id: RequestId::parse("request-issue-pass-0001")?,
    };
    let issued = repository.issue_pass(&issue).await?;
    sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET response_body = $3, response_content_type = 'application/json'
        WHERE workspace_id = $1 AND scope = 'admission.pass.issue' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(issue.idempotency_key.as_str())
    .bind(serde_json::to_value(&issued)?)
    .execute(&pool)
    .await?;
    let retry_issue = IssueAdmissionPassCommand {
        request_id: RequestId::parse("request-issue-pass-0001-retry")?,
        ..issue.clone()
    };
    let replay = repository.issue_pass(&retry_issue).await?;
    assert_eq!(issued, replay);
    let (idempotency_body, idempotency_content_type) = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT response_body::text, response_content_type
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = 'admission.pass.issue' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(issue.idempotency_key.as_str())
    .fetch_one(&pool)
    .await?;
    assert!(!idempotency_body.contains(issued.claim_token.as_str()));
    assert!(idempotency_body.contains("\"alg\": \"XChaCha20-Poly1305\""));
    assert_eq!(
        idempotency_content_type,
        "application/vnd.crowdrelay.encrypted+json"
    );

    let claim = ClaimAdmissionPassCommand {
        workspace_id,
        token: issued.claim_token.clone(),
        idempotency_key: IdempotencyKey::parse("claim-pass-0001")?,
        request_id: RequestId::parse("request-claim-pass-0001")?,
    };
    let claimed = repository.claim_pass(&claim).await?;
    let replayed_claim = repository
        .claim_pass(&ClaimAdmissionPassCommand {
            request_id: RequestId::parse("request-claim-pass-0001-retry")?,
            ..claim.clone()
        })
        .await?;
    assert_eq!(claimed, replayed_claim);
    let claim_idempotency_body = sqlx::query_scalar::<_, String>(
        r#"
        SELECT response_body::text
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = 'admission.pass.claim' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(claim.idempotency_key.as_str())
    .fetch_one(&pool)
    .await?;
    assert!(!claim_idempotency_body.contains(claimed.session_token.as_str()));
    assert!(claim_idempotency_body.contains("\"alg\": \"XChaCha20-Poly1305\""));
    assert!(
        claimed.pass.session_expires_at > claimed.pass.starts_at,
        "a winner session must remain valid through the event, even when the event is more than the fixed fallback TTL away"
    );
    assert_eq!(claimed.pass.status, AdmissionPassStatus::Claimed);
    let loaded = repository
        .load_pass(workspace_id, &claimed.session_token)
        .await?;
    assert_eq!(loaded.pass_id, issued.pass_id);
    let wrong_event = redeem_command_for_event(
        workspace_id,
        &issued.public_reference,
        "other-show",
        "redeem-wrong-event",
    )?;
    assert_eq!(
        repository.redeem_pass(&wrong_event).await,
        Err(RepositoryError::NotFound),
        "gate staff must bind every QR or manual lookup to the active event"
    );
    sqlx::query(
        "UPDATE events SET starts_at = now(), ends_at = now() + interval '4 hours' \
         WHERE workspace_id = $1 AND slug = 'test-show'",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let first_repository = repository.clone();
    let second_repository = repository.clone();
    let first = redeem_command(workspace_id, &issued.public_reference, "redeem-pass-0001")?;
    let second = redeem_command(workspace_id, &issued.public_reference, "redeem-pass-0002")?;
    let (first, second) = tokio::join!(
        first_repository.redeem_pass(&first),
        second_repository.redeem_pass(&second)
    );
    let statuses = [first?.status, second?.status];
    assert!(statuses.contains(&AdmissionRedemptionStatus::Redeemed));
    assert!(statuses.contains(&AdmissionRedemptionStatus::AlreadyRedeemed));

    let redemption_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM pass_redemptions WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(redemption_count, 1);

    sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET created_at = now() - interval '3 days',
            completed_at = now() - interval '2 days',
            expires_at = now() - interval '1 day'
        WHERE workspace_id = $1 AND scope = 'admission.pass.issue' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(issue.idempotency_key.as_str())
    .execute(&pool)
    .await?;
    let second_issue = IssueAdmissionPassCommand {
        fan_email: NormalizedEmail::parse("second-winner@example.test")?,
        idempotency_key: issue.idempotency_key.clone(),
        request_id: RequestId::parse("request-issue-pass-0002")?,
        ..issue
    };
    let second_pass = repository.issue_pass(&second_issue).await?;
    assert_ne!(second_pass.pass_id, issued.pass_id);
    let first_revoke = repository
        .revoke_pass(&revoke_command(
            workspace_id,
            &second_pass.public_reference,
            "revoke-pass-0001",
        )?)
        .await?;
    assert_eq!(first_revoke.status, AdmissionPassStatus::Revoked);
    let second_revoke = repository
        .revoke_pass(&revoke_command(
            workspace_id,
            &second_pass.public_reference,
            "revoke-pass-0002",
        )?)
        .await?;
    assert_eq!(second_revoke.status, AdmissionPassStatus::Revoked);

    let revocation_events = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM outbox_events WHERE workspace_id = $1 AND event_type = 'admission.pass.revoked'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(revocation_events, 1);
    let issued_count = sqlx::query_scalar::<_, i32>(
        "SELECT issued_count FROM admission_pools WHERE workspace_id = $1 AND slug = 'guest-list'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(issued_count, 1);

    pool.close().await;
    Ok(())
}

fn revoke_command(
    workspace_id: WorkspaceId,
    public_reference: &str,
    key: &str,
) -> Result<RevokeAdmissionPassCommand, Box<dyn std::error::Error>> {
    Ok(RevokeAdmissionPassCommand {
        workspace_id,
        public_reference: public_reference.to_owned(),
        idempotency_key: IdempotencyKey::parse(key)?,
        request_id: RequestId::parse(format!("request-{key}"))?,
    })
}

fn redeem_command(
    workspace_id: WorkspaceId,
    public_reference: &str,
    key: &str,
) -> Result<RedeemAdmissionPassCommand, Box<dyn std::error::Error>> {
    redeem_command_for_event(workspace_id, public_reference, "test-show", key)
}

fn redeem_command_for_event(
    workspace_id: WorkspaceId,
    public_reference: &str,
    event_slug: &str,
    key: &str,
) -> Result<RedeemAdmissionPassCommand, Box<dyn std::error::Error>> {
    Ok(RedeemAdmissionPassCommand {
        workspace_id,
        event_slug: EventSlug::parse(event_slug)?,
        pass_id: None,
        event_id: None,
        public_reference: public_reference.to_owned(),
        idempotency_key: IdempotencyKey::parse(key)?,
        request_id: RequestId::parse(format!("request-{key}"))?,
    })
}

async fn seed_fixture(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    slug: &WorkspaceSlug,
    staff_key_hash: [u8; 32],
) -> Result<(), Box<dyn std::error::Error>> {
    let event_id = Uuid::now_v7();
    let fan_id = Uuid::now_v7();
    let second_fan_id = Uuid::now_v7();
    let admin_id = Uuid::now_v7();
    let staff_id = Uuid::now_v7();
    let staff_session_id = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Admission E2E')")
        .bind(workspace_id.into_uuid())
        .bind(slug.as_str())
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO fans (id, workspace_id, normalized_email, display_name, status)
        VALUES
            ($1, $3, 'winner@example.test', 'Winner', 'active'),
            ($2, $3, 'second-winner@example.test', 'Second winner', 'active')
        "#,
    )
    .bind(fan_id)
    .bind(second_fan_id)
    .bind(workspace_id.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO events \
         (id, workspace_id, slug, title, starts_at, status, published_at) \
         VALUES
         ($1, $2, 'test-show', 'Test show', now() + interval '60 days', 'published', now()),
         ($3, $2, 'other-show', 'Other show', now() + interval '61 days', 'published', now())",
    )
    .bind(event_id)
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO admission_pools (workspace_id, event_id, slug, name, capacity, active) VALUES ($1, $2, 'guest-list', 'Guest list', 2, true)",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO workspace_members (id, workspace_id, normalized_email, role) VALUES ($1, $3, 'admin@example.test', 'admin'), ($2, $3, 'gate@example.test', 'staff')",
    )
    .bind(admin_id)
    .bind(staff_id)
    .bind(workspace_id.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO workspace_member_sessions (
            id, workspace_id, member_id, session_token_hash, csrf_token_hash, expires_at
        ) VALUES ($1, $2, $3, $4, $5, now() + interval '365 days')
        "#,
    )
    .bind(staff_session_id)
    .bind(workspace_id.into_uuid())
    .bind(staff_id)
    .bind(staff_key_hash.as_slice())
    .bind(vec![9_u8; 32])
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}
