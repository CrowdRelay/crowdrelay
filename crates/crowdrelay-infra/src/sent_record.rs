//! What actually left, and what did not (O.4, O.7).
//!
//! The console could always answer "did it work": `RecentAutopilotAction`
//! carries the status, the executor's own status, its provider reference and
//! the error kind. It could never answer the two questions an operator asks
//! before pressing the button again — **what did we say**, and **to whom**.
//!
//! Both have been on disk the whole time. `emit_outward_action` writes the
//! emitted payload into `outbox_events`, and
//! `autopilot_action_emissions` ties it to the action. Nothing read it
//! back. Trust in the next click is built by being able to see the last one.
//!
//! Two reads live here:
//!
//! * [`sent_record`] — one action, the words it carried and the addresses it
//!   went to.
//! * [`failed_sends`] — outward sends that failed in the last week, named. The
//!   overview counts them (`failed_24h`, `executor_failed_24h`); a count tells
//!   an operator that something broke and nothing about which promoter never
//!   heard from them.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// How far back [`failed_sends`] looks. The same week
/// `lapsed_approvals` uses, for the same reason: an operator back from a few
/// days away sees everything that went wrong while they were gone.
pub const FAILED_WINDOW_DAYS: i64 = 7;

/// Rows returned by [`failed_sends`]. Bounded because an operator page is not
/// an archive; the count is reported separately and is not capped.
const MAX_FAILED: i64 = 25;

/// One action, as the world outside received it.
#[derive(Debug, Serialize)]
pub struct SentRecord {
    pub action_id: Uuid,
    pub action_kind: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// The executor's own word for what happened, when one reported.
    pub executor_status: Option<String>,
    /// The provider's identifier for the thing that was sent — a message id, a
    /// post id. What a support request from the other end would quote.
    pub provider_reference: Option<String>,
    /// `crowdrelay.gig.outreach_requested` and friends. Absent when the action
    /// never emitted, which is itself the answer: nothing left.
    pub event_type: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub emitted_at: Option<OffsetDateTime>,
    /// The subject as it was sent, when the payload carried a draft.
    pub subject: Option<String>,
    /// The body as it was sent. This is the field the whole read exists for.
    pub body: Option<String>,
    /// Every address the emission named, in the order it named them.
    pub recipients: Vec<String>,
}

/// One outward send that failed.
#[derive(Debug, Serialize)]
pub struct FailedSend {
    pub action_id: Uuid,
    pub action_kind: String,
    pub context: String,
    /// `executor_unavailable`, a provider's own error kind, whatever was
    /// recorded. Named rather than counted.
    pub error_kind: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub attempt_count: i32,
    /// Who never heard from the tenant. Empty when the action failed before it
    /// emitted anything — nobody was written to, which is a different failure
    /// and reads differently.
    pub recipients: Vec<String>,
}

/// The failed sends of the last [`FAILED_WINDOW_DAYS`] days, and how many there
/// were in total.
#[derive(Debug, Serialize)]
pub struct FailedSends {
    pub window_days: i64,
    pub items: Vec<FailedSend>,
    pub total: i64,
}

/// Pulls every address out of an emitted payload, whatever shape it used.
///
/// Four shapes exist across the outward events and none of them is going to be
/// unified for this read: a `recipients` array of objects with `contact_email`,
/// a flat array of strings, a `recipients` object keyed by audience
/// (`{"band": [{"email": …}], "counterparty": {"email": …}}` — what the
/// post-show and release reports emit), and a single `recipient_email`.
/// Reading all four here keeps the console honest about sends the newest code
/// did not write — a report whose `recipients` is an object used to read back
/// as "sent to nobody", which is the lie this whole read exists to prevent.
fn addresses(payload: &serde_json::Value) -> Vec<String> {
    fn collect(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::String(email) => found.push(email.to_owned()),
            serde_json::Value::Array(list) => {
                for entry in list {
                    collect(entry, found);
                }
            }
            serde_json::Value::Object(map) => {
                // An entry that carries an address field is a recipient; an
                // object without one is a grouping (`band`, `counterparty`)
                // whose values hold the recipients.
                let mut named = false;
                for key in ["contact_email", "email", "recipient_email"] {
                    if let Some(email) = map.get(key).and_then(|v| v.as_str()) {
                        found.push(email.to_owned());
                        named = true;
                    }
                }
                if !named {
                    for nested in map.values() {
                        collect(nested, found);
                    }
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    if let Some(recipients) = payload.get("recipients") {
        collect(recipients, &mut found);
    }
    if found.is_empty()
        && let Some(single) = payload.get("recipient_email").and_then(|v| v.as_str())
    {
        found.push(single.to_owned());
    }
    found
}

fn draft_field<'a>(payload: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    payload
        .get("draft")
        .and_then(|draft| draft.get(field))
        .and_then(|value| value.as_str())
        .or_else(|| payload.get(field).and_then(|value| value.as_str()))
}

/// What one action sent, read back from the emission it wrote.
///
/// # Errors
///
/// Propagates the database error.
pub async fn sent_record(
    pool: &PgPool,
    workspace_id: Uuid,
    action_id: Uuid,
) -> Result<Option<SentRecord>, sqlx::Error> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<OffsetDateTime>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<OffsetDateTime>,
            Option<serde_json::Value>,
        ),
    >(
        r#"
        SELECT
            action.action_kind,
            action.status,
            action.finished_at,
            receipt.executor_status,
            receipt.provider_reference,
            outbound.event_type,
            emission.emitted_at,
            outbound.payload
        FROM autopilot_actions AS action
        LEFT JOIN autopilot_action_emissions AS emission
          ON emission.workspace_id = action.workspace_id
         AND emission.action_id = action.id
        LEFT JOIN outbox_events AS outbound
          ON outbound.workspace_id = emission.workspace_id
         AND outbound.id = emission.outbox_event_id
        -- The executor's own last word. `autopilot_execution_reports`
        -- is provider-confirmed evidence and is deliberately separate from the
        -- action's status: `succeeded` there means the provider did it, not
        -- that CrowdRelay committed an intent.
        LEFT JOIN LATERAL (
            SELECT report.status AS executor_status, report.provider_reference
            FROM autopilot_execution_reports AS report
            WHERE report.workspace_id = action.workspace_id
              AND report.action_id = action.id
            ORDER BY report.occurred_at DESC, report.id DESC
            LIMIT 1
        ) AS receipt ON true
        WHERE action.workspace_id = $1 AND action.id = $2
        ORDER BY emission.emitted_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(action_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| {
        let payload = row.7.unwrap_or(serde_json::Value::Null);
        SentRecord {
            action_id,
            action_kind: row.0,
            status: row.1,
            finished_at: row.2,
            executor_status: row.3,
            provider_reference: row.4,
            event_type: row.5,
            emitted_at: row.6,
            subject: draft_field(&payload, "subject").map(str::to_owned),
            body: draft_field(&payload, "body").map(str::to_owned),
            recipients: addresses(&payload),
        }
    }))
}

/// Outward sends that failed inside the window.
///
/// Bound on the durable `action_class`, so "outward" means what the ledger
/// recorded at decision time rather than what a payload claims. A first-party
/// action that failed is the system's own problem and already surfaces as an
/// alert; this list is the one where somebody outside never heard from the
/// tenant.
///
/// # Errors
///
/// Propagates the database error.
pub async fn failed_sends(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<FailedSends, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<String>,
            Option<OffsetDateTime>,
            i32,
            Option<serde_json::Value>,
            i64,
        ),
    >(
        r#"
        SELECT
            action.id,
            action.action_kind,
            action.context,
            action.last_error_kind,
            action.finished_at,
            action.attempt_count,
            outbound.payload,
            count(*) OVER ()::bigint AS total_count
        FROM autopilot_actions AS action
        LEFT JOIN autopilot_action_emissions AS emission
          ON emission.workspace_id = action.workspace_id
         AND emission.action_id = action.id
        LEFT JOIN outbox_events AS outbound
          ON outbound.workspace_id = emission.workspace_id
         AND outbound.id = emission.outbox_event_id
        WHERE action.workspace_id = $1
          AND action.status = 'failed'
          AND action.action_class IN ('owned_audience', 'third_party', 'paid')
          AND action.finished_at IS NOT NULL
          AND action.finished_at > $2 - make_interval(days => $3::int)
        ORDER BY action.finished_at DESC, action.id DESC
        LIMIT $4
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(i32::try_from(FAILED_WINDOW_DAYS).unwrap_or(7))
    .bind(MAX_FAILED)
    .fetch_all(pool)
    .await?;

    let total = rows.first().map_or(0, |row| row.7);
    let items = rows
        .into_iter()
        .map(|row| FailedSend {
            action_id: row.0,
            action_kind: row.1,
            context: row.2,
            error_kind: row.3,
            finished_at: row.4,
            attempt_count: row.5,
            recipients: row.6.as_ref().map(addresses).unwrap_or_default(),
        })
        .collect();

    Ok(FailedSends {
        window_days: FAILED_WINDOW_DAYS,
        items,
        total,
    })
}
