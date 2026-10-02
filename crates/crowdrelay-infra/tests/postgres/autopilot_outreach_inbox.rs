//! In-flight outreach is a property of the inbox, not of one opportunity.
//! Split from `autopilot_outreach_engine.rs`, whose fixture it reuses.

use crate::autopilot_outreach_engine::fixture;
use crowdrelay_domain::OutreachTargetId;
use uuid::Uuid;

/// Measured in production 2026-10-02: NNRadio held three pending pitches
/// (two for the same video under two release sources) and Power Radio
/// Berlin-Brandenburg two. `in_flight` was scoped to the opportunity, so each
/// sibling opportunity of one inbox parked its own approval card. It is
/// scoped to the target: a pending card on one opportunity holds every other
/// opportunity of that address, and never another address.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_pending_pitch_holds_every_opportunity_of_the_same_inbox()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("target-in-flight").await?;
    let station = f.target("Radio Szum", "radio", true, false, None).await?;
    let other = f.target("Radio Cisza", "radio", true, false, None).await?;
    let mut opportunities = Vec::new();
    for (target, key) in [
        (station, "release:a"),
        (station, "release:b"),
        (other, "release:a"),
    ] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO outreach_opportunities
                 (id, workspace_id, target_id, source, subject_kind, subject_key, template_key,
                  relevance_basis_points, confidence_basis_points, observed_at, expires_at)
             VALUES ($1, $2, $3, 'release_autopilot', 'release', $4, 'release',
                     8500, 9000, $5, $6)",
        )
        .bind(id)
        .bind(f.ws())
        .bind(target)
        .bind(key)
        .bind(f.now)
        .bind(f.now + time::Duration::days(30))
        .execute(&f.pool)
        .await?;
        opportunities.push(id);
    }
    let in_flight = |target: Uuid| {
        let f = &f;
        async move {
            let snapshots = f
                .repository
                .load_target_outreach_snapshots(f.workspace_id, OutreachTargetId::from_uuid(target))
                .await?;
            Ok::<_, Box<dyn std::error::Error>>(
                snapshots.iter().map(|s| s.in_flight).collect::<Vec<_>>(),
            )
        }
    };
    assert_eq!(
        in_flight(station).await?,
        [false, false],
        "nothing pending yet"
    );

    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id)
         VALUES ($1,$2,$3,'outreach','outreach_opportunity',$4,
                 'request_relationship_outreach',9000,'require_approval','test',
                 '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
    )
    .bind(decision_id)
    .bind(f.ws())
    .bind(format!("decision:outreach:{decision_id}"))
    .bind(opportunities[0])
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_actions (
             workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, action_class)
         VALUES ($1,$2,'outreach','outreach.request','outreach_opportunity',$3,$4,
                 '{}'::jsonb,'awaiting_approval','third_party')",
    )
    .bind(f.ws())
    .bind(decision_id)
    .bind(opportunities[0])
    .bind(format!("action:outreach:{decision_id}"))
    .execute(&f.pool)
    .await?;

    assert_eq!(
        in_flight(station).await?,
        [true, true],
        "the sibling opportunity of the same inbox is held while one card is pending"
    );
    assert_eq!(
        in_flight(other).await?,
        [false],
        "another address is not held by it"
    );
    Ok(())
}
