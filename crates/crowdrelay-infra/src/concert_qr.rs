//! PostgreSQL persistence for concert QR campaigns and check-ins.
//!
//! The API layer retains token signing/verification, input validation and
//! response formatting. This adapter owns the durable write transactions:
//! campaign creation, revocation and idempotent fan check-in.

use async_trait::async_trait;
use crowdrelay_application::{
    CheckinCommand, CheckinIdentity, CheckinResult, ConcertEventInfo, ConcertQrError,
    ConcertQrRepository, CreateCampaignCommand, CreateCampaignResult, RevokeCampaignCommand,
    UpdateCampaignContextCommand,
};
use crowdrelay_domain::{FanId, WorkspaceId};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::fan_lifecycle::{issue_confirmation_token, issue_fan_action_token};

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
    city_id: Option<Uuid>,
    timezone: String,
}

/// How the checking-in fan resolved inside the transaction.
enum FanResolution {
    /// Live session cookie — verified identity.
    Session(Uuid),
    /// Email claim — the fan row behind the address plus its status, which
    /// decides the follow-up email (confirm, sign in, or none).
    EmailClaim { fan_id: Uuid, status: String },
}

impl FanResolution {
    fn fan_id(&self) -> Uuid {
        match *self {
            Self::Session(fan_id) | Self::EmailClaim { fan_id, .. } => fan_id,
        }
    }
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
                SELECT id, slug, title, venue, starts_at, city_id, timezone
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
                max_checkins, placement, announced_from_stage, incentive,
                created_at, updated_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11)
            "#,
        )
        .bind(campaign_id)
        .bind(command.workspace_id)
        .bind(event.id)
        .bind(&command.label)
        .bind(command.valid_from)
        .bind(command.valid_until)
        .bind(command.max_checkins)
        .bind(&command.placement)
        .bind(command.announced_from_stage)
        .bind(&command.incentive)
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

    async fn update_campaign_context(
        &self,
        command: &UpdateCampaignContextCommand,
    ) -> Result<(), ConcertQrError> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            tracing::warn!(%error, "concert QR update_campaign_context begin failed");
            ConcertQrError::Unavailable
        })?;

        let updated = match sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE concert_qr_campaigns
            SET placement = $3,
                announced_from_stage = $4,
                incentive = $5,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND revoked_at IS NULL
            RETURNING id
            "#,
        )
        .bind(command.workspace_id)
        .bind(command.campaign_id)
        .bind(&command.placement)
        .bind(command.announced_from_stage)
        .bind(&command.incentive)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "concert QR update_campaign_context update failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        if updated.is_none() {
            return Err(ConcertQrError::NotFound);
        }

        if let Err(error) = sqlx::query(
            "INSERT INTO audit_events (workspace_id, actor_kind, action, target_type, target_id, request_id, metadata) VALUES ($1, 'service', 'concert_qr.context_updated', 'concert_qr_campaign', $2, $3, $4)",
        )
        .bind(command.workspace_id)
        .bind(command.campaign_id.to_string())
        .bind(&command.request_id)
        .bind(json!({
            "placement": command.placement,
            "announced_from_stage": command.announced_from_stage,
            "incentive": command.incentive,
        }))
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "concert QR update_campaign_context audit insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        tx.commit().await.map_err(|error| {
            tracing::warn!(%error, "concert QR update_campaign_context commit failed");
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
            "SELECT id, slug, title, venue, starts_at, city_id, timezone FROM events WHERE workspace_id = $1 AND slug = $2 AND id = $3 AND status = 'published' FOR SHARE",
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

        let resolution = if let Some(session_token) = command.session_token.as_ref() {
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
            .bind(session_token)
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
            FanResolution::Session(fan_id)
        } else {
            self.resolve_email_claim(&mut tx, command).await?
        };
        let fan_id = resolution.fan_id();

        let existing = match sqlx::query_as::<_, (Uuid, OffsetDateTime, String)>(
            "SELECT campaign_id, checked_in_at, identity_source FROM concert_checkins WHERE workspace_id = $1 AND event_id = $2 AND fan_id = $3",
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

        if let Some((existing_campaign, checked_in_at, stored_source)) = existing {
            tx.commit().await.map_err(|error| {
                tracing::warn!(%error, "concert QR check_in idempotent commit failed");
                ConcertQrError::Unavailable
            })?;
            // A rescan must report the provenance of the row that actually
            // exists, not how this request happened to identify itself.
            let identity = match stored_source.as_str() {
                "email_claim" => CheckinIdentity::EmailClaim,
                _ => CheckinIdentity::Session,
            };
            return Ok(CheckinResult {
                event_id: event.id,
                event_slug: event.slug,
                campaign_id: existing_campaign,
                created: false,
                checked_in_at,
                identity,
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
        let identity = match &resolution {
            FanResolution::Session(_) => CheckinIdentity::Session,
            FanResolution::EmailClaim { .. } => CheckinIdentity::EmailClaim,
        };

        if let Err(error) = sqlx::query(
            "INSERT INTO concert_checkins (id, workspace_id, event_id, campaign_id, fan_id, checked_in_at, request_id, identity_source) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(checkin_id)
        .bind(command.workspace_id)
        .bind(event.id)
        .bind(campaign.id)
        .bind(fan_id)
        .bind(checked_in_at)
        .bind(&command.request_id)
        .bind(identity.as_str())
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

        // Identity spine, §4e-5: a paid ticket order for this same event
        // carrying a *different* fan's buyer email is the honest signal that
        // the checker and the buyer may be one person. It parks a merge
        // candidate for a human decision — never an automatic merge.
        match sqlx::query_scalar::<_, Uuid>(
            "SELECT DISTINCT i.fan_id FROM ticket_orders o \
             JOIN ticket_sales s ON s.workspace_id = o.workspace_id \
                 AND s.id = o.ticket_sale_id \
             JOIN fan_identifiers i ON i.workspace_id = o.workspace_id \
                 AND i.kind = 'email' AND i.value = o.buyer_email \
             WHERE o.workspace_id = $1 AND s.event_id = $2 \
                 AND o.status IN ('paid', 'partially_refunded') \
                 AND i.fan_id <> $3",
        )
        .bind(command.workspace_id)
        .bind(event.id)
        .bind(fan_id)
        .fetch_all(&mut *tx)
        .await
        {
            Ok(buyer_fan_ids) => {
                for buyer_fan_id in buyer_fan_ids {
                    if let Err(error) = crate::fan_identity::record_merge_candidate_in_tx(
                        &mut tx,
                        command.workspace_id,
                        fan_id,
                        buyer_fan_id,
                        json!({
                            "kind": "order_email_vs_checkin",
                            "event_id": event.id,
                            "event_slug": event.slug,
                            "checkin_id": checkin_id,
                        }),
                    )
                    .await
                    {
                        tracing::warn!(%error, "concert QR check_in merge candidate failed");
                        return Err(ConcertQrError::Unavailable);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in buyer identity lookup failed");
                return Err(ConcertQrError::Unavailable);
            }
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

        if let FanResolution::EmailClaim { status, .. } = &resolution {
            self.finish_email_claim(&mut tx, command, &event, fan_id, status)
                .await?;
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
            identity,
        })
    }
}

impl PostgresConcertQrRepository {
    /// Upsert the fan behind an email claim, holding the row lock so the rest
    /// of the check-in serializes exactly like the session path.
    ///
    /// A scan always lands `pending`: the address is unverified no matter what
    /// the workspace's signup policy is, and an unverified claim must not
    /// inherit `active` reachability. Suppressed rows surface their status so
    /// the caller can record the attendance fact while skipping every email.
    async fn resolve_email_claim(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &CheckinCommand,
    ) -> Result<FanResolution, ConcertQrError> {
        let Some(email) = command.email.as_ref() else {
            // The HTTP layer requires an email before this branch is reached;
            // a missing one here is a contract violation, not a panic.
            return Err(ConcertQrError::Invalid);
        };
        // The identity spine resolves first: an address a merge moved now
        // belongs to the survivor, so the claim must land on them rather than
        // resurrect the tombstone or double-count the person.
        match crate::fan_identity::resolve_fan_for_email(tx, command.workspace_id, email).await {
            Ok(Some((fan_id, status))) => {
                return Ok(FanResolution::EmailClaim { fan_id, status });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in identity resolution failed");
                return Err(ConcertQrError::Unavailable);
            }
        }
        let inserted = match sqlx::query_as::<_, (Uuid, String)>(
            r#"
            INSERT INTO fans (workspace_id, normalized_email, status)
            VALUES ($1, $2, 'pending')
            ON CONFLICT (workspace_id, normalized_email) DO NOTHING
            RETURNING id, status
            "#,
        )
        .bind(command.workspace_id)
        .bind(email)
        .fetch_optional(&mut **tx)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "concert QR check_in fan upsert failed");
                return Err(ConcertQrError::Unavailable);
            }
        };

        let (fan_id, status) = match inserted {
            Some((id, status)) => {
                // The fan INSERT created a row — this scan is how they
                // arrived. A conflict would mean they already have
                // provenance from wherever they first showed up.
                let request_id = command
                    .request_id
                    .clone()
                    .unwrap_or_else(|| format!("concert_qr:{}", command.campaign_id));
                crate::acquisition::record_fan_arrival(
                    tx,
                    WorkspaceId::from_uuid(command.workspace_id),
                    FanId::from_uuid(id),
                    "concert_qr",
                    &request_id,
                )
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "concert QR check_in arrival record failed");
                    ConcertQrError::Unavailable
                })?;
                (id, status)
            }
            None => match sqlx::query_as::<_, (Uuid, String)>(
                "SELECT id, status FROM fans WHERE workspace_id = $1 AND normalized_email = $2 FOR UPDATE",
            )
            .bind(command.workspace_id)
            .bind(email)
            .fetch_optional(&mut **tx)
            .await
            {
                Ok(Some(row)) => row,
                Ok(None) => {
                    // The upsert and its fallback disagree about reality, which
                    // is a store fault, not a caller fault.
                    tracing::error!("concert QR check_in fan upsert returned no row");
                    return Err(ConcertQrError::Unavailable);
                }
                Err(error) => {
                    tracing::warn!(%error, "concert QR check_in fan lookup failed");
                    return Err(ConcertQrError::Unavailable);
                }
            },
        };

        Ok(FanResolution::EmailClaim { fan_id, status })
    }

    /// Consent, city interest and the inbox follow-up for an email-claim
    /// check-in — only ever reached after the check-in row itself committed,
    /// so a rescan dedupes before any of this can repeat.
    ///
    /// The follow-up matches the signup contract exactly: pending fans get a
    /// `fan.confirmation_requested` token, active and unsubscribed ones get
    /// `fan.session_requested` — sign-in is transactional mail an unsubscribe
    /// does not block. Suppressed means do-not-contact: the attendance fact
    /// stands and nothing is sent.
    async fn finish_email_claim(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &CheckinCommand,
        event: &EventRow,
        fan_id: Uuid,
        status: &str,
    ) -> Result<(), ConcertQrError> {
        if let Some(consent) = command.consent.as_ref()
            && let Err(error) = sqlx::query(
                r#"
                INSERT INTO fan_consents (
                    workspace_id, fan_id, purpose, granted, policy_version,
                    source, request_id
                )
                VALUES ($1, $2, 'marketing', $3, $4, 'concert_checkin', $5)
                "#,
            )
            .bind(command.workspace_id)
            .bind(fan_id)
            .bind(consent.granted)
            .bind(&consent.policy_version)
            .bind(&command.request_id)
            .execute(&mut **tx)
            .await
        {
            tracing::warn!(%error, "concert QR check_in consent insert failed");
            return Err(ConcertQrError::Unavailable);
        }

        if let Some(city_id) = event.city_id
            && let Err(error) = sqlx::query(
                "INSERT INTO fan_city_interests (workspace_id, fan_id, city_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(command.workspace_id)
            .bind(fan_id)
            .bind(city_id)
            .execute(&mut **tx)
            .await
        {
            tracing::warn!(%error, "concert QR check_in city interest failed");
            return Err(ConcertQrError::Unavailable);
        }

        let workspace = WorkspaceId::from_uuid(command.workspace_id);
        let fan = FanId::from_uuid(fan_id);
        // The scan's welcome doubles as the T+1 recall: one contact carrying
        // the night's context, so the follow-ask lever must not send a second
        // message to the same fan inside the same window.
        let mut payload = json!({
            "workspace_id": command.workspace_id,
            "fan_id": fan_id,
            "email": command.email,
            "display_name": null,
            "locale": null,
            "city_slug": null,
            "event_id": event.id,
            "event_slug": event.slug,
            "event_title": event.title,
            "venue": event.venue,
            "event_starts_at": event.starts_at,
            "source": "concert_checkin",
        });
        let event_type = match status {
            "pending" => {
                let token = match issue_confirmation_token(tx, workspace, fan).await {
                    Ok(token) => token,
                    Err(error) => {
                        tracing::warn!(error = %error, "concert QR check_in confirmation token failed");
                        return Err(ConcertQrError::Unavailable);
                    }
                };
                if let Some(map) = payload.as_object_mut() {
                    map.insert("confirmation_token".to_owned(), json!(token.as_str()));
                    map.insert(
                        "policy_version".to_owned(),
                        command
                            .consent
                            .as_ref()
                            .map_or(serde_json::Value::Null, |consent| {
                                json!(consent.policy_version)
                            }),
                    );
                }
                "fan.confirmation_requested"
            }
            "active" | "unsubscribed" => {
                let token = match issue_fan_action_token(tx, workspace, fan, "session", 2).await {
                    Ok(token) => token,
                    Err(error) => {
                        tracing::warn!(error = %error, "concert QR check_in session token failed");
                        return Err(ConcertQrError::Unavailable);
                    }
                };
                if let Some(map) = payload.as_object_mut() {
                    map.insert("session_recovery_token".to_owned(), json!(token.as_str()));
                }
                "fan.session_requested"
            }
            _ => return Ok(()),
        };

        // One contact, scheduled next morning in the event's timezone: the
        // scan lands late at night, and a confirm/sign-in mail sent at 23:40
        // competes with the night itself. The rule is "the next 10:00 local
        // that is still ahead" — a post-midnight scan lands at 10:00 that same
        // morning, while a daytime or post-10:00 scan lands at 10:00 tomorrow.
        // Without the `> now() + 1 hour` guard a scan between 10:00 and 18:00
        // local produced an `available_at` in the past, and the "next morning"
        // promise silently became "right now".
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id,
                available_at
            )
            VALUES (
                $1, $2, 1, $3, $4,
                CASE
                    WHEN (date_trunc('day', now() AT TIME ZONE $5)
                          + interval '10 hours') AT TIME ZONE $5
                         > now() + interval '1 hour'
                    THEN (date_trunc('day', now() AT TIME ZONE $5)
                          + interval '10 hours') AT TIME ZONE $5
                    ELSE (date_trunc('day', now() AT TIME ZONE $5)
                          + interval '34 hours') AT TIME ZONE $5
                END
            )
            "#,
        )
        .bind(command.workspace_id)
        .bind(event_type)
        .bind(payload)
        .bind(&command.request_id)
        .bind(&event.timezone)
        .execute(&mut **tx)
        .await
        {
            tracing::warn!(%error, "concert QR check_in follow-up outbox failed");
            return Err(ConcertQrError::Unavailable);
        }

        Ok(())
    }
}
