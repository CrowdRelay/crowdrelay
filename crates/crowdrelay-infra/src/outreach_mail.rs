//! The outreach ledger, kept current from the connected mailbox.
//!
//! The act writes to press, radio, venues and agents from its own Gmail, and
//! they answer there. The ledger (`outreach_interactions`) used to learn about
//! either only from a sheet import or from a person pressing "I wrote back",
//! so "whose turn is it" was as stale as the last import: on 2026-09-25 the
//! console said sixteen contacts were waiting on the act while the mailbox
//! was the only place that knew whether they still were.
//!
//! A mail touch is one message between the mailbox and one outreach contact:
//! outbound when the mailbox sent it, inbound when the contact did. It lands
//! in the ledger once per message and contact (`source_key = gmail:{id}`), and
//! moves the contact's own clocks forward only: an old message read late must
//! not make a contact look more recently written to than it was.
//!
//! What a touch deliberately does not do:
//! - classify an answer: an inbound message is `received`, never a guess at
//!   yes or no, and a contact already marked positive, declined or
//!   do-not-contact keeps that verdict;
//! - read a body: the mailbox scan reads headers only.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Which way one message went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailDirection {
    /// The mailbox sent it: the act wrote to the contact.
    Outbound,
    /// The contact sent it: they answered, or wrote first.
    Inbound,
}

/// One message between the mailbox and the contacts it names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailTouch {
    pub message_id: String,
    pub direction: MailDirection,
    /// Normalised addresses on the other side: every recipient of an
    /// outbound message, the sender of an inbound one.
    pub counterparts: Vec<String>,
    pub at: OffsetDateTime,
}

/// Records a touch against every outreach contact whose address it names.
/// Returns how many ledger rows were written; zero for a message that names
/// no contact, or one already recorded.
///
/// # Errors
///
/// Any database error.
pub async fn record_mail_touch(
    pool: &PgPool,
    workspace_id: Uuid,
    touch: &MailTouch,
) -> Result<u64, sqlx::Error> {
    if touch.counterparts.is_empty() {
        return Ok(0);
    }
    let source_key = format!("gmail:{}", touch.message_id);
    let mut transaction = pool.begin().await?;
    let touched: Vec<Uuid> = match touch.direction {
        MailDirection::Outbound => {
            sqlx::query_scalar::<_, Uuid>(
                r#"
                WITH contacts AS (
                    SELECT target.id
                    FROM outreach_targets AS target
                    WHERE target.workspace_id = $1
                      AND lower(btrim(target.contact_email)) = ANY($2::text[])
                ), written AS (
                    INSERT INTO outreach_interactions (
                        workspace_id, target_id, direction, phase, disposition,
                        source_key, occurred_at, metadata
                    )
                    SELECT $1, contacts.id, 'outbound',
                           CASE WHEN EXISTS (
                               SELECT 1 FROM outreach_interactions AS earlier
                               WHERE earlier.workspace_id = $1
                                 AND earlier.target_id = contacts.id
                                 AND earlier.direction = 'outbound'
                                 AND earlier.occurred_at < $4
                           ) THEN 'followup' ELSE 'initial' END,
                           'none', $3, $4, jsonb_build_object('source', 'gmail')
                    FROM contacts
                    ON CONFLICT (workspace_id, target_id, source_key) DO NOTHING
                    RETURNING target_id
                )
                UPDATE outreach_targets AS target
                SET last_outreach_at = GREATEST(COALESCE(target.last_outreach_at, $4), $4),
                    version = target.version + 1,
                    updated_at = now()
                FROM written
                WHERE target.workspace_id = $1 AND target.id = written.target_id
                RETURNING target.id
                "#,
            )
            .bind(workspace_id)
            .bind(&touch.counterparts)
            .bind(&source_key)
            .bind(touch.at)
            .fetch_all(&mut *transaction)
            .await?
        }
        MailDirection::Inbound => {
            sqlx::query_scalar::<_, Uuid>(
                r#"
                WITH contacts AS (
                    SELECT target.id
                    FROM outreach_targets AS target
                    WHERE target.workspace_id = $1
                      AND lower(btrim(target.contact_email)) = ANY($2::text[])
                ), written AS (
                    INSERT INTO outreach_interactions (
                        workspace_id, target_id, direction, phase, disposition,
                        source_key, occurred_at, metadata
                    )
                    SELECT $1, contacts.id, 'inbound', 'reply', 'received',
                           $3, $4, jsonb_build_object('source', 'gmail')
                    FROM contacts
                    ON CONFLICT (workspace_id, target_id, source_key) DO NOTHING
                    RETURNING target_id
                )
                UPDATE outreach_targets AS target
                SET last_reply_at = GREATEST(COALESCE(target.last_reply_at, $4), $4),
                    -- A person answered, so the contact is served. A verdict
                    -- someone already recorded is not overwritten by a receipt.
                    last_reply_disposition = CASE
                        WHEN COALESCE(target.last_reply_disposition::text, 'none') = 'none'
                        THEN 'received'
                        ELSE target.last_reply_disposition
                    END,
                    version = target.version + 1,
                    updated_at = now()
                FROM written
                WHERE target.workspace_id = $1 AND target.id = written.target_id
                RETURNING target.id
                "#,
            )
            .bind(workspace_id)
            .bind(&touch.counterparts)
            .bind(&source_key)
            .bind(touch.at)
            .fetch_all(&mut *transaction)
            .await?
        }
    };
    if !touched.is_empty() {
        sqlx::query(
            r#"
            INSERT INTO outreach_target_history (workspace_id, target_id, version, snapshot)
            SELECT workspace_id, id, version, jsonb_build_object(
                'target_kind', target_kind,
                'display_name', display_name,
                'contact_email', contact_email,
                'active', active,
                'verified', verified,
                'accepts_outreach', accepts_outreach,
                'priority', priority,
                'relationship_score', relationship_score,
                'do_not_contact', do_not_contact,
                'last_outreach_at', last_outreach_at,
                'last_reply_at', last_reply_at,
                'last_reply_disposition', last_reply_disposition
            )
            FROM outreach_targets
            WHERE workspace_id = $1 AND id = ANY($2)
            "#,
        )
        .bind(workspace_id)
        .bind(&touched)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(u64::try_from(touched.len()).unwrap_or(u64::MAX))
}

/// Every outreach contact with an address, as `(id, normalised address)`.
/// The reconciler picks its batch from these.
///
/// # Errors
///
/// Any database error.
pub async fn outreach_addresses(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, lower(btrim(contact_email))
        FROM outreach_targets
        WHERE workspace_id = $1
          AND contact_email IS NOT NULL
          AND position('@' IN contact_email) > 1
        ORDER BY id
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
}

/// Whether the ledger already holds this message for any contact — the
/// reconciler skips fetching a message it has recorded.
///
/// # Errors
///
/// Any database error.
pub async fn message_recorded(
    pool: &PgPool,
    workspace_id: Uuid,
    message_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM outreach_interactions WHERE workspace_id = $1 AND source_key = $2)",
    )
    .bind(workspace_id)
    .bind(format!("gmail:{message_id}"))
    .fetch_one(pool)
    .await
}
