//! Community relay batch mutations, split out of `control_mutations.rs`.
//!
//! The batch is the operator's one answer for a whole spread of community
//! deliveries: approve releases every parked delivery under the drip
//! interval, revoke cancels whatever has not yet left. Both run under the
//! usual operator-action idempotency ledger.

use super::control_mutations::OUTWARD_HOLD_SECONDS;
use super::*;

/// The post-ledger statuses whose words can still be changed — the row is
/// seeded but nothing has reached Reddit. `posting` is absent on purpose:
/// a claimed send may already hold the old text inside a Reddit call, and
/// rewriting the row then would not rewrite what leaves. `awaiting_manual_post`
/// is out for the same reason — the words may already be in a person's hands.
const EDITABLE_POST_STATUSES: &[&str] = &["pending", "rate_limited"];

impl PostgresAutopilotRepository {
    /// Approves a community relay batch — the content's whole spread at once.
    ///
    /// The thing parked in front of the operator is the question "does this
    /// post go to the communities that will take it", asked once per source
    /// rather than once per community. Approving writes the standing answer
    /// on the batch row — drafts still landing queue under it without asking
    /// again — and releases every delivery already parked for the source.
    /// Released rungs drip out at the batch's `interval_seconds`, the cadence
    /// the operator saw on the card; the community executor owns that pacing.
    ///
    /// `revisions` carries the card's edit boxes: `action_id → field → text`,
    /// reviewed per delivery through the same draft-revision gate a
    /// single-action approval applies. A refused edit refuses the whole
    /// approval — the batch never approves around a draft the operator
    /// meant to fix.
    pub(super) async fn approve_community_relay_operator(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
        interval_seconds: Option<i32>,
        revisions: Option<
            &std::collections::BTreeMap<Uuid, std::collections::BTreeMap<String, String>>,
        >,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            // The audit row records which deliveries the operator meant to
            // edit — ids only; the before/after text lives in
            // `draft_revisions` once each edit is accepted.
            let details = match revisions {
                Some(revisions) if !revisions.is_empty() => json!({
                    "requested_status": "approved",
                    "revision_actions": revisions.keys().collect::<Vec<_>>(),
                }),
                _ => json!({"requested_status": "approved"}),
            };
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "approve_community_relay_batch",
                "content_source",
                source_id,
                "admin_api_key",
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: source_id,
                    status: "approved".to_owned(),
                    replayed: true,
                });
            }
            // The batch is the approval's subject — no batch means no drafts
            // ever landed for this source, and there is nothing to approve.
            // An answered batch is a conflict, not a second approval.
            let batch_status = sqlx::query_scalar::<_, String>(
                "UPDATE community_relay_batches \
                 SET status = 'approved', approved_at = now(), \
                     approved_by = 'operator:community_relay', \
                     observe_until = now() + INTERVAL '7 days', \
                     interval_seconds = COALESCE($3, interval_seconds), \
                     updated_at = now() \
                 WHERE workspace_id = $1 AND source_id = $2 \
                   AND status = 'awaiting_approval' \
                 RETURNING status",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(interval_seconds)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if batch_status.is_none() {
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM community_relay_batches \
                     WHERE workspace_id = $1 AND source_id = $2)",
                )
                .bind(workspace_id.into_uuid())
                .bind(source_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                return if exists {
                    Err(RepositoryError::Conflict)
                } else {
                    Err(RepositoryError::NotFound)
                };
            }
            // Operator edits land inside the approval, before the release:
            // the queue never holds an approved action carrying unapproved
            // text. A refusal rolls the batch flip back with it — there is
            // no half-edited approval.
            if let Some(revisions) = revisions {
                for (action_id, revision) in revisions {
                    apply_relay_revision(
                        &mut transaction,
                        workspace_id,
                        source_id,
                        *action_id,
                        revision,
                        operation_id,
                    )
                    .await?;
                }
            }
            let now = OffsetDateTime::now_utc();
            let released: Vec<Uuid> = sqlx::query_scalar(
                r#"
                UPDATE autopilot_actions
                SET status='queued', approved_at=$3, approved_by='operator:community_relay',
                    -- O.2: an outward send waits out its hold window before a
                    -- worker may claim it — the batch is the approval, not a
                    -- shortcut past the window that makes revoking meaningful.
                    available_at = now() + CASE
                        WHEN action_class IN ('owned_audience', 'third_party', 'paid')
                        THEN make_interval(secs => $4::double precision)
                        ELSE INTERVAL '0'
                    END
                WHERE workspace_id=$1
                  AND action_kind='community.engage.request'
                  AND payload->>'source_id' = $2::text
                  AND status='awaiting_approval'
                  AND (approval_expires_at IS NULL OR approval_expires_at > $3)
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(now)
            .bind(OUTWARD_HOLD_SECONDS)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // A parked rung can carry an open crew assignment — close it, or
            // a reminder keeps asking somebody to approve what already
            // queued.
            sqlx::query(
                "UPDATE team_assignments \
                 SET status='done', completed_at=$3, next_reminder_at=NULL \
                 WHERE workspace_id=$1 AND action_id = ANY($2) AND status='open'",
            )
            .bind(workspace_id.into_uuid())
            .bind(&released)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: source_id,
                status: format!("approved:{}", released.len()),
                replayed: false,
            })
        })
        .await
    }

    /// Revokes a community relay batch — "stop the rest of this spread".
    ///
    /// The batch row is the standing answer, so revoking flips it and
    /// everything the answer covered: deliveries still parked lose their ask,
    /// deliveries queued but not yet executed are cancelled, and the
    /// community_posts rows the drip was about to send are cancelled rather
    /// than left claimable. A post already on Reddit keeps its record — a
    /// revoke cannot unpost, and pretending it could is the dishonest part
    /// the status names around.
    pub(super) async fn revoke_community_relay_operator(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "revoke_community_relay_batch",
                "content_source",
                source_id,
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"requested_status": "revoked"}),
            )
            .await?;
            if let Some(existing) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: source_id,
                    status: "revoked".to_owned(),
                    replayed: true,
                });
            }
            let revoked = sqlx::query_scalar::<_, String>(
                "UPDATE community_relay_batches \
                 SET status = 'revoked', revoked_at = now(), \
                     revoked_by = 'operator:community_relay', updated_at = now() \
                 WHERE workspace_id = $1 AND source_id = $2 \
                   AND status IN ('awaiting_approval', 'approved') \
                 RETURNING status",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if revoked.is_none() {
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM community_relay_batches \
                     WHERE workspace_id = $1 AND source_id = $2)",
                )
                .bind(workspace_id.into_uuid())
                .bind(source_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                return if exists {
                    Err(RepositoryError::Conflict)
                } else {
                    Err(RepositoryError::NotFound)
                };
            }
            let now = OffsetDateTime::now_utc();
            let cancelled: Vec<Uuid> = sqlx::query_scalar(
                "UPDATE autopilot_actions \
                 SET status='cancelled', finished_at=$3 \
                 WHERE workspace_id=$1 \
                   AND action_kind='community.engage.request' \
                   AND payload->>'source_id' = $2::text \
                   AND status IN ('awaiting_approval', 'queued') \
                 RETURNING id",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(now)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                "UPDATE team_assignments \
                 SET status='cancelled', completed_at=NULL, next_reminder_at=NULL \
                 WHERE workspace_id=$1 AND action_id = ANY($2) AND status='open'",
            )
            .bind(workspace_id.into_uuid())
            .bind(&cancelled)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // The queue rows the drip was about to send stop being claimable.
            // `posting` rows are mid-flight — a revoke cannot reach inside a
            // Reddit call, and the row's own recovery owns that outcome.
            let posts_cancelled = sqlx::query(
                "UPDATE community_posts \
                 SET status='cancelled', updated_at=$3 \
                 WHERE workspace_id=$1 AND relay_source_id=$2 \
                   AND status IN ('pending','rate_limited','awaiting_manual_post')",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: source_id,
                status: format!(
                    "revoked:{}:{}",
                    cancelled.len(),
                    posts_cancelled.rows_affected()
                ),
                replayed: false,
            })
        })
        .await
    }
}

/// Applies one delivery's revision inside a batch approval's transaction.
///
/// The action row is locked and proven to be a delivery of this batch in
/// this workspace — an id that misses names a row the operator cannot see
/// on this card, and editing the wrong draft is worse than refusing. The
/// review runs against `RELAY_REVISABLE_FIELDS` (the post's own words) and
/// the same refusals a single-action revision applies; every accepted field
/// lands in `draft_revisions` with the batch's operation id.
///
/// Where the words live depends on how far the delivery got: a parked or
/// queued action's payload still feeds the post seed, while a delivered
/// action's `community_posts` row is what will actually send — so a seeded
/// row in a still-editable status is rewritten too. A post already on its
/// way out is refused: the old text may be inside a Reddit call, and
/// rewriting the row then would lie about what leaves.
async fn apply_relay_revision(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    source_id: Uuid,
    action_id: Uuid,
    revision: &std::collections::BTreeMap<String, String>,
    operation_id: Uuid,
) -> Result<(), RepositoryError> {
    let Some((mut payload, action_status)) = sqlx::query_as::<_, (serde_json::Value, String)>(
        "SELECT payload, status FROM autopilot_actions \
         WHERE workspace_id = $1 AND id = $2 \
           AND action_kind = 'community.engage.request' \
           AND payload->>'source_id' = $3::text \
         FOR UPDATE",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(source_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    else {
        return Err(RepositoryError::ConflictBecause(
            "delivery revision names an action outside this batch",
        ));
    };
    // The post ledger row is the truth of what will send once it exists;
    // its status decides whether the words can still change.
    let post_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM community_posts \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let post_editable = post_status
        .as_deref()
        .is_some_and(|status| EDITABLE_POST_STATUSES.contains(&status));
    if post_status.is_some() && !post_editable {
        return Err(RepositoryError::ConflictBecause(
            "delivery revision refused: the post has already left",
        ));
    }
    let action_editable = matches!(
        action_status.as_str(),
        "awaiting_approval" | "queued" | "processing"
    );
    if !action_editable && !post_editable {
        return Err(RepositoryError::ConflictBecause(
            "delivery revision refused: the delivery is finished — nothing left to edit",
        ));
    }

    let draft = crowdrelay_domain::draft_revision::revisable_fields_in(
        crowdrelay_domain::draft_revision::RELAY_REVISABLE_FIELDS,
        &payload,
    );
    let changed = crowdrelay_domain::draft_revision::review_revision_fields(
        crowdrelay_domain::draft_revision::RELAY_REVISABLE_FIELDS,
        &draft,
        revision,
    )
    .map_err(|refusal| RepositoryError::ConflictBecause(refusal.conflict_reason()))?;
    // The growth ceiling is generous; Reddit's own limit is not. A title it
    // will reject must refuse here, not at send time under an approval that
    // already recorded.
    if let Some(title) = changed.get("title")
        && title.chars().count() > crowdrelay_domain::draft_revision::RELAY_TITLE_CHAR_LIMIT
    {
        return Err(RepositoryError::ConflictBecause(
            "delivery revision refused: the title is longer than Reddit's 300-character limit",
        ));
    }
    crowdrelay_domain::draft_revision::apply_revision(&mut payload, &changed);
    // The payload keeps the words that were approved even when the action
    // already ran — the ledger row and the post row tell one story.
    sqlx::query("UPDATE autopilot_actions SET payload = $3 WHERE workspace_id = $1 AND id = $2")
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(&payload)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    if post_editable {
        // Only the post's own columns move, and only fields that actually
        // changed — the row's link, media and target stay the batch's facts.
        let rewritten = sqlx::query(
            "UPDATE community_posts \
             SET title = COALESCE($3, title), body = COALESCE($4, body), updated_at = now() \
             WHERE workspace_id = $1 AND action_id = $2 \
               AND status = ANY($5)",
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(changed.get("title").map(String::as_str))
        .bind(changed.get("body").map(String::as_str))
        .bind(EDITABLE_POST_STATUSES)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        if rewritten.rows_affected() == 0 {
            // The pre-check passed but the claim won the race — same refusal.
            return Err(RepositoryError::ConflictBecause(
                "delivery revision refused: the post has already left",
            ));
        }
    }
    // Same ledger shape as the single-action path: one row per edited field,
    // the machine's words, the operator's words, and how far they moved.
    for (field, after) in &changed {
        let before = draft.get(field).cloned().unwrap_or_default();
        let mut single = std::collections::BTreeMap::new();
        single.insert(field.clone(), after.clone());
        let distance = crowdrelay_domain::draft_revision::revision_distance(&draft, &single);
        sqlx::query(
            "INSERT INTO draft_revisions \
             (workspace_id, action_id, operation_id, field, before_text, after_text, distance_chars) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(operation_id)
        .bind(field)
        .bind(before)
        .bind(after)
        .bind(i64::try_from(distance).unwrap_or(i64::MAX))
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}
