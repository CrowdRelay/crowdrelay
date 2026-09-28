//! A press pitch is an email send: dispatch re-verifies the recipient under
//! lock and reserves the contact window, like every other outward arm.
//!
//! The payload freezes `recipient_email`/`recipient_target_id` at approval
//! time, and a contact can change between approval and claim — marked
//! do-not-contact, deactivated, or re-addressed. Before the re-pin existed,
//! the frozen address went out anyway and no `contact_touches` row was
//! written, so the pitch spent none of the cooldown or monthly attention
//! budget every other send answers to.

use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::AutopilotActionRepository;
use serde_json::json;
use uuid::Uuid;

use crate::autopilot_outreach_engine::fixture;

/// The action row as the worker's producer writes it: an approved
/// `agent.content.request` whose payload addresses the draft to one
/// registry contact.
async fn seed_pitch(
    f: &crate::autopilot_outreach_engine::Fixture,
    target_id: Uuid,
    recipient_email: &str,
    recipient_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'growth_intelligence','agent_outcome',$4,
                   'request_agent_content',9000,'require_approval','pitch',
                   '{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(f.ws())
    .bind(format!("decision-{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(f.now)
    .execute(&f.pool)
    .await?;
    let payload = json!({
        "kind": "request_agent_content",
        "template_id": "press-pitch",
        "task_id": Uuid::now_v7(),
        "draft": {"subject": "Virya — nowy singel", "body": "Dzień dobry."},
        "recipient_email": recipient_email,
        "recipient_name": recipient_name,
        "recipient_target_id": target_id,
    });
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'growth_intelligence','agent.content.request','agent_outcome',
                   $4,$5,$6,'queued',$7,$8,'operator:test',$8)"#,
    )
    .bind(action_id)
    .bind(f.ws())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("action-{action_id}"))
    .bind(&payload)
    // The row's class is the payload's own classification, not a fixture
    // literal — the evidence gate reads the truth.
    .bind(
        serde_json::from_value::<crowdrelay_application::autopilot::AutopilotActionPayload>(
            payload.clone(),
        )
        .map(|parsed| parsed.action_class().as_str())
        .unwrap_or("third_party"),
    )
    .bind(f.now)
    .execute(&f.pool)
    .await?;
    Ok(action_id)
}

async fn target_email(f: &crate::autopilot_outreach_engine::Fixture, target: Uuid) -> String {
    sqlx::query_scalar("SELECT contact_email FROM outreach_targets WHERE id = $1")
        .bind(target)
        .fetch_one(&f.pool)
        .await
        .expect("the seeded target carries an address")
}

async fn claim_and_execute(
    f: &crate::autopilot_outreach_engine::Fixture,
    action_id: Uuid,
) -> Result<(), RepositoryError> {
    let claimed = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, f.now)
        .await
        .expect("claim runs");
    let action = claimed
        .iter()
        .find(|action| action.id.into_uuid() == action_id)
        .expect("the queued pitch is claimable");
    f.repository
        .execute_action(f.workspace_id, action, f.now)
        .await
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_pitch_to_a_do_not_contact_target_never_leaves_the_gate()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("press-pitch-dnc").await?;
    // Approved while live, marked do-not-contact before the claim.
    let target = f
        .target_at(
            "Gazeta Kultura",
            "press",
            true,
            false,
            None,
            "press.example.pl",
        )
        .await?;
    let email = target_email(&f, target).await;
    let action_id = seed_pitch(&f, target, &email, "Gazeta Kultura").await?;
    sqlx::query("UPDATE outreach_targets SET do_not_contact = true WHERE id = $1")
        .bind(target)
        .execute(&f.pool)
        .await?;

    let result = claim_and_execute(&f, action_id).await;
    assert!(
        matches!(result, Err(RepositoryError::Conflict)),
        "a suppressed contact must refuse, got {result:?}"
    );
    let emitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.agent.content_requested'",
    )
    .bind(f.ws())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(emitted, 0, "the refused pitch emitted no send event");
    let touches: i64 =
        sqlx::query_scalar("SELECT count(*) FROM contact_touches WHERE workspace_id = $1")
            .bind(f.ws())
            .fetch_one(&f.pool)
            .await?;
    assert_eq!(touches, 0, "a refused send spends no contact budget");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_pitch_to_a_readdressed_target_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("press-pitch-moved").await?;
    let target = f
        .target_at("Radio Zet", "press", true, false, None, "radio.example.pl")
        .await?;
    let email = target_email(&f, target).await;
    let action_id = seed_pitch(&f, target, &email, "Radio Zet").await?;
    // The contact's mailbox moved after the approval froze the payload —
    // sending to the stale address writes to somebody the operator never
    // approved.
    sqlx::query(
        "UPDATE outreach_targets SET contact_email = 'new-desk@radio.example.pl' WHERE id = $1",
    )
    .bind(target)
    .execute(&f.pool)
    .await?;

    let result = claim_and_execute(&f, action_id).await;
    assert!(
        matches!(result, Err(RepositoryError::Conflict)),
        "a re-addressed contact is a different recipient — refuse, got {result:?}"
    );
    let emitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.agent.content_requested'",
    )
    .bind(f.ws())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(emitted, 0, "the refused pitch emitted no send event");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_clean_pitch_reserves_the_contact_window_and_emits()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("press-pitch-clean").await?;
    let target = f
        .target_at(
            "Dziennik Muzyczny",
            "press",
            true,
            false,
            None,
            "music.example.pl",
        )
        .await?;
    let email = target_email(&f, target).await;
    let action_id = seed_pitch(&f, target, &email, "Dziennik Muzyczny").await?;

    claim_and_execute(&f, action_id).await?;

    // The pitch spends against the same ledger every other send answers to.
    let normalized = email.trim().to_lowercase();
    let governor: Option<(String,)> = sqlx::query_as(
        "SELECT last_context FROM contact_governor
         WHERE workspace_id = $1 AND normalized_contact = $2",
    )
    .bind(f.ws())
    .bind(&normalized)
    .fetch_optional(&f.pool)
    .await?;
    assert_eq!(
        governor.map(|row| row.0).as_deref(),
        Some("press_pitch"),
        "the governor saw the pitch as a contact"
    );
    let touches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contact_touches
         WHERE workspace_id = $1 AND normalized_contact = $2",
    )
    .bind(f.ws())
    .bind(&normalized)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(touches, 1, "the touch is on the ledger");
    // And the send event carries the row's current address, not just the
    // payload's frozen one.
    let emitted: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT payload FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.agent.content_requested'",
    )
    .bind(f.ws())
    .fetch_optional(&f.pool)
    .await?;
    let (payload,) = emitted.expect("the pitch emitted its send event");
    assert_eq!(
        payload.get("recipient_email").and_then(|v| v.as_str()),
        Some(email.as_str()),
        "the event addresses the re-pinned recipient"
    );
    Ok(())
}
