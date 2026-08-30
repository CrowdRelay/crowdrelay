use std::time::Duration;

use reqwest::{
    Client, StatusCode, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{
    SecretValue,
    model::{DeliveryClaim, WebhookEnvelope},
    signature::sign_webhook,
};

/// HTTP header name for the CrowdRelay event UUID.
pub const CROWDRELAY_EVENT_ID: &str = "CrowdRelay-Event-Id";
/// HTTP header name for the CrowdRelay event type.
pub const CROWDRELAY_EVENT_TYPE: &str = "CrowdRelay-Event-Type";
/// HTTP header name for the CrowdRelay event schema version.
pub const CROWDRELAY_EVENT_VERSION: &str = "CrowdRelay-Event-Version";
/// HTTP header name for the CrowdRelay webhook timestamp.
pub const CROWDRELAY_TIMESTAMP: &str = "CrowdRelay-Timestamp";
/// HTTP header name for the CrowdRelay HMAC-SHA256 signature.
pub const CROWDRELAY_SIGNATURE: &str = "CrowdRelay-Signature";

const CROWDRELAY_REQUEST_ID: &str = "CrowdRelay-Request-Id";
const CROWDRELAY_TRACE_ID: &str = "CrowdRelay-Trace-Id";

#[derive(Clone, Debug)]
pub(super) struct WebhookDispatcher {
    client: Client,
    allow_http_endpoints: bool,
}

impl WebhookDispatcher {
    pub fn new(
        connect_timeout: Duration,
        user_agent: &str,
        allow_http_endpoints: bool,
    ) -> Result<Self, TransportBuildError> {
        let client = Client::builder()
            .connect_timeout(connect_timeout)
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .tcp_keepalive(Duration::from_secs(30))
            .redirect(Policy::none())
            .user_agent(user_agent)
            .build()
            .map_err(TransportBuildError::Client)?;

        Ok(Self {
            client,
            allow_http_endpoints,
        })
    }

    pub async fn dispatch(&self, claim: &DeliveryClaim, secret: &SecretValue) -> DispatchResult {
        let endpoint = match validate_endpoint(&claim.endpoint_url, self.allow_http_endpoints) {
            Ok(endpoint) => endpoint,
            Err(error_kind) => return DispatchResult::permanent(error_kind),
        };

        let event_id = claim.public_event_id();
        let occurred_at = match claim.event_created_at.format(&Rfc3339) {
            Ok(value) => value,
            Err(_) => return DispatchResult::permanent("invalid_event_timestamp"),
        };
        let envelope = WebhookEnvelope {
            id: &event_id,
            event_type: &claim.event_type,
            version: claim.event_version,
            workspace_id: claim.workspace_id,
            occurred_at: &occurred_at,
            data: &claim.payload,
        };
        let body = match serde_json::to_vec(&envelope) {
            Ok(body) => body,
            Err(_) => return DispatchResult::permanent("event_serialization"),
        };
        let timestamp = OffsetDateTime::now_utc().unix_timestamp();
        let signature = match sign_webhook(secret, timestamp, &body) {
            Ok(signature) => format!("v1={signature}"),
            Err(_) => return DispatchResult::permanent("invalid_signing_secret"),
        };
        let headers = match protocol_headers(claim, &event_id, timestamp, &signature) {
            Ok(headers) => headers,
            Err(error_kind) => return DispatchResult::permanent(error_kind),
        };

        match self
            .client
            .post(endpoint)
            .timeout(Duration::from_millis(
                u64::try_from(claim.timeout_ms).unwrap_or(1),
            ))
            .headers(headers)
            .body(body)
            .send()
            .await
        {
            Ok(response) => classify_http_status(response.status()),
            Err(error) => classify_transport_error(&error),
        }
    }
}

fn protocol_headers(
    claim: &DeliveryClaim,
    event_id: &str,
    timestamp: i64,
    signature: &str,
) -> Result<HeaderMap, &'static str> {
    let mut headers = HeaderMap::with_capacity(7);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    insert_header(&mut headers, CROWDRELAY_EVENT_ID, event_id)?;
    insert_header(&mut headers, CROWDRELAY_EVENT_TYPE, &claim.event_type)?;
    insert_header(
        &mut headers,
        CROWDRELAY_EVENT_VERSION,
        &claim.event_version.to_string(),
    )?;
    insert_header(&mut headers, CROWDRELAY_TIMESTAMP, &timestamp.to_string())?;
    insert_header(&mut headers, CROWDRELAY_SIGNATURE, signature)?;

    if let Some(request_id) = &claim.request_id {
        // Trace propagation is best-effort; an old malformed stored request ID
        // must not block a business event forever.
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(CROWDRELAY_REQUEST_ID.as_bytes()),
            HeaderValue::from_str(request_id),
        ) {
            headers.insert(name, value);
        }
    }

    if let Some(trace_id) = claim.trace_id {
        // Trace_id propagation is best-effort — same rationale as request_id.
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(CROWDRELAY_TRACE_ID.as_bytes()),
            HeaderValue::from_str(&trace_id.to_string()),
        ) {
            headers.insert(name, value);
        }
    }

    Ok(headers)
}

fn insert_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), &'static str> {
    let name =
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid_protocol_header_name")?;
    let value = HeaderValue::from_str(value).map_err(|_| "invalid_event_metadata")?;
    headers.insert(name, value);
    Ok(())
}

fn validate_endpoint(value: &str, allow_http: bool) -> Result<Url, &'static str> {
    let endpoint = Url::parse(value).map_err(|_| "invalid_endpoint_url")?;
    let scheme_is_allowed =
        endpoint.scheme() == "https" || (allow_http && endpoint.scheme() == "http");

    if !scheme_is_allowed {
        return Err("endpoint_scheme_not_allowed");
    }
    if endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err("invalid_endpoint_url");
    }

    Ok(endpoint)
}

pub(super) fn classify_http_status(status: StatusCode) -> DispatchResult {
    if status.is_success() {
        return DispatchResult::delivered(status);
    }

    if matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error() {
        DispatchResult::retryable(Some(status), "http_retryable_status")
    } else {
        DispatchResult::permanent_with_status(status, "http_permanent_status")
    }
}

fn classify_transport_error(error: &reqwest::Error) -> DispatchResult {
    let kind = if error.is_timeout() {
        "transport_timeout"
    } else if error.is_connect() {
        "transport_connect"
    } else {
        "transport_request"
    };

    DispatchResult::retryable(None, kind)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DispatchDisposition {
    Delivered,
    Retryable,
    Permanent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DispatchResult {
    pub disposition: DispatchDisposition,
    pub response_status: Option<i16>,
    pub error_kind: Option<&'static str>,
}

impl DispatchResult {
    pub const fn delivered(status: StatusCode) -> Self {
        Self {
            disposition: DispatchDisposition::Delivered,
            response_status: Some(status.as_u16() as i16),
            error_kind: None,
        }
    }

    pub const fn retryable(status: Option<StatusCode>, error_kind: &'static str) -> Self {
        Self {
            disposition: DispatchDisposition::Retryable,
            response_status: match status {
                Some(status) => Some(status.as_u16() as i16),
                None => None,
            },
            error_kind: Some(error_kind),
        }
    }

    pub const fn permanent(error_kind: &'static str) -> Self {
        Self {
            disposition: DispatchDisposition::Permanent,
            response_status: None,
            error_kind: Some(error_kind),
        }
    }

    pub const fn permanent_with_status(status: StatusCode, error_kind: &'static str) -> Self {
        Self {
            disposition: DispatchDisposition::Permanent,
            response_status: Some(status.as_u16() as i16),
            error_kind: Some(error_kind),
        }
    }
}

#[derive(Debug, Error)]
pub(super) enum TransportBuildError {
    #[error("failed to build the bounded webhook HTTP client")]
    Client(#[source] reqwest::Error),
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;

    fn claim() -> DeliveryClaim {
        DeliveryClaim {
            delivery_id: Uuid::from_u128(1),
            workspace_id: Uuid::from_u128(2),
            event_id: Uuid::from_u128(3),
            event_type: "fan.created".to_owned(),
            event_version: 1,
            payload: json!({"fan_id": "not-logged"}),
            event_created_at: OffsetDateTime::UNIX_EPOCH,
            request_id: Some("019fa000-0000-7000-8000-000000000001".to_owned()),
            trace_id: Some(Uuid::from_u128(0x019fa000_0000_7000_8000_000000000002)),
            endpoint_id: Uuid::from_u128(4),
            endpoint_url: "https://n8n.example/webhook".to_owned(),
            signing_secret_ref: "n8n/current".to_owned(),
            timeout_ms: 5_000,
            attempt_number: 1,
            max_attempts: 12,
        }
    }

    #[test]
    fn emits_the_documented_protocol_headers() -> Result<(), Box<dyn std::error::Error>> {
        let claim = claim();
        let headers = protocol_headers(&claim, "evt_0003", 1_785_240_000, "v1=abc")?;

        assert_eq!(headers[CROWDRELAY_EVENT_ID], "evt_0003");
        assert_eq!(headers[CROWDRELAY_EVENT_TYPE], "fan.created");
        assert_eq!(headers[CROWDRELAY_EVENT_VERSION], "1");
        assert_eq!(headers[CROWDRELAY_TIMESTAMP], "1785240000");
        assert_eq!(headers[CROWDRELAY_SIGNATURE], "v1=abc");
        Ok(())
    }

    #[test]
    fn propagates_trace_id_header() -> Result<(), Box<dyn std::error::Error>> {
        let claim = claim();
        let headers = protocol_headers(&claim, "evt_0003", 1_785_240_000, "v1=abc")?;
        assert!(headers.contains_key(CROWDRELAY_TRACE_ID));
        Ok(())
    }

    #[test]
    fn only_success_statuses_are_delivered() {
        assert_eq!(
            classify_http_status(StatusCode::ACCEPTED).disposition,
            DispatchDisposition::Delivered
        );
        assert_eq!(
            classify_http_status(StatusCode::NO_CONTENT).disposition,
            DispatchDisposition::Delivered
        );
        assert_eq!(
            classify_http_status(StatusCode::MULTIPLE_CHOICES).disposition,
            DispatchDisposition::Permanent
        );
    }

    #[test]
    fn retries_only_transient_http_statuses() -> Result<(), Box<dyn std::error::Error>> {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_EARLY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::from_u16(599)?,
        ] {
            assert_eq!(
                classify_http_status(status).disposition,
                DispatchDisposition::Retryable,
                "{status} should be retried"
            );
        }

        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
        ] {
            assert_eq!(
                classify_http_status(status).disposition,
                DispatchDisposition::Permanent,
                "{status} needs operator action"
            );
        }
        Ok(())
    }

    #[test]
    fn production_endpoint_policy_requires_https_and_no_userinfo() {
        assert!(validate_endpoint("https://n8n.example/webhook", false).is_ok());
        assert_eq!(
            validate_endpoint("http://n8n.example/webhook", false),
            Err("endpoint_scheme_not_allowed")
        );
        assert_eq!(
            validate_endpoint("https://admin:secret@n8n.example/webhook", false),
            Err("invalid_endpoint_url")
        );
        assert_eq!(
            validate_endpoint("https://n8n.example/webhook#secret", false),
            Err("invalid_endpoint_url")
        );
        assert!(validate_endpoint("http://127.0.0.1:5678/webhook", true).is_ok());
    }
}
