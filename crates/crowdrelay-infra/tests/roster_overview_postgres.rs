//! The roster view (5.5), against a real schema.
//!
//! Worth a database for the same reason the sibling suites are: the org
//! boundary is a join, the governor/touch/consent predicates are the
//! production ones — and none of it is checked at compile time. A column
//! that does not exist would compile, lint and pass every unit test, then
//! fail on the first request.
//!
//! What is asserted: each act's state lands under the right act and never
//! under a labelmate or an outsider; the spend, cooldown, and closed-door
//! counts come from the real governor and touch ledgers; the reachable-fan
//! count honours the latest-grant discipline (a revoked consent does not
//! count); and the page leads with the act missing the most.

mod common;

use crowdrelay_domain::roster_overview::{
    GAP_NO_BRIEFING, GAP_NO_REACHABLE_FANS, GAP_NO_UPCOMING_SHOW,
};
use crowdrelay_infra::roster_overview::roster_overview;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn organization(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn workspace(
    pool: &PgPool,
    slug: &str,
    organization_id: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(id)
}

/// The decision + action pair a touch row's foreign key needs.
async fn dispatched_action(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'booking_opportunity','content_suggestion',$4,
                  'outreach.target.request',7000,'require_approval',
                  'test decision','{}','{}','{}',now(),$1)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{decision_id}"))
    .bind(subject_id)
    .execute(pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, available_at, finished_at
        ) VALUES ($1,$2,$3,'booking_opportunity','outreach.target.request','content_suggestion',$4,
                  $5,$6,'succeeded',now(),now())
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(subject_id)
    .bind(format!("action-{action_id}"))
    .bind(json!({"kind": "outreach.target.request"}))
    .execute(pool)
    .await?;
    Ok(action_id)
}

/// One spent touch on the shared budget — the row the reservation writes.
async fn touch(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
    contact: &str,
    touched_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO contact_touches (workspace_id, normalized_contact, action_id, touched_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(workspace_id)
    .bind(contact)
    .bind(action_id)
    .bind(touched_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// A governor row — `on_hold` sets the cooldown ahead of now, `blocked` sets
/// the door closed; `last_action_id` stays null, the schema allows it.
async fn governor(
    pool: &PgPool,
    workspace_id: Uuid,
    contact: &str,
    on_hold: bool,
    blocked: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO contact_governor (
            workspace_id, normalized_contact, last_context, last_outbound_at,
            next_contact_after, do_not_contact
        ) VALUES ($1, $2, 'booking_outreach', now() - INTERVAL '10 days', $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(contact)
    .bind(if on_hold {
        OffsetDateTime::now_utc() + time::Duration::days(5)
    } else {
        OffsetDateTime::now_utc() - time::Duration::days(1)
    })
    .bind(blocked)
    .execute(pool)
    .await?;
    Ok(())
}

/// An active fan with a marketing consent row — granted or revoked.
async fn fan(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    granted: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace_id)
    .bind(email)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1, $2, 'marketing', $3, 'privacy-2026-07', 'integration_test')",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(granted)
    .execute(pool)
    .await?;
    Ok(())
}

/// A published show `days` out — the pipeline's calendar half.
async fn upcoming_show(
    pool: &PgPool,
    workspace_id: Uuid,
    slug: &str,
    days: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, $3, $3, now() + ($4::int * INTERVAL '1 day'), 'published', now())",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(slug)
    .bind(days)
    .execute(pool)
    .await?;
    Ok(())
}

/// Two member acts — one fully provisioned, one entirely empty — plus an
/// outsider holding every resource, and the assertions the screen depends
/// on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn every_acts_attention_pipeline_and_gaps_land_under_that_act()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let pool = &database;
    let now = OffsetDateTime::now_utc();
    let today = now.date();

    async {
        let label = organization(pool, "roster-label").await?;
        let busy = workspace(pool, "busy-act", Some(label)).await?;
        let empty = workspace(pool, "empty-act", Some(label)).await?;
        let outsider = workspace(pool, "outsider", None).await?;

        // Busy: two touches spent, one fan cooling down, one door closed,
        // one decision waiting, two shows coming, a briefing, two reachable
        // fans (a third revoked — the latest grant is what counts).
        let busy_action = dispatched_action(pool, busy).await?;
        touch(pool, busy, busy_action, "promoter@example.test", now).await?;
        let busy_action_two = dispatched_action(pool, busy).await?;
        touch(
            pool,
            busy,
            busy_action_two,
            "promoter@example.test",
            now - time::Duration::days(10),
        )
        .await?;
        // A touch older than the window does not count.
        let busy_action_three = dispatched_action(pool, busy).await?;
        touch(
            pool,
            busy,
            busy_action_three,
            "old@example.test",
            now - time::Duration::days(45),
        )
        .await?;
        governor(pool, busy, "cooling@example.test", true, false).await?;
        governor(pool, busy, "closed@example.test", false, true).await?;
        governor(pool, busy, "open@example.test", false, false).await?;
        let _ = dispatched_action(pool, busy).await?;
        // A live approval: pending pipeline depth.
        let decision_id = Uuid::now_v7();
        let subject_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
            ) VALUES ($1,$2,$3,'booking_opportunity','content_suggestion',$4,
                      'outreach.target.request',7000,'require_approval',
                      'test decision','{}','{}','{}',now(),$1)
            "#,
        )
        .bind(decision_id)
        .bind(busy)
        .bind(format!("decision-{decision_id}"))
        .bind(subject_id)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO autopilot_actions (
                id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
                idempotency_key, payload, status, available_at, approval_expires_at
            ) VALUES ($1,$2,$3,'booking_opportunity','outreach.target.request',
                      'content_suggestion',$4,$5,$6,'awaiting_approval',now(),$7)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(busy)
        .bind(decision_id)
        .bind(subject_id)
        .bind(format!("action-{}", Uuid::now_v7()))
        .bind(json!({"kind": "outreach.target.request"}))
        .bind(now + time::Duration::days(2))
        .execute(pool)
        .await?;
        upcoming_show(pool, busy, "busy-next", 5).await?;
        upcoming_show(pool, busy, "busy-later", 40).await?;
        sqlx::query(
            "INSERT INTO daily_briefings (workspace_id, local_date, title, body)
             VALUES ($1, $2, 'briefing', 'body')",
        )
        .bind(busy)
        .bind(today)
        .execute(pool)
        .await?;
        fan(pool, busy, "one@example.test", true).await?;
        fan(pool, busy, "two@example.test", true).await?;
        fan(pool, busy, "revoked@example.test", false).await?;

        // The outsider holds every resource — none of it may appear.
        let outsider_action = dispatched_action(pool, outsider).await?;
        touch(pool, outsider, outsider_action, "shared@example.test", now).await?;
        governor(pool, outsider, "shared@example.test", true, true).await?;
        upcoming_show(pool, outsider, "outsider-show", 3).await?;
        fan(pool, outsider, "outsider@example.test", true).await?;
        sqlx::query(
            "INSERT INTO daily_briefings (workspace_id, local_date, title, body)
             VALUES ($1, $2, 'briefing', 'body')",
        )
        .bind(outsider)
        .bind(today)
        .execute(pool)
        .await?;

        let overview = roster_overview(pool, label, now).await?;

        assert_eq!(overview.acts.len(), 2, "the outsider never appears");
        assert_eq!(overview.org_touches_30d, 2);

        // The empty act leads — it is missing everything.
        let empty_row = overview
            .acts
            .iter()
            .find(|act| act.workspace_id.into_uuid() == empty)
            .ok_or("empty act present")?;
        assert_eq!(overview.acts[0].workspace_id.into_uuid(), empty);
        assert_eq!(
            empty_row.gaps,
            vec![
                GAP_NO_UPCOMING_SHOW.to_owned(),
                GAP_NO_BRIEFING.to_owned(),
                GAP_NO_REACHABLE_FANS.to_owned(),
            ]
        );
        assert_eq!(empty_row.pipeline.pending_decisions, 0);
        assert_eq!(empty_row.pipeline.next_show_at, None);

        let busy_row = overview
            .acts
            .iter()
            .find(|act| act.workspace_id.into_uuid() == busy)
            .ok_or("busy act present")?;
        assert!(busy_row.gaps.is_empty());
        assert_eq!(
            busy_row.attention.touches_30d, 2,
            "the 45-day-old touch is out"
        );
        assert_eq!(busy_row.attention.contacts_on_hold, 1);
        assert_eq!(busy_row.attention.do_not_contact, 1);
        assert_eq!(busy_row.pipeline.pending_decisions, 1);
        assert_eq!(busy_row.pipeline.upcoming_shows, 2);
        assert!(busy_row.pipeline.next_show_at.is_some());
        assert_eq!(busy_row.latest_briefing_date, Some(today));
        assert_eq!(
            busy_row.reachable_fans, 2,
            "the revoked consent does not count"
        );

        // A stranger's organisation answers an empty page, not an error.
        let stranger = roster_overview(pool, Uuid::now_v7(), now).await?;
        assert!(stranger.acts.is_empty());
        assert_eq!(stranger.org_touches_30d, 0);

        Ok(())
    }
    .await
}
