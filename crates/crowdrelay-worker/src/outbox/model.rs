use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
pub(super) struct OutboxEventClaim {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub attempt_number: i32,
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct DeliveryClaim {
    pub delivery_id: Uuid,
    pub workspace_id: Uuid,
    pub event_id: Uuid,
    pub event_type: String,
    pub event_version: i32,
    pub payload: Value,
    pub event_created_at: OffsetDateTime,
    pub request_id: Option<String>,
    pub trace_id: Option<Uuid>,
    pub action_id: Option<Uuid>,
    pub endpoint_id: Uuid,
    pub endpoint_url: String,
    pub signing_secret_ref: String,
    pub timeout_ms: i32,
    pub attempt_number: i32,
    pub max_attempts: i32,
}

impl DeliveryClaim {
    pub fn public_event_id(&self) -> String {
        format!("evt_{}", self.event_id.simple())
    }
}

#[derive(Debug, Serialize)]
pub(super) struct WebhookEnvelope<'a> {
    pub id: &'a str,
    #[serde(rename = "type")]
    pub event_type: &'a str,
    pub version: i32,
    pub workspace_id: Uuid,
    pub occurred_at: &'a str,
    pub data: &'a Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AttemptOutcome {
    Delivered,
    Retry,
    Dead,
    /// The transport exhausted retries after ambiguous failures (e.g.,
    /// provider timeouts where the request may or may not have reached
    /// the provider). The outbox delivery is dead (transport gave up),
    /// but the linked autopilot action transitions to `unknown` because
    /// we cannot confirm whether the side effect happened.
    Ambiguous,
}

impl AttemptOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Retry => "retry",
            Self::Dead => "dead",
            Self::Ambiguous => "ambiguous",
        }
    }

    pub const fn delivery_status(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Retry => "pending",
            Self::Dead | Self::Ambiguous => "dead",
        }
    }
}

#[derive(Debug)]
pub(super) struct AttemptResolution {
    pub outcome: AttemptOutcome,
    pub response_status: Option<i16>,
    pub error_kind: Option<&'static str>,
    pub retry_delay_ms: i64,
    pub started_at: OffsetDateTime,
    pub finished_at: OffsetDateTime,
    pub duration_ms: i32,
}
