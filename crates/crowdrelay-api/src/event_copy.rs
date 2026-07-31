//! Trusted application of AI-assisted event copy.
//!
//! The generator never writes directly to the public event record. It receives
//! a content-addressed outbox request, returns a bounded candidate, and this
//! endpoint atomically verifies that the source request is still current before
//! making the description public.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_DESCRIPTION_CHARS: usize = 4_000;
const MAX_MODEL_CHARS: usize = 160;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyEventCopyRequest {
    enrichment_id: Uuid,
    source_hash: String,
    language: String,
    model: String,
    description: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyEventCopyResponse {
    applied: bool,
    duplicate: bool,
    stale: bool,
    event_id: Uuid,
    enrichment_id: Uuid,
    updated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct EnrichmentRow {
    id: Uuid,
    event_id: Uuid,
    source_hash: Vec<u8>,
    language: String,
    status: String,
    model: Option<String>,
    generated_description: Option<String>,
}

pub async fn apply_event_copy(
    State(state): State<crate::AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ApplyEventCopyRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let event_id = match Uuid::parse_str(&event_id) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let normalized = match normalize_request(payload) {
        Some(value) => value,
        None => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };

    let future = apply_event_copy_inner(&state, event_id, normalized);
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(2),
        future,
    )
    .await
    {
        Ok(Ok(response)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(response),
        )
            .into_response(),
        Ok(Err(ApplyCopyError::NotFound)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(ApplyCopyError::Conflict)) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Ok(Err(ApplyCopyError::Unavailable)) | Err(_) => {
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

struct NormalizedApplyRequest {
    enrichment_id: Uuid,
    source_hash: [u8; 32],
    language: String,
    model: String,
    description: String,
}

fn normalize_request(request: ApplyEventCopyRequest) -> Option<NormalizedApplyRequest> {
    let hash = hex::decode(request.source_hash.trim()).ok()?;
    let source_hash: [u8; 32] = hash.try_into().ok()?;
    let language = request.language.trim().to_ascii_lowercase();
    if language.len() != 2 || !language.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }
    let model = clean_text(&request.model, MAX_MODEL_CHARS)?;
    let description = clean_text(&request.description, MAX_DESCRIPTION_CHARS)?;
    if description.lines().count() > 12 {
        return None;
    }
    Some(NormalizedApplyRequest {
        enrichment_id: request.enrichment_id,
        source_hash,
        language,
        model,
        description,
    })
}

fn clean_text(value: &str, maximum_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > maximum_chars
        || value
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        return None;
    }
    Some(value.to_owned())
}

async fn apply_event_copy_inner(
    state: &crate::AppState,
    event_id: Uuid,
    request: NormalizedApplyRequest,
) -> Result<ApplyEventCopyResponse, ApplyCopyError> {
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(|_| ApplyCopyError::Unavailable)?;
    configure_transaction(&mut transaction, state).await?;

    let enrichment = sqlx::query_as::<_, EnrichmentRow>(
        r#"
        SELECT id, event_id, source_hash, language::text AS language, status,
               model, generated_description
        FROM event_copy_enrichments
        WHERE workspace_id = $1 AND id = $2 AND event_id = $3
        FOR UPDATE
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(request.enrichment_id)
    .bind(event_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| ApplyCopyError::Unavailable)?
    .ok_or(ApplyCopyError::NotFound)?;

    if enrichment.source_hash.as_slice() != request.source_hash.as_slice()
        || enrichment.language != request.language
    {
        return Err(ApplyCopyError::Conflict);
    }
    if enrichment.status == "applied" {
        let duplicate = enrichment.model.as_deref() == Some(request.model.as_str())
            && enrichment.generated_description.as_deref() == Some(request.description.as_str());
        if !duplicate {
            return Err(ApplyCopyError::Conflict);
        }
        transaction
            .commit()
            .await
            .map_err(|_| ApplyCopyError::Unavailable)?;
        return Ok(ApplyEventCopyResponse {
            applied: true,
            duplicate: true,
            stale: false,
            event_id: enrichment.event_id,
            enrichment_id: enrichment.id,
            updated_at: OffsetDateTime::now_utc(),
        });
    }
    if enrichment.status != "pending" {
        return Err(ApplyCopyError::Conflict);
    }

    let updated = sqlx::query_scalar::<_, OffsetDateTime>(
        r#"
        UPDATE events
        SET description = $4,
            description_origin = 'ai',
            description_source_hash = $5,
            description_language = $6,
            updated_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND description_origin <> 'manual'
          AND EXISTS (
              SELECT 1
              FROM event_copy_enrichments AS enrichment
              WHERE enrichment.workspace_id = $1
                AND enrichment.id = $3
                AND enrichment.event_id = events.id
                AND enrichment.source_hash = $5
                AND enrichment.status = 'pending'
          )
        RETURNING updated_at
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_id)
    .bind(request.enrichment_id)
    .bind(&request.description)
    .bind(request.source_hash.as_slice())
    .bind(&request.language)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| ApplyCopyError::Unavailable)?;

    let stale = updated.is_none();
    sqlx::query(
        r#"
        UPDATE event_copy_enrichments
        SET status = $4,
            model = $5,
            generated_description = CASE WHEN $4 = 'applied' THEN $6 ELSE NULL END,
            rejection_reason = CASE WHEN $4 = 'stale' THEN 'event description was manually curated or source changed' ELSE NULL END,
            completed_at = now()
        WHERE workspace_id = $1 AND id = $2 AND event_id = $3 AND status = 'pending'
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(request.enrichment_id)
    .bind(event_id)
    .bind(if stale { "stale" } else { "applied" })
    .bind(&request.model)
    .bind(&request.description)
    .execute(&mut *transaction)
    .await
    .map_err(|_| ApplyCopyError::Unavailable)?;

    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, metadata
        ) VALUES ($1, 'service', $2, 'event', $3, $4)
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(if stale {
        "event.copy_enrichment_stale"
    } else {
        "event.copy_enriched"
    })
    .bind(event_id.to_string())
    .bind(serde_json::json!({
        "enrichment_id": request.enrichment_id,
        "language": request.language,
        "model": request.model,
        "source_hash": hex::encode(request.source_hash),
    }))
    .execute(&mut *transaction)
    .await
    .map_err(|_| ApplyCopyError::Unavailable)?;

    transaction
        .commit()
        .await
        .map_err(|_| ApplyCopyError::Unavailable)?;
    Ok(ApplyEventCopyResponse {
        applied: !stale,
        duplicate: false,
        stale,
        event_id,
        enrichment_id: request.enrichment_id,
        updated_at: updated.unwrap_or_else(OffsetDateTime::now_utc),
    })
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
) -> Result<(), ApplyCopyError> {
    let statement_ms = state.ticketing.operation_timeout().as_millis();
    let lock_ms = state.ticketing.lock_timeout().as_millis();
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(|_| ApplyCopyError::Unavailable)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum ApplyCopyError {
    NotFound,
    Conflict,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_request_is_content_addressed_and_bounded() {
        let request = ApplyEventCopyRequest {
            enrichment_id: Uuid::nil(),
            source_hash: "00".repeat(32),
            language: "PL".to_owned(),
            model: "gemini-2.5-flash-lite".to_owned(),
            description: "Pierwszy akapit.\n\nDrugi akapit.".to_owned(),
        };
        let normalized = normalize_request(request).expect("valid request");
        assert_eq!(normalized.source_hash, [0; 32]);
        assert_eq!(normalized.language, "pl");
        assert!(
            normalize_request(ApplyEventCopyRequest {
                enrichment_id: Uuid::nil(),
                source_hash: "invalid".to_owned(),
                language: "pl".to_owned(),
                model: "gemini".to_owned(),
                description: "Opis".to_owned(),
            })
            .is_none()
        );
    }
}
