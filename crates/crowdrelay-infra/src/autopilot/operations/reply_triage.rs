//! Reply triage repository — loads unclassified replies and records
//! first-party classifications.
//!
//! The worker calls this after wave outcome settlement. Replies with
//! `Received` disposition are loaded, classified by the domain classifier,
//! and the result is recorded. `Auto` classifications also update the
//! target's disposition; `NeedsHuman` classifications are stored for the
//! operator brief to surface.

use async_trait::async_trait;
use crowdrelay_application::autopilot::{
    AutopilotReplyTriageRepository, ReplyNeedingTriage, ReplyTriageResult,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::outreach::{OutreachReplyDisposition, OutreachTargetKind};
use crowdrelay_domain::reply_triage::ReplyClassification;
use serde_json::json;
use sqlx::FromRow;

use super::*;

#[derive(FromRow)]
struct ReplyRow {
    id: uuid::Uuid,
    target_id: uuid::Uuid,
    target_kind: String,
    reply_text: String,
    previous_disposition: Option<String>,
}

#[async_trait]
impl AutopilotReplyTriageRepository for PostgresAutopilotRepository {
    async fn load_replies_needing_triage(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<ReplyNeedingTriage>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, ReplyRow>(
                r#"
                SELECT
                    c.id,
                    c.target_id,
                    c.target_kind,
                    c.reply_text,
                    c.previous_disposition
                FROM viryaos_reply_classifications c
                WHERE c.workspace_id = $1
                  AND c.classification_result = 'auto'
                  AND c.classified_disposition IS NULL
                ORDER BY c.classified_at
                LIMIT $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(i64::from(limit.min(100)))
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            rows.into_iter()
                .map(|row| {
                    let target_kind = OutreachTargetKind::parse(&row.target_kind)
                        .ok_or(RepositoryError::Unexpected)?;
                    let previous_disposition = row
                        .previous_disposition
                        .as_deref()
                        .and_then(parse_reply_disposition);
                    Ok(ReplyNeedingTriage {
                        reply_id: row.id,
                        target_id: row.target_id,
                        target_kind,
                        reply_text: row.reply_text,
                        previous_disposition,
                    })
                })
                .collect()
        })
        .await
    }

    async fn record_reply_classification(
        &self,
        workspace_id: WorkspaceId,
        reply_id: uuid::Uuid,
        result: &ReplyTriageResult,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            // Fetch the target_id and reply_text once. Both are needed
            // downstream (target update for Auto, outbox event for NeedsHuman)
            // and a single read avoids redundant subqueries.
            //
            // If the row is gone (already classified by another worker between
            // load and record), silently skip — a race-lost classification is
            // not an error and must not fail the autopilot cycle.
            let (target_id, reply_text): (uuid::Uuid, String) =
                match sqlx::query_as::<_, (uuid::Uuid, String)>(
                    "SELECT target_id, reply_text FROM viryaos_reply_classifications
                     WHERE workspace_id = $1 AND id = $2
                       AND classified_disposition IS NULL
                       AND classification_result = 'auto'",
                )
                .bind(workspace_id.into_uuid())
                .bind(reply_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                {
                    Some(row) => row,
                    None => {
                        transaction.rollback().await.map_err(map_sqlx)?;
                        return Ok(());
                    }
                };

            let (classification_result, classified_disposition, human_review_reason, confidence_bp, matched_rules) =
                match &result.classification {
                    ReplyClassification::Auto {
                        disposition,
                        confidence,
                        matched_rules,
                    } => {
                        let disp = match disposition {
                            OutreachReplyDisposition::Positive => "positive",
                            OutreachReplyDisposition::Declined => "declined",
                            OutreachReplyDisposition::DoNotContact => "do_not_contact",
                            _ => return Err(RepositoryError::Unexpected),
                        };
                        (
                            "auto",
                            Some(disp),
                            None::<&str>,
                            confidence.basis_points(),
                            json!(matched_rules),
                        )
                    }
                    ReplyClassification::NeedsHuman { reason, confidence } => {
                        let r = reason.as_str();
                        (
                            "needs_human",
                            None::<&str>,
                            Some(r),
                            confidence.basis_points(),
                            json!(Vec::<&str>::new()),
                        )
                    }
                };

            // Update the classification row. The `classified_disposition IS
            // NULL` guard is optimistic locking: if another worker already
            // classified this reply between load and record, the UPDATE
            // affects zero rows and we skip the target update too — no
            // double-counting of the relationship delta.
            let updated = sqlx::query(
                r#"
                UPDATE viryaos_reply_classifications
                SET classification_result = $3,
                    classified_disposition = $4,
                    human_review_reason = $5,
                    confidence_basis_points = $6,
                    matched_rules = $7,
                    classified_at = $8
                WHERE workspace_id = $1
                  AND id = $2
                  AND classified_disposition IS NULL
                  AND classification_result = 'auto'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(reply_id)
            .bind(classification_result)
            .bind(classified_disposition)
            .bind(human_review_reason)
            .bind(i32::from(confidence_bp))
            .bind(&matched_rules)
            .bind(result.classified_at)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            if updated.rows_affected() == 0 {
                // Another worker classified this reply between load and
                // record. Roll back and return Ok — a race-lost
                // classification is not an error, and the target update
                // must not run.
                transaction.rollback().await.map_err(map_sqlx)?;
                return Ok(());
            }

            // For auto-classifications, also update the target's disposition.
            // `last_reply_at` is NOT updated here — the ingress already set it
            // to the reply's occurred_at, and the classification time is not
            // when the reply was received.
            if let ReplyClassification::Auto { disposition, .. } = &result.classification {
                let disp_str = match disposition {
                    OutreachReplyDisposition::Positive => "positive",
                    OutreachReplyDisposition::Declined => "declined",
                    OutreachReplyDisposition::DoNotContact => "do_not_contact",
                    _ => return Err(RepositoryError::Unexpected),
                };
                let relationship_delta = match disposition {
                    OutreachReplyDisposition::Positive => 5,
                    OutreachReplyDisposition::Declined => -5,
                    OutreachReplyDisposition::DoNotContact => -15,
                    _ => 0,
                };
                let new_version = sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_outreach_targets
                    SET last_reply_disposition = $3,
                        do_not_contact = CASE WHEN $3 = 'do_not_contact' THEN true ELSE do_not_contact END,
                        accepts_outreach = CASE WHEN $3 = 'do_not_contact' THEN false ELSE accepts_outreach END,
                        relationship_score = GREATEST(0, LEAST(100, relationship_score + $4)),
                        version = version + 1
                    WHERE workspace_id = $1 AND id = $2
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(target_id)
                .bind(disp_str)
                .bind(relationship_delta)
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;

                // Record a history snapshot, same as the ingress does after
                // every target mutation. Without this, the history table has
                // gaps at versions created by the worker.
                sqlx::query(
                    r#"
                    INSERT INTO viryaos_outreach_target_history (
                        workspace_id, target_id, version, snapshot
                    )
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
                        'last_reply_at', last_reply_at,
                        'last_reply_disposition', last_reply_disposition
                    )
                    FROM viryaos_outreach_targets
                    WHERE workspace_id = $1 AND id = $2 AND version = $3
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(target_id)
                .bind(new_version)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;

                // When the classifier confirms DNC, update the contact governor
                // so the target's email is blocked across all future targets.
                // The ingress skips this when reply_text is present (because the
                // applied disposition is `received`); the worker is the sole
                // authority for DNC via classification.
                if disp_str == "do_not_contact" {
                    sqlx::query(
                        r#"
                        INSERT INTO viryaos_contact_governor (
                            workspace_id, normalized_contact, last_context, last_action_id,
                            last_outbound_at, next_contact_after, do_not_contact
                        )
                        SELECT $1, lower(btrim(contact_email)), 'outreach', NULL, $3, $3, true
                        FROM viryaos_outreach_targets
                        WHERE workspace_id = $1 AND id = $2
                        ON CONFLICT (workspace_id, normalized_contact) DO UPDATE
                        SET do_not_contact = true,
                            last_context = EXCLUDED.last_context,
                            next_contact_after = GREATEST(
                                viryaos_contact_governor.next_contact_after,
                                EXCLUDED.next_contact_after
                            ),
                            updated_at = now()
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(target_id)
                    .bind(result.classified_at)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
            }

            // For NeedsHuman classifications, emit an ops.alert outbox event so
            // the operator brief surfaces the reply for human review. Same
            // transaction: the classification and its alert are atomic.
            if let ReplyClassification::NeedsHuman { reason, .. } = &result.classification {
                sqlx::query(
                    r#"
                    INSERT INTO outbox_events (
                        workspace_id, event_type, event_version, payload, request_id
                    ) VALUES (
                        $1, 'crowdrelay.ops.reply_needs_human', 1,
                        jsonb_build_object(
                            'reply_id', $2::uuid,
                            'target_id', $3::uuid,
                            'reply_text', $4::text,
                            'reason', $5::text,
                            'classified_at', $6::timestamptz,
                            'source', 'crowdrelay-worker'
                        ),
                        $7
                    )
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(reply_id)
                .bind(target_id)
                .bind(&reply_text)
                .bind(reason.as_str())
                .bind(result.classified_at)
                .bind(format!("reply-triage:{reply_id}"))
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}

fn parse_reply_disposition(value: &str) -> Option<OutreachReplyDisposition> {
    match value {
        "none" => Some(OutreachReplyDisposition::None),
        "received" => Some(OutreachReplyDisposition::Received),
        "positive" => Some(OutreachReplyDisposition::Positive),
        "declined" => Some(OutreachReplyDisposition::Declined),
        "do_not_contact" => Some(OutreachReplyDisposition::DoNotContact),
        _ => None,
    }
}
