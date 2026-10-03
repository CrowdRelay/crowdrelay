//! Fan privacy / account-erasure persistence.
//!
//! Consent records and paid-commerce evidence are intentionally retained, so a
//! fan is converted into an unlinkable tombstone rather than hard-deleted. All
//! authentication, push, public leaderboard and game identity links are removed
//! atomically in the same transaction.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone)]
pub struct PostgresFanPrivacyRepository {
    pool: PgPool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FanPrivacyError {
    Unauthorized,
    Unexpected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FanErasureReceipt {
    pub fan_id: Uuid,
}

pub struct SynesthesiaLeaderboardUnpublishReceipt {
    pub fan_id: Uuid,
    pub changed: bool,
}

impl PostgresFanPrivacyRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn unexpected(error: sqlx::Error) -> FanPrivacyError {
        tracing::error!(error = %error, "fan account erasure persistence failed");
        FanPrivacyError::Unexpected
    }

    pub async fn erase_account(
        &self,
        workspace_id: Uuid,
        session_token: &str,
        request_id: Option<&str>,
    ) -> Result<FanErasureReceipt, FanPrivacyError> {
        let mut tx = self.pool.begin().await.map_err(Self::unexpected)?;
        let fan_id = Self::lock_current_fan(&mut tx, workspace_id, session_token).await?;
        let tombstone_email = format!("deleted-{fan_id}@account.invalid");
        Self::erase_contact_intents(&mut tx, workspace_id, fan_id).await?;

        // AREA persists the fan e-mail separately. Preserve collectible history,
        // but remove the account relationship and direct address.
        sqlx::query(
            r#"
            UPDATE area_ticket_rewards AS reward
            SET fan_email = $3, updated_at = now()
            FROM area_players AS player
            WHERE player.workspace_id = $1
              AND player.fan_id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
              AND reward.workspace_id = player.workspace_id
              AND reward.player_id = player.id
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .bind(&tombstone_email)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        sqlx::query(
            r#"
            UPDATE area_players
            SET normalized_email = format('deleted-%s@account.invalid', fan_id),
                fan_id = NULL,
                last_seen_at = now()
            WHERE workspace_id = $1
              AND fan_id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        // Acquisition/referral data is not required for accounting or consent
        // evidence. Erase the deleted fan from the marketing graph and make any
        // public referral code unusable. `fan_acquisition_events` is append-only
        // for normal product writes but deliberately permits DELETE for a
        // separately authorized privacy-erasure workflow.
        // Deactivate first. Referral resolution uses FOR SHARE on the code row,
        // so this UPDATE waits for any resolver that already observed the code
        // as active. Once we own the row and mark it inactive, no new resolver
        // can enter; the DELETEs below then remove every relationship committed
        // by the in-flight resolver before the lock was acquired.
        sqlx::query(
            r#"
            UPDATE referral_codes
            SET active = false
            WHERE workspace_id = $1
              AND fan_id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
              AND active
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        sqlx::query(
            r#"
            DELETE FROM fan_acquisition_events
            WHERE workspace_id = $1
              AND (
                  fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))
                  OR referrer_fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                  )
              )
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        sqlx::query(
            r#"
            DELETE FROM referral_attributions
            WHERE workspace_id = $1
              AND (
                  referrer_fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                  )
                  OR referred_fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                  )
              )
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        sqlx::query(
            "DELETE FROM fan_provenance_events
             WHERE workspace_id = $1
               AND fan_id IN (
                   SELECT fan_id FROM canonical_fan_family($1,$2)
               )",
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        // Draw entry e-mail is voluntary/non-transactional data. Deleting the
        // account forfeits pending Synesthesia draw participation and removes it.
        sqlx::query(
            "DELETE FROM synesthesia_reward_entries
         WHERE workspace_id = $1
           AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        // Keep anonymous gameplay history, but sever account identity and remove
        // every public leaderboard publication/handoff secret.
        sqlx::query(
            r#"
            UPDATE synesthesia_runs
            SET fan_id = NULL,
                linked_at = NULL,
                handoff_token_hash = NULL,
                handoff_expires_at = NULL,
                leaderboard_name = NULL,
                leaderboard_published_at = NULL
            WHERE workspace_id = $1
              AND fan_id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        // Non-paid passes may carry optional profile data and can be scrubbed.
        // Paid passes/order records are retained for ticket fulfilment/accounting.
        sqlx::query(
            r#"
            UPDATE admission_passes
            SET holder_name = NULL, holder_email = NULL, updated_at = now()
            WHERE workspace_id = $1
              AND fan_id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
              AND issuance_method <> 'paid'
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        // Kill every credential/contact surface before changing the canonical
        // identity. Push deliveries cascade from endpoints. The identity
        // spine holds the erased address and device link verbatim, so it
        // goes too — otherwise the erased person could never re-register
        // under their own address and staff views would keep serving PII.
        for statement in [
            "DELETE FROM fan_ad_attribution WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM fan_identifiers WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "UPDATE signal_installations SET fan_id = NULL WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM fan_push_endpoints WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM fan_action_tokens WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM fan_sessions WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM nearby_gig_notifications WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM fan_location_preferences WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM fan_city_interests WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM event_reminder_jobs WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM event_interests WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            "DELETE FROM fan_audience_tags WHERE workspace_id = $1 AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
            // The person layer: the public handle the band observed before this
            // fan joined, and every observation about it. Deleting the person
            // cascades its identities, prospects and evidence.
            "DELETE FROM persons WHERE workspace_id = $1 AND id IN (
                 SELECT person_id FROM fan_prospects
                 WHERE workspace_id = $1
                   AND linked_fan_id IN (
                       SELECT fan_id FROM canonical_fan_family($1,$2)
                   ))",
            // The same person reached through the fan's email identity: a
            // Latarnik role (and its frozen evidence) is erased with them.
            "DELETE FROM persons WHERE workspace_id = $1 AND id IN (
                 SELECT pi.person_id
                 FROM person_identities pi
                 JOIN fans f ON f.workspace_id = pi.workspace_id
                            AND f.normalized_email = pi.value
                 WHERE pi.workspace_id = $1
                   AND pi.kind = 'email'
                   AND f.id IN (
                       SELECT fan_id FROM canonical_fan_family($1,$2)
                   ))",
        ] {
            sqlx::query(statement)
                .bind(workspace_id)
                .bind(fan_id)
                .execute(&mut *tx)
                .await
                .map_err(Self::unexpected)?;
        }

        sqlx::query(
            r#"
            UPDATE fans
            SET normalized_email = format('deleted-%s@account.invalid', id),
                display_name = NULL,
                locale = NULL,
                deleted_at = now()
            WHERE workspace_id = $1
              AND id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        sqlx::query(
            "UPDATE fans
             SET status = 'suppressed'
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        sqlx::query(
            r#"
            INSERT INTO audit_events (
                workspace_id, actor_kind, action, target_type, target_id, request_id, metadata
            )
            VALUES (
                $1, 'service', 'fan.account_erased', 'fan', $2, $3,
                jsonb_build_object(
                    'identity_erased', true,
                    'acquisition_referral_erased', true,
                    'paid_commerce_retained', true,
                    'consent_evidence_retained', true
                )
            )
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id.to_string())
        .bind(request_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        tx.commit().await.map_err(Self::unexpected)?;
        Ok(FanErasureReceipt { fan_id })
    }

    pub async fn unpublish_synesthesia_leaderboard(
        &self,
        workspace_id: Uuid,
        session_token: &str,
        request_id: Option<&str>,
    ) -> Result<SynesthesiaLeaderboardUnpublishReceipt, FanPrivacyError> {
        let mut tx = self.pool.begin().await.map_err(Self::unexpected)?;
        let fan_id = Self::lock_current_fan(&mut tx, workspace_id, session_token).await?;
        // Every campaign, not one. The filter used to name the first tenant's
        // album campaign, so on any other tenant — or any later campaign —
        // "take my name off the leaderboard" matched nothing and reported
        // success. A fan asking to be unlisted means unlisted everywhere; the
        // audit row records which campaigns that touched.
        let mut campaigns = sqlx::query_scalar::<_, String>(
            r#"
            UPDATE synesthesia_runs
            SET leaderboard_name = NULL,
                leaderboard_published_at = NULL,
                updated_at = now()
            WHERE workspace_id = $1
              AND fan_id = $2
              AND leaderboard_name IS NOT NULL
            RETURNING campaign_slug
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(Self::unexpected)?;
        campaigns.sort();
        campaigns.dedup();
        let changed = !campaigns.is_empty();

        if changed {
            // `actor_kind` is 'service' like the erasure audit above: the
            // column's CHECK allows member, system and service, and the 'fan'
            // this used to write violated it — rolling back the unpublish, so
            // a fan asking to take their name off the leaderboard never could.
            sqlx::query(
                r#"
                INSERT INTO audit_events (
                    workspace_id, actor_kind, action, target_type, target_id, request_id, metadata
                )
                VALUES (
                    $1, 'service', 'synesthesia.leaderboard_unpublished', 'fan', $2, $3,
                    jsonb_build_object('campaign_slugs', $4::text[])
                )
                "#,
            )
            .bind(workspace_id)
            .bind(fan_id.to_string())
            .bind(request_id)
            .bind(&campaigns)
            .execute(&mut *tx)
            .await
            .map_err(Self::unexpected)?;
        }

        tx.commit().await.map_err(Self::unexpected)?;
        Ok(SynesthesiaLeaderboardUnpublishReceipt { fan_id, changed })
    }

    async fn erase_contact_intents(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        fan_id: Uuid,
    ) -> Result<(), FanPrivacyError> {
        let event_ids = sqlx::query_scalar::<_, Uuid>(r#"
            UPDATE outbox_events AS event
            SET payload = jsonb_build_object('fan_id', $2::uuid::text, 'identity_erased', true),
                status = CASE WHEN event.status IN ('pending','processing') THEN 'dead' ELSE event.status END,
                dead_at = CASE WHEN event.status IN ('pending','processing') THEN now() ELSE event.dead_at END,
                locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
                last_error_kind = 'account_erased'
            WHERE event.workspace_id = $1 AND event.event_type NOT LIKE 'ticket.%'
              AND (
                  event.payload->>'fan_id' IN (
                      SELECT fan_id::text FROM canonical_fan_family($1,$2)
                  )
                  OR event.payload#>>'{fan,id}' IN (
                      SELECT fan_id::text FROM canonical_fan_family($1,$2)
                  )
                  OR event.payload->>'email' IN (
                      SELECT normalized_email
                      FROM fans
                      WHERE workspace_id=$1
                        AND id IN (
                            SELECT fan_id FROM canonical_fan_family($1,$2)
                        )
                  )
              )
            RETURNING event.id
        "#).bind(workspace_id).bind(fan_id).fetch_all(&mut **tx).await.map_err(Self::unexpected)?;
        sqlx::query(r#"
            UPDATE webhook_deliveries
            SET status = CASE WHEN status IN ('pending','processing') THEN 'cancelled' ELSE status END,
                cancelled_at = CASE WHEN status IN ('pending','processing') THEN now() ELSE cancelled_at END,
                locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
                response_excerpt = NULL, last_error_kind = 'account_erased'
            WHERE workspace_id=$1 AND outbox_event_id = ANY($2)
        "#).bind(workspace_id).bind(&event_ids).execute(&mut **tx).await.map_err(Self::unexpected)?;
        Ok(())
    }

    async fn lock_current_fan(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        session_token: &str,
    ) -> Result<Uuid, FanPrivacyError> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT fan.id
            FROM fan_sessions AS session
            INNER JOIN fans AS fan
              ON fan.workspace_id = session.workspace_id
             AND fan.id = session.fan_id
            WHERE session.workspace_id = $1
              AND session.session_token_hash = digest($2, 'sha256')
              AND session.revoked_at IS NULL
              AND session.expires_at > now()
              AND fan.status = 'active'
              AND fan.deleted_at IS NULL
            LIMIT 1
            FOR UPDATE OF fan
            "#,
        )
        .bind(workspace_id)
        .bind(session_token)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Self::unexpected)?
        .ok_or(FanPrivacyError::Unauthorized)
    }
}
