//! After the rest of the suite: no tuple-shaped timestamp left the building.
//!
//! Named `zz_` so the single-threaded suite runs it last, against every outbox
//! event and audit row the other tests wrote. A bare `OffsetDateTime` inside a
//! `json!` literal serializes as `[2026, 268, 7, 0, 0, 0, 0, 0, 0]`. Production
//! held it in every team-assignment email payload, in `event.updated`,
//! `event.cancelled`, `event.change_due` and `ticket.order.paid`, and in the
//! audit metadata the daily briefing cast with `::timestamptz` (which aborted a
//! whole day's team handoffs). The n8n workflows read these values with
//! `new Date(...)` and `String(...)`. Wrap the value in
//! `crowdrelay_domain::wire_time::Wire(&value)`.
//!
//! A static gate cannot do this job: `json!` hides the type. The rows the
//! suite actually produced do not.

use crate::common;

const TUPLE: &str = r"\[\s*(19|20|21)[0-9]{2}\s*,\s*[0-9]{1,3}\s*,\s*[0-9]{1,2}\s*,\s*[0-9]{1,2}\s*,\s*[0-9]{1,2}\s*,\s*[0-9]+\s*,\s*-?[0-9]{1,2}\s*,\s*-?[0-9]{1,2}\s*,\s*-?[0-9]{1,2}\s*\]";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn no_payload_or_audit_row_carries_a_tuple_timestamp()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let judged: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM outbox_events) + (SELECT count(*) FROM audit_events)
              + (SELECT count(*) FROM operator_actions)
              + (SELECT count(*) FROM autopilot_decisions)",
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        judged > 50,
        "only {judged} rows to judge; this must run after the rest of the suite"
    );
    let offenders: Vec<(String, String, i64)> = sqlx::query_as(
        r#"
        SELECT 'outbox', event_type, count(*)::bigint
        FROM outbox_events WHERE payload::text ~ $1 GROUP BY event_type
        UNION ALL
        SELECT 'audit', action, count(*)::bigint
        FROM audit_events WHERE metadata::text ~ $1 GROUP BY action
        UNION ALL
        SELECT 'operator_action', action, count(*)::bigint
        FROM operator_actions WHERE details::text ~ $1 GROUP BY action
        UNION ALL
        SELECT 'decision.input_snapshot', decision_kind, count(*)::bigint
        FROM autopilot_decisions WHERE input_snapshot::text ~ $1 GROUP BY decision_kind
        UNION ALL
        SELECT 'decision.policy_snapshot', decision_kind, count(*)::bigint
        FROM autopilot_decisions WHERE policy_snapshot::text ~ $1 GROUP BY decision_kind
        ORDER BY 1, 2
        "#,
    )
    .bind(TUPLE)
    .fetch_all(&pool)
    .await?;
    assert!(
        offenders.is_empty(),
        "these rows carry serde's tuple timestamp; wrap the value in \
         crowdrelay_domain::wire_time::Wire(&value): {offenders:?}"
    );
    Ok(())
}
