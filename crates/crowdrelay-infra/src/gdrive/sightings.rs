//! Mailbox sightings — the direction-aware half of the contact scan.
//!
//! `last_seen_at` on a contact means "an address appeared in a header
//! somewhere". The sighting pair means something sharper: they wrote to
//! the band (`record_inbound_sighting`), or the band wrote to them
//! (`record_outbound_sighting`). The second also lands an `outbound`
//! interaction row on whichever registry the address belongs to — the
//! console's "waiting on a reply" reads those tables, so an answer the
//! act sent from Gmail must exist there or the conversation keeps asking
//! for a reply that already went out.

use super::*;

impl PostgresGDriveRepository {
    /// The moment this address last wrote to the band's mailbox — message
    /// `From` = the address, `internalDate` of the message. `last_seen_at`
    /// is any sighting in any header; this is direction. Monotonic: an
    /// earlier timestamp never lowers the stored one.
    pub async fn record_inbound_sighting(
        &self,
        workspace_id: Uuid,
        normalized_email: &str,
        at: time::OffsetDateTime,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            r#"
            UPDATE drive_contacts
            SET last_inbound_at = GREATEST(COALESCE(last_inbound_at, $3), $3),
                updated_at = now()
            WHERE workspace_id = $1 AND normalized_email = $2
            "#,
        )
        .bind(workspace_id)
        .bind(normalized_email)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The mirror of [`Self::record_inbound_sighting`]: the tenant's own
    /// mailbox reached this address at `at` — message `From` = the band,
    /// `internalDate` of the message. Where the address names a known
    /// outreach or booking contact, an `outbound` interaction row lands
    /// too — the conversation lists, the unanswered-reply reads and the
    /// funnel all answer "did we write back" from those tables, so a reply
    /// the act sent from Gmail has to exist there or it stays invisible.
    ///
    /// `source_key` carries the Gmail message id (`gmail:{id}`), so a
    /// rescan of the same message is a no-op under the unique constraint.
    /// `last_outreach_at` moves forward only. `outreach_targets.version`
    /// is deliberately *not* bumped: the version guards in-flight waves,
    /// and a mailbox sighting must not fail a send that locked the row.
    /// A do-not-contact outreach target records nothing.
    pub async fn record_outbound_sighting(
        &self,
        workspace_id: Uuid,
        normalized_email: &str,
        at: time::OffsetDateTime,
        source_key: &str,
    ) -> Result<(), GDriveError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            r#"
            WITH target AS (
                SELECT id FROM outreach_targets
                WHERE workspace_id = $1
                  AND lower(btrim(contact_email)) = $2
                  AND NOT do_not_contact
            ), bump AS (
                UPDATE outreach_targets
                SET last_outreach_at = GREATEST(COALESCE(last_outreach_at, $3), $3),
                    updated_at = now()
                FROM target
                WHERE outreach_targets.workspace_id = $1
                  AND outreach_targets.id = target.id
            )
            INSERT INTO outreach_interactions (
                workspace_id, target_id, direction, phase, disposition,
                source_key, occurred_at, metadata
            )
            SELECT $1, target.id, 'outbound',
                   CASE WHEN EXISTS (
                       SELECT 1 FROM outreach_interactions prior
                       WHERE prior.workspace_id = $1 AND prior.target_id = target.id
                         AND prior.direction = 'outbound'
                   ) THEN 'followup' ELSE 'initial' END,
                   'none', $4, $3, jsonb_build_object('channel', 'gmail')
            FROM target
            ON CONFLICT (workspace_id, target_id, source_key) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(normalized_email)
        .bind(at)
        .bind(source_key)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"
            WITH target AS (
                SELECT id FROM booking_targets
                WHERE workspace_id = $1
                  AND lower(btrim(contact_email)) = $2
            ), bump AS (
                UPDATE booking_targets
                SET last_outreach_at = GREATEST(COALESCE(last_outreach_at, $3), $3),
                    updated_at = now()
                FROM target
                WHERE booking_targets.workspace_id = $1
                  AND booking_targets.id = target.id
            )
            INSERT INTO booking_interactions (
                workspace_id, target_id, direction, phase, disposition,
                source_key, occurred_at, metadata
            )
            SELECT $1, target.id, 'outbound',
                   CASE WHEN EXISTS (
                       SELECT 1 FROM booking_interactions prior
                       WHERE prior.workspace_id = $1 AND prior.target_id = target.id
                         AND prior.direction = 'outbound'
                   ) THEN 'followup' ELSE 'initial' END,
                   'none', $4, $3, jsonb_build_object('channel', 'gmail')
            FROM target
            ON CONFLICT (workspace_id, target_id, source_key) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(normalized_email)
        .bind(at)
        .bind(source_key)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}
