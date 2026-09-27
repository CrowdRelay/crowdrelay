//! A letter whose opportunity retired leaves the approval queue.
//!
//! Dispatch already refuses a retired or expired opportunity, so these rows
//! could never send — but each one was a click the operator could only fail
//! on. On 2026-09-27 fifteen of the thirty-seven letters queued for one show
//! were addressed to contacts the show had stopped being news to.

use crate::common;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::sweep_lapsed_approval_asks;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_letter_for_a_retired_opportunity_is_withdrawn() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let now = OffsetDateTime::now_utc();
    let workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $2)")
        .bind(workspace_id)
        .bind(format!("retired-{}", workspace_id.simple()))
        .execute(&pool)
        .await?;

    let live = letter(
        &pool,
        workspace_id,
        "live",
        true,
        now + time::Duration::days(10),
        now,
    )
    .await?;
    let retired = letter(
        &pool,
        workspace_id,
        "retired",
        false,
        now + time::Duration::days(10),
        now,
    )
    .await?;
    // Active but past its window: the show is two days out and the pitch closed.
    let closed = letter(
        &pool,
        workspace_id,
        "closed",
        true,
        now - time::Duration::hours(1),
        now,
    )
    .await?;

    let mut transaction = pool.begin().await?;
    let stats = sweep_lapsed_approval_asks(
        &mut transaction,
        Some(WorkspaceId::from_uuid(workspace_id)),
        now,
        None,
    )
    .await?;
    transaction.commit().await?;
    assert_eq!(stats.opportunities_retired, 2);
    assert_eq!(stats.approvals_expired, 0);

    for (id, expected_status, expected_kind) in [
        (live, "awaiting_approval", None),
        (retired, "cancelled", Some("opportunity_retired")),
        (closed, "cancelled", Some("opportunity_retired")),
    ] {
        let (status, kind, key) = sqlx::query_as::<_, (String, Option<String>, String)>(
            "SELECT status, last_error_kind, idempotency_key FROM autopilot_actions
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(status, expected_status, "{key}");
        assert_eq!(kind.as_deref(), expected_kind, "{key}");
        // A withdrawn ask frees its key, so a re-published show may be
        // written for again.
        assert_eq!(key.contains(":retired:"), expected_kind.is_some(), "{key}");
    }

    // Re-running changes nothing.
    let mut transaction = pool.begin().await?;
    let again = sweep_lapsed_approval_asks(
        &mut transaction,
        Some(WorkspaceId::from_uuid(workspace_id)),
        now,
        None,
    )
    .await?;
    transaction.commit().await?;
    assert_eq!(again.opportunities_retired, 0);
    Ok(())
}

/// One contact, one opportunity in the given state, and a letter awaiting
/// approval against it. Returns the action id.
async fn letter(
    pool: &PgPool,
    workspace_id: Uuid,
    key: &str,
    active: bool,
    expires_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO outreach_targets
         (workspace_id, target_kind, display_name, contact_email, active, do_not_contact)
         VALUES ($1, 'press', $2, $2 || '@example.pl', true, false)
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(key)
    .fetch_one(pool)
    .await?;
    let opportunity_id: Uuid = sqlx::query_scalar(
        "INSERT INTO outreach_opportunities
         (workspace_id, target_id, source, subject_kind, subject_key, template_key,
          relevance_basis_points, confidence_basis_points, active, observed_at, expires_at)
         VALUES ($1, $2, 'event_autopilot', 'event', 'event:' || $3, 'event.press.v1',
                 8000, 8800, $4, $5, $6)
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(target_id)
    .bind(key)
    .bind(active)
    .bind(now - time::Duration::days(3))
    .bind(expires_at)
    .fetch_one(pool)
    .await?;

    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1, $2, $3, 'outreach', 'outreach_opportunity', $4,
                  'outreach.request', 8800, 'require_approval', 'a show in their country',
                  '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, $5, $6)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{key}"))
    .bind(opportunity_id)
    .bind(now - time::Duration::days(1))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approval_expires_at
        ) VALUES ($1, $2, $3, 'outreach', 'outreach.request', 'outreach_opportunity', $4,
                  $5, jsonb_build_object('kind', 'request_outreach',
                                         'opportunity_id', $4::text,
                                         'target_id', $6::text),
                  'awaiting_approval', $7)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(opportunity_id)
    .bind(format!("outreach:{key}"))
    .bind(target_id)
    .bind(now + time::Duration::days(2))
    .execute(pool)
    .await?;
    Ok(action_id)
}
