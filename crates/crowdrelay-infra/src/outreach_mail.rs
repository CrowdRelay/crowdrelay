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

/// An outreach contact whose reply is worth reading: the sender is a target
/// the workspace mailed within the last `days` days. Carries the disposition
/// the ledger holds for the target at read time — the "previous" a later
/// reply classification records.
#[derive(Debug)]
pub struct PitchedReplyTarget {
    pub target_id: Uuid,
    pub target_kind: String,
    pub disposition: Option<String>,
}

/// Which of `counterparts` (lowercase-normalised addresses) are outreach
/// targets this workspace actually pitched within `days` days. An inbound
/// message from anyone else is mail, not a reply — the caller must not fetch
/// its body.
///
/// # Errors
///
/// Any database error.
pub async fn pitched_reply_targets(
    pool: &PgPool,
    workspace_id: Uuid,
    counterparts: &[String],
    days: i32,
) -> Result<Vec<PitchedReplyTarget>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, String, Option<String>)>(
        r#"
        SELECT target.id, target.target_kind,
               target.last_reply_disposition::text
        FROM outreach_targets AS target
        WHERE target.workspace_id = $1
          AND lower(btrim(target.contact_email)) = ANY($2::text[])
          AND EXISTS (
              SELECT 1 FROM outreach_interactions AS earlier
              WHERE earlier.workspace_id = $1
                AND earlier.target_id = target.id
                AND earlier.direction = 'outbound'
                AND earlier.occurred_at > now() - make_interval(days => $3::int)
          )
        "#,
    )
    .bind(workspace_id)
    .bind(counterparts)
    .bind(days)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|(target_id, target_kind, disposition)| PitchedReplyTarget {
                target_id,
                target_kind,
                disposition,
            })
            .collect()
    })
}

/// Records what a replied contact actually said: the inbound interaction's
/// `metadata` gains `reply_text`, and a `reply_classifications` row with
/// `classification_result = 'auto'` queues it for the first-party
/// classifier — the same shape `record_reply` writes for a reply the
/// operator files by hand. Both writes are one transaction, and the
/// `metadata ? 'reply_text'` guard is the per-message idempotency: a second
/// pass over the same message updates nothing and inserts nothing, so the
/// caller reports `false` instead of double-counting.
///
/// # Errors
///
/// Any database error.
pub async fn record_reply_text(
    pool: &PgPool,
    workspace_id: Uuid,
    message_id: &str,
    target: &PitchedReplyTarget,
    reply_text: &str,
    occurred_at: OffsetDateTime,
) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let marked = sqlx::query_scalar::<_, i64>(
        r#"
        UPDATE outreach_interactions
        SET metadata = metadata || jsonb_build_object('reply_text', $4::text)
        WHERE workspace_id = $1
          AND target_id = $2
          AND source_key = $3
          AND direction = 'inbound'
          AND NOT (metadata ? 'reply_text')
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(target.target_id)
    .bind(format!("gmail:{message_id}"))
    .bind(reply_text)
    .fetch_optional(&mut *transaction)
    .await?;
    if marked.is_none() {
        transaction.commit().await?;
        return Ok(false);
    }
    sqlx::query(
        r#"
        INSERT INTO reply_classifications (
            workspace_id, target_id, target_kind,
            reply_text, previous_disposition,
            classification_result, classified_disposition,
            confidence_basis_points, matched_rules, classified_at
        )
        VALUES ($1, $2, $3, $4, $5, 'auto', NULL, 0, '[]'::jsonb, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(target.target_id)
    .bind(&target.target_kind)
    .bind(reply_text)
    .bind(&target.disposition)
    .bind(occurred_at)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(true)
}

/// Inbound ledger rows that predate reply-body capture: `(message_id,
/// target, occurred_at)` for every recorded reply still missing
/// `reply_text`, oldest first. The backfill drains them a few per cycle.
/// The same pitched window the live path applies gates it — measured back
/// from the reply's own `occurred_at`, so an old pitch followed by a fresh
/// answer still qualifies.
///
/// # Errors
///
/// Any database error.
pub async fn unbodied_replies(
    pool: &PgPool,
    workspace_id: Uuid,
    since: OffsetDateTime,
    pitched_days: i32,
    limit: i64,
) -> Result<Vec<(String, PitchedReplyTarget, OffsetDateTime)>, sqlx::Error> {
    sqlx::query_as::<_, (String, Uuid, String, Option<String>, OffsetDateTime)>(
        r#"
        SELECT replace(oi.source_key, 'gmail:', '') AS message_id,
               oi.target_id, target.target_kind,
               target.last_reply_disposition::text, oi.occurred_at
        FROM outreach_interactions oi
        JOIN outreach_targets target
          ON target.workspace_id = oi.workspace_id
         AND target.id = oi.target_id
        WHERE oi.workspace_id = $1
          AND oi.direction = 'inbound'
          AND oi.source_key LIKE 'gmail:%'
          AND oi.occurred_at > $2
          AND NOT (oi.metadata ? 'reply_text')
          AND EXISTS (
              SELECT 1 FROM outreach_interactions AS earlier
              WHERE earlier.workspace_id = oi.workspace_id
                AND earlier.target_id = oi.target_id
                AND earlier.direction = 'outbound'
                AND earlier.occurred_at > oi.occurred_at - make_interval(days => $4::int)
          )
        ORDER BY oi.occurred_at
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .bind(pitched_days)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(message_id, target_id, target_kind, disposition, occurred_at)| {
                    (
                        message_id,
                        PitchedReplyTarget {
                            target_id,
                            target_kind,
                            disposition,
                        },
                        occurred_at,
                    )
                },
            )
            .collect()
    })
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
