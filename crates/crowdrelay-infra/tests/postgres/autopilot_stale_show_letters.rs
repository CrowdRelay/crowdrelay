//! A show opportunity's letter composed before show letters existed is
//! refused at execution. Borrows the outreach-engine fixtures.
//!
//! On 2026-09-27 thirty-five outreach actions under the Gorzów show's
//! opportunities carried the catalogue pitch — "we would love to submit
//! {album} for coverage" — and waited for a wave approval that would have
//! sent them as written. #322 made new ones compose the show letter; this
//! pins that the stale ones cannot go out.

use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::AutopilotActionRepository;
use serde_json::json;
use uuid::Uuid;

use crate::autopilot_outreach_engine::fixture;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_album_pitch_under_a_show_opportunity_is_refused()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("stale-show-letter").await?;
    f.release_source(
        "Echoes",
        "https://open.spotify.example/album/echoes",
        "album",
        Some(1_746_057_600),
    )
    .await?;
    let target = f
        .target_at(
            "Radio Lokalne",
            "radio",
            true,
            false,
            None,
            "radio.example.pl",
        )
        .await?;
    f.event_in_city(20, "published", "Gorzów Wielkopolski")
        .await?;
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    let (opportunity_id, target_version) = sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT opportunity.id, target.version
         FROM outreach_opportunities AS opportunity
         JOIN outreach_targets AS target ON target.id = opportunity.target_id
         WHERE opportunity.workspace_id = $1 AND opportunity.source = 'event_autopilot'
           AND opportunity.target_id = $2",
    )
    .bind(f.ws())
    .bind(target)
    .fetch_one(&f.pool)
    .await?;

    // The shape the old candidate wrote: the target-kind template and the
    // catalogue letter, under a show opportunity.
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'outreach','outreach_opportunity',$4,'request_relationship_outreach',
                   8800,'require_approval','stale','{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(f.ws())
    .bind(format!("decision-{decision_id}"))
    .bind(opportunity_id)
    .bind(f.now)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'outreach','outreach.request','outreach_opportunity',$4,$5,$6,
                   'queued','third_party',$7,'operator:test',$7)"#,
    )
    .bind(action_id)
    .bind(f.ws())
    .bind(decision_id)
    .bind(opportunity_id)
    .bind(format!("action-{action_id}"))
    .bind(json!({
        "kind": "request_outreach",
        "opportunity_id": opportunity_id,
        "target_id": target,
        "target_version": target_version,
        "target_name": target.to_string(),
        "phase": "initial",
        "template_key": "outreach.radio.v1",
        "wave_id": null,
        "draft": {
            "subject": "Supply Test Act — Echoes",
            "body": "Dzień dobry,\n\nPiszemy w imieniu Supply Test Act i chcielibyśmy zaproponować Wam Echoes do anteny.",
        },
    }))
    .bind(f.now)
    .execute(&f.pool)
    .await?;

    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, f.now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued outreach action is claimable");
    let result = f
        .repository
        .execute_action(f.workspace_id, action, f.now)
        .await;
    assert!(
        matches!(
            result,
            Err(RepositoryError::ConflictBecause(reason)) if reason.contains("album pitch")
        ),
        "a stale show letter must be refused, got {result:?}"
    );
    let sent = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM outreach_interactions WHERE workspace_id = $1 AND target_id = $2",
    )
    .bind(f.ws())
    .bind(target)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(sent, 0, "nothing was recorded as sent");
    Ok(())
}
