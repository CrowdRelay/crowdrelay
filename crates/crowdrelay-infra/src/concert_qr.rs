//! PostgreSQL persistence for concert QR campaigns and check-ins.
//!
//! The API layer retains token signing/verification, input validation and
//! response formatting. This adapter owns the durable write transactions:
//! campaign creation, revocation and idempotent fan check-in.

use async_trait::async_trait;
use crowdrelay_application::{
    CheckinCommand, CheckinResult, ConcertEventInfo, ConcertQrError, ConcertQrRepository,
    CreateCampaignCommand, CreateCampaignResult, RevokeCampaignCommand,
};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

/// PostgreSQL implementation of [`ConcertQrRepository`].
#[derive(Clone)]
pub struct PostgresConcertQrRepository {
    pool: PgPool,
}

impl PostgresConcertQrRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Debug, FromRow)]
struct EventRow {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct LockedCampaignRow {
    id: Uuid,
    event_id: Uuid,
    valid_from: OffsetDateTime,
    valid_until: OffsetDateTime,
    max_checkins: Option<i32>,
    active: bool,
    revoked_at: Option<OffsetDateTime>,
}

#[async_trait]
impl ConcertQrRepository for PostgresConcertQrRepository {
    async fn create_campaign(
        &self,
        command: &CreateCampaignCommand,
    ) -> Result<CreateCampaignResult, ConcertQrError> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            tracing::warn!(%error, "concert QR create_campaign begin failed");
            ConcertQrError::Unavailable
        })?;

        let event = match sqlx::query_as::<_, EventRow>(
            r#"
                SELECT id, slug, title, venue, starts_at
                FROM events
                WHERE workspace_id = $1 AND slug = $2 AND status = 'published'
                FOR SHARE
                "#,
        )
        .bind(command.workspace_id)
        .bind(&command.event_slug)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(ConcertQrError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "concert QR create_campaign event lookup failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        let campaign_id = Uuid::now_v7();
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO concert_qr_campaigns (
                id, workspace_id, event_id, label, valid_from, valid_until,
                max_checkins, created_at, updated_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
            "#,
        )
        .bind(campaign_id)
        .bind(command.workspace_id)
        .bind(event.id)
        .bind(&command.label)
        .bind(command.valid_from)
        .bind(command.valid_until)
        .bind(command.max_checkins)
        .bind(command.created_at)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "concert QR create_campaign insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO audit_events (
                workspace_id, actor_kind, action, target_type, target_id, request_id, metadata
            ) VALUES ($1, 'service', 'concert_qr.created', 'concert_qr_campaign', $2, $3, $4)
            "#,
        )
        .bind(command.workspace_id)
        .bind(campaign_id.to_string())
        .bind(&command.request_id)
        .bind(json!({
            "event_id": event.id,
            "event_slug": event.slug,
            "valid_from": command.valid_from,
            "valid_until": command.valid_until,
            "max_checkins": command.max_checkins,
        }))
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "concert QR create_campaign audit insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        tx.commit().await.map_err(|error| {
            tracing::warn!(%error, "concert QR create_campaign commit failed");
            ConcertQrError::Unavailable
        })?;

        Ok(CreateCampaignResult {
            campaign_id,
            event: ConcertEventInfo {
                id: event.id,
                slug: event.slug,
                title: event.title,
                venue: event.venue,
                starts_at: event.starts_at,
            },
            created_at: command.created_at,
        })
    }

    async fn revoke_campaign(&self, command: &RevokeCampaignCommand) -> Result<(), ConcertQrError> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            tracing::warn!(%error, "concert QR revoke_campaign begin failed");
            ConcertQrError::Unavailable
        })?;

        let updated = match sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE concert_qr_campaigns
            SET active = false, revoked_at = COALESCE(revoked_at, now())
            WHERE workspace_id = $1 AND id = $2
            RETURNING id
            "#,
        )
        .bind(command.workspace_id)
        .bind(command.campaign_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "concert QR revoke_campaign update failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        if updated.is_none() {
            return Err(ConcertQrError::NotFound);
        }

        if let Err(error) = sqlx::query(
            "INSERT INTO audit_events (workspace_id, actor_kind, action, target_type, target_id, request_id) VALUES ($1, 'service', 'concert_qr.revoked', 'concert_qr_campaign', $2, $3)",
        )
        .bind(command.workspace_id)
        .bind(command.campaign_id.to_string())
        .bind(&command.request_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "concert QR revoke_campaign audit insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        tx.commit().await.map_err(|error| {
            tracing::warn!(%error, "concert QR revoke_campaign commit failed");
            ConcertQrError::Unavailable
        })?;

        Ok(())
    }

    async fn check_in(&self, command: &CheckinCommand) -> Result<CheckinResult, ConcertQrError> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            tracing::warn!(%error, "concert QR check_in begin failed");
            ConcertQrError::Unavailable
        })?;

        let event = match sqlx::query_as::<_, EventRow>(
            "SELECT id, slug, title, venue, starts_at FROM events WHERE workspace_id = $1 AND slug = $2 AND id = $3 AND status = 'published' FOR SHARE",
        )
        .bind(command.workspace_id)
        .bind(&command.event_slug)
        .bind(command.event_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(ConcertQrError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in event lookup failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        let campaign = match sqlx::query_as::<_, LockedCampaignRow>(
            r#"
            SELECT id, event_id, valid_from, valid_until, max_checkins, active, revoked_at
            FROM concert_qr_campaigns
            WHERE workspace_id = $1 AND id = $2 AND event_id = $3
            FOR UPDATE
            "#,
        )
        .bind(command.workspace_id)
        .bind(command.campaign_id)
        .bind(event.id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(ConcertQrError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in campaign lock failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        if !campaign.active
            || campaign.revoked_at.is_some()
            || command.now < campaign.valid_from
            || command.now > campaign.valid_until
            || command.expires_at != campaign.valid_until.unix_timestamp()
            || campaign.event_id != event.id
        {
            return Err(ConcertQrError::NotFound);
        }

        // Resolve the fan from the session token, bumping last_seen_at.
        let fan_id = match sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE fan_sessions
            SET last_seen_at = now()
            WHERE workspace_id = $1
              AND session_token_hash = digest($2, 'sha256')
              AND revoked_at IS NULL
              AND expires_at > now()
            RETURNING fan_id
            "#,
        )
        .bind(command.workspace_id)
        .bind(&command.session_token)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return Err(ConcertQrError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in fan resolve failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        // Serialize all check-ins for one fan before testing the unique
        // (workspace, event, fan) invariant. This keeps retries idempotent even
        // when two independently issued campaign QR codes are scanned at once.
        match sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM fans WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(command.workspace_id)
        .bind(fan_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(_)) => {}
            Ok(None) => return Err(ConcertQrError::NotFound),
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in fan lock failed");
                return Err(ConcertQrError::Unavailable);
            }
        }

        let existing = match sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
            "SELECT campaign_id, checked_in_at FROM concert_checkins WHERE workspace_id = $1 AND event_id = $2 AND fan_id = $3",
        )
        .bind(command.workspace_id)
        .bind(event.id)
        .bind(fan_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in existing lookup failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        if let Some((existing_campaign, checked_in_at)) = existing {
            tx.commit().await.map_err(|error| {
                tracing::warn!(%error, "concert QR check_in idempotent commit failed");
                ConcertQrError::Unavailable
            })?;
            return Ok(CheckinResult {
                event_id: event.id,
                event_slug: event.slug,
                campaign_id: existing_campaign,
                created: false,
                checked_in_at,
            });
        }

        if let Some(max_checkins) = campaign.max_checkins {
            let count = match sqlx::query_scalar::<_, i64>(
                "SELECT count(*)::bigint FROM concert_checkins WHERE workspace_id = $1 AND campaign_id = $2",
            )
            .bind(command.workspace_id)
            .bind(campaign.id)
            .fetch_one(&mut *tx)
            .await
            {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(%error, "concert QR check_in count failed");
                    return Err(ConcertQrError::Unavailable);
                }
            };
            if count >= i64::from(max_checkins) {
                return Err(ConcertQrError::Conflict);
            }
        }

        let checkin_id = Uuid::now_v7();
        let checked_in_at = command.now;

        if let Err(error) = sqlx::query(
            "INSERT INTO concert_checkins (id, workspace_id, event_id, campaign_id, fan_id, checked_in_at, request_id) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(checkin_id)
        .bind(command.workspace_id)
        .bind(event.id)
        .bind(campaign.id)
        .bind(fan_id)
        .bind(checked_in_at)
        .bind(&command.request_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "concert QR check_in insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        // A venue check-in is stronger evidence than a simple interest click, so
        // it also enrolls the fan in any event-scoped ticket draw idempotently.
        if let Err(error) = sqlx::query(
            "INSERT INTO event_interests (workspace_id, event_id, fan_id, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT (workspace_id, event_id, fan_id) DO NOTHING",
        )
        .bind(command.workspace_id)
        .bind(event.id)
        .bind(fan_id)
        .bind(checked_in_at)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "concert QR check_in event_interests insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
            VALUES ($1, 'concert.checked_in', 1, $2, $3)
            "#,
        )
        .bind(command.workspace_id)
        .bind(json!({
            "checkin_id": checkin_id,
            "campaign_id": campaign.id,
            "event_id": event.id,
            "event_slug": event.slug,
            "fan_id": fan_id,
            "checked_in_at": checked_in_at,
        }))
        .bind(&command.request_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "concert QR check_in outbox insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        tx.commit().await.map_err(|error| {
            tracing::warn!(%error, "concert QR check_in commit failed");
            ConcertQrError::Unavailable
        })?;

        Ok(CheckinResult {
            event_id: event.id,
            event_slug: event.slug,
            campaign_id: campaign.id,
            created: true,
            checked_in_at,
        })
    }
}
