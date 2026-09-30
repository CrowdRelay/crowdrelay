//! The control plane's operator mutations, split out of `control.rs`.
//!
//! Same repository, same transactions, one seam: everything here records an
//! operator decision about a parked action or a finding, under the usual
//! idempotency ledger. Reads live in `control.rs`.

use super::*;

/// Seconds an approved outward action waits before a worker may claim it.
///
/// One number for every outward class, taken from the domain so the SQL here
/// and `gig_outreach`'s own insert cannot drift apart about how long "hold on"
/// lasts. See `ActionClass::hold_seconds` for why the window exists at all.
pub(super) const OUTWARD_HOLD_SECONDS: f64 =
    crowdrelay_domain::action_class::ActionClass::ThirdParty.hold_seconds() as f64;

/// Why an approve or cancel matched no row, in words an operator can act on.
///
/// Only `awaiting_approval` rows are transitionable, so anything else is either
/// already decided or timed out. The strings are `&'static` so they travel into
/// `Problem::detail` unchanged.
fn transition_conflict_reason(status: &str, expired: bool) -> &'static str {
    match status {
        "awaiting_approval" if expired => {
            "This action's approval window has closed. The brain will re-evaluate it on the next cycle."
        }
        "queued" | "processing" => {
            "This action was already approved and is running. Nothing is waiting on you."
        }
        "succeeded" => "This action has already run. Nothing is waiting on you.",
        "failed" => {
            "This action already ran and failed. Approving it again will not retry it — the brain will decide whether to."
        }
        "cancelled" => "This action was already cancelled.",
        // `awaiting_approval` and unexpired reaching here means the UPDATE lost
        // a race with a concurrent decision rather than hitting a stale row.
        _ => "Another decision on this action landed first. Reload to see its current state.",
    }
}

/// The applied-revision bundle the approve path carries between the payload
/// rewrite and the ledger insert: the new payload, the fields that changed,
/// and the draft as it stood before the revision.
type AppliedRevision = (
    serde_json::Value,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
);

impl PostgresAutopilotRepository {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn control_action_transition(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
        operator_action: &'static str,
        target_status: &'static str,
        revision: Option<&std::collections::BTreeMap<String, String>>,
        approved_by: &'static str,
        error_kind: Option<&'static str>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            // The audit row records which fields the operator meant to edit —
            // names only; the before/after text lives in
            // `draft_revisions` once the edit is accepted. `actor` says which
            // door the answer came through — the admin API key or a mailed
            // one-click link — because `actor_type` is a credential-class
            // vocabulary, not a channel one.
            let details = match revision {
                Some(revision) => json!({
                    "requested_status": target_status,
                    "actor": approved_by,
                    "revision_fields": revision.keys().collect::<Vec<_>>(),
                }),
                None => json!({"requested_status": target_status, "actor": approved_by}),
            };
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                operator_action,
                "autopilot_action",
                action_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing) = replay {
                let status = sqlx::query_scalar::<_, String>(
                    "SELECT status FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::NotFound)?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: action_id.into_uuid(),
                    status,
                    replayed: true,
                });
            }

            // A wave is approved as a wave: an individual approve or cancel on
            // one of its pitches would send — or silently pull — a letter the
            // batch review never covered. While the wave is unsettled the
            // only doors are its own approval and its expiry.
            let wave_locked = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(
                    SELECT 1
                    FROM outreach_waves AS wave
                    JOIN autopilot_actions AS action
                      ON action.workspace_id = wave.workspace_id
                     AND action.payload->>'wave_id' = wave.id::text
                    WHERE action.workspace_id = $1
                      AND action.id = $2
                      AND wave.state IN ('drafting', 'sealed')
                )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if wave_locked {
                return Err(RepositoryError::Conflict);
            }

            // Approve-with-revision: the operator's edit is reviewed against
            // the stored payload before anything moves — a refused revision
            // refuses the whole approval rather than silently approving the
            // original. Locking the row here keeps the payload we reviewed the
            // payload we write; the status UPDATE below then cannot race a
            // concurrent cancel into rewriting a draft that was just edited.
            let mut revised: Option<AppliedRevision> = None;
            if let Some(revision) = revision
                && target_status == "queued"
                && let Some((payload,)) = sqlx::query_as::<_, (serde_json::Value,)>(
                    r#"
                    SELECT payload FROM autopilot_actions
                    WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_approval'
                    FOR UPDATE
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            {
                let draft = crowdrelay_domain::draft_revision::revisable_fields(&payload);
                let changed = crowdrelay_domain::draft_revision::review_revision(&draft, revision)
                    .map_err(|refusal| {
                        RepositoryError::ConflictBecause(refusal.conflict_reason())
                    })?;
                let mut applied = payload;
                crowdrelay_domain::draft_revision::apply_revision(&mut applied, &changed);
                revised = Some((applied, changed, draft));
                // A missing row falls through to the ordinary UPDATE below,
                // which reports the action's real state instead of a revision
                // refusal for a draft that is no longer approvable.
            }

            let updated = if target_status == "queued" {
                let query = if revised.is_some() {
                    // The reviewed words replace the draft in the same
                    // statement that flips the status — the queue never holds
                    // an approved action carrying the unapproved text.
                    sqlx::query_scalar::<_, String>(
                        r#"
                        UPDATE autopilot_actions
                        SET status = 'queued', payload = $3,
                            approved_at = now(), approved_by = $5,
                            -- O.2: an outward send waits out its hold window
                            -- before a worker may claim it. `action_class` is
                            -- the same durable classification the outward gate
                            -- binds on, so nothing here depends on the payload
                            -- describing itself honestly.
                            available_at = now() + CASE
                                WHEN action_class IN ('owned_audience', 'third_party', 'paid')
                                THEN make_interval(secs => $4::double precision)
                                ELSE INTERVAL '0'
                            END
                        WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_approval'
                          AND (approval_expires_at IS NULL OR approval_expires_at > now())
                        RETURNING status
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(action_id.into_uuid())
                    .bind(revised.as_ref().map(|(payload, _, _)| payload.clone()))
                    .bind(OUTWARD_HOLD_SECONDS)
                    .bind(approved_by)
                } else {
                    sqlx::query_scalar::<_, String>(
                        r#"
                        UPDATE autopilot_actions
                        SET status = 'queued', approved_at = now(),
                            approved_by = $4,
                            available_at = now() + CASE
                                WHEN action_class IN ('owned_audience', 'third_party', 'paid')
                                THEN make_interval(secs => $3::double precision)
                                ELSE INTERVAL '0'
                            END
                        WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_approval'
                          AND (approval_expires_at IS NULL OR approval_expires_at > now())
                        RETURNING status
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(action_id.into_uuid())
                    .bind(OUTWARD_HOLD_SECONDS)
                    .bind(approved_by)
                };
                query
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?
            } else {
                sqlx::query_scalar::<_, String>(
                    r#"
                    UPDATE autopilot_actions
                    SET status = 'cancelled', finished_at = now(),
                        last_error_kind = COALESCE($3, last_error_kind)
                    WHERE workspace_id = $1 AND id = $2
                      AND (
                            status = 'awaiting_approval'
                            -- O.2: an approved outward send that is still
                            -- inside its hold window and that no worker has
                            -- claimed. `attempt_count = 0` and the future
                            -- `available_at` are what "nothing has happened
                            -- yet" looks like from here; `processing` is
                            -- deliberately not in this set, because a letter
                            -- that is going out cannot be called back and
                            -- pretending otherwise is worse than refusing.
                         OR (status = 'queued'
                             AND attempt_count = 0
                             AND available_at > now())
                      )
                    RETURNING status
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .bind(error_kind)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            };
            // The transition matched nothing, and the operator deserves to know
            // which nothing. Collapsing a wrong action id, an already-run action
            // and an expired approval into one `Conflict` made a stale queue
            // read exactly like a broken button — which is how "Do it" came to
            // fail with "cannot be applied to the current durable state" on
            // every click, on a board whose actions had all already executed.
            let status = match updated {
                Some(status) => status,
                None => {
                    let existing = sqlx::query_as::<_, (String, bool)>(
                        "SELECT status,
                                approval_expires_at IS NOT NULL
                                  AND approval_expires_at <= now() AS expired
                         FROM autopilot_actions
                         WHERE workspace_id = $1 AND id = $2",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(action_id.into_uuid())
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    return Err(match existing {
                        None => RepositoryError::NotFound,
                        Some((status, expired)) => {
                            RepositoryError::ConflictBecause(transition_conflict_reason(
                                &status, expired,
                            ))
                        }
                    });
                }
            };
            if target_status == "queued" {
                sqlx::query(
                    r#"
                    UPDATE team_assignments
                    SET status='done', completed_at=now(), next_reminder_at=NULL
                    WHERE workspace_id=$1 AND action_id=$2 AND status='open'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;

                // §4d-3.1 — every edited field gets its own ledger row: the
                // machine's words, the band's words, and how far they moved.
                // This is the voice signal, not an edit log — the distance is
                // what §4d-3.2 reads to know whether drafts are getting closer.
                if let Some((_, changed, draft_before)) = &revised {
                    for (field, after) in changed {
                        let before = draft_before.get(field).cloned().unwrap_or_default();
                        // Per-field distance under the same rule the total
                        // uses, so the ledger and the trend measure one thing.
                        let mut single = std::collections::BTreeMap::new();
                        single.insert(field.clone(), after.clone());
                        let distance =
                            crowdrelay_domain::draft_revision::revision_distance(
                                draft_before,
                                &single,
                            );
                        sqlx::query(
                            r#"
                            INSERT INTO draft_revisions
                                (workspace_id, action_id, operation_id, field,
                                 before_text, after_text, distance_chars)
                            VALUES ($1, $2, $3, $4, $5, $6, $7)
                            -- A field already saved before approval (a wave
                            -- pitch's `revise`) keeps the machine's words as
                            -- its `before`; this edit is the final text.
                            ON CONFLICT (workspace_id, action_id, field) DO UPDATE SET
                                operation_id = EXCLUDED.operation_id,
                                after_text = EXCLUDED.after_text
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(action_id.into_uuid())
                        .bind(operation_id)
                        .bind(field)
                        .bind(before)
                        .bind(after)
                        .bind(i64::try_from(distance).unwrap_or(i64::MAX))
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                    }
                }
            } else {
                sqlx::query(
                    r#"
                    UPDATE team_assignments
                    SET status='cancelled', completed_at=NULL, next_reminder_at=NULL
                    WHERE workspace_id=$1 AND action_id=$2 AND status='open'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;

                // A cancelled content suggestion is the band's "not for us" —
                // a first-class taste signal, not a dismissal. Resolve the row
                // declined and write its outcome in the same transaction, so a
                // rejection cannot leave a zombie suggestion holding headroom
                // in the open queue while its ask is dead.
                let payload = sqlx::query_scalar::<_, serde_json::Value>(
                    r#"
                    SELECT payload FROM autopilot_actions
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                match serde_json::from_value::<AutopilotActionPayload>(payload) {
                    Ok(AutopilotActionPayload::RaiseContentArc { arc_id, .. }) => {
                        // A cancelled arc ask is the band's "not this season"
                        // — retire it so the open-arc check frees the slot and
                        // the anchor cooldown remembers the answer.
                        sqlx::query(
                            r#"
                            UPDATE arcs
                            SET status = 'retired', updated_at = now()
                            WHERE workspace_id = $1 AND id = $2 AND status = 'proposed'
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(arc_id.into_uuid())
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                    }
                    Ok(AutopilotActionPayload::RaiseContentSuggestion {
                        suggestion_id, ..
                    }) => {
                        let changed = sqlx::query(
                            r#"
                            UPDATE content_suggestions
                            SET status = 'declined', updated_at = now()
                            WHERE workspace_id = $1 AND id = $2 AND status IN ('raised', 'approved')
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(suggestion_id.into_uuid())
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?
                        .rows_affected();
                        // The outcome row pairs with the transition or it
                        // does not exist — an already-resolved suggestion
                        // earns no second verdict. The corner case is a
                        // decline racing the expiry sweep: a suggestion
                        // flipped `expired` mid-cancel matches no row, so
                        // the cancel commits while the "not for us" is
                        // dropped — the format is not suppressed and a
                        // re-raised beat will earn the verdict on the next
                        // refusal.
                        if changed > 0 {
                            sqlx::query(
                                r#"
                                INSERT INTO suggestion_outcomes (
                                    workspace_id, suggestion_id, outcome, decided_by, reason, results
                                ) VALUES ($1, $2, 'declined', $3, $4, '{}'::jsonb)
                                "#,
                            )
                            .bind(workspace_id.into_uuid())
                            .bind(suggestion_id.into_uuid())
                            .bind("operator:admin_api_key")
                            .bind("cancelled in the approval queue")
                            .execute(&mut *transaction)
                            .await
                            .map_err(map_sqlx)?;
                        }
                    }
                    _ => {}
                }
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: action_id.into_uuid(),
                status,
                replayed: false,
            })
        })
        .await
    }

    /// Records "we did this ourselves" about one finding.
    ///
    /// A first-class outcome, not a dismissal: the ledger row says a human
    /// took the opportunity, which is a success the measured record can read
    /// as one. The decision leaves every read model through that row, and any
    /// action of it still parked is withdrawn in the same transaction so a
    /// handled finding cannot go out anyway hours later.
    pub(super) async fn mark_decision_handled_operator(
        &self,
        workspace_id: WorkspaceId,
        decision_id: AutopilotDecisionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // The finding must exist before anything records having handled
            // it; otherwise a stale board click writes a suppression row for
            // a decision nobody ever saw.
            let exists = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM autopilot_decisions
                    WHERE workspace_id = $1 AND id = $2
                )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(decision_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !exists {
                return Err(RepositoryError::NotFound);
            }
            let operation_id = Uuid::now_v7();
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "handle_autopilot_decision_externally",
                "autopilot_decision",
                decision_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"decision_id": decision_id, "outcome": "handled_by_human"}),
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: decision_id.into_uuid(),
                    status: "handled_externally".into(),
                    replayed: true,
                });
            }
            sqlx::query(
                r#"
                UPDATE autopilot_actions
                SET status = 'cancelled', finished_at = now()
                WHERE workspace_id = $1 AND decision_id = $2 AND status = 'awaiting_approval'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(decision_id.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // "We did this ourselves" about a suggestion ask is the best
            // answer the engine can get: the beat happened. Resolve the row
            // done with its outcome so the cancelled ask cannot strand it in
            // the open queue and the band's initiative counts as the success
            // it is.
            sqlx::query(
                r#"
                WITH resolved AS (
                    UPDATE content_suggestions AS suggestion
                    SET status = 'done', updated_at = now()
                    WHERE suggestion.workspace_id = $1
                      AND suggestion.status = 'raised'
                      AND EXISTS (
                          SELECT 1 FROM autopilot_actions AS action
                          WHERE action.workspace_id = suggestion.workspace_id
                            AND action.decision_id = $2
                            AND action.subject_kind = 'content_suggestion'
                            AND action.subject_id = suggestion.id
                            AND action.status = 'cancelled'
                      )
                    RETURNING suggestion.id
                )
                INSERT INTO suggestion_outcomes (
                    workspace_id, suggestion_id, outcome, decided_by, reason
                )
                SELECT $1, resolved.id, 'done', 'operator:admin_api_key',
                       'handled outside the system'
                FROM resolved
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(decision_id.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // An arc ask handled outside the system means the season's shape
            // is being lived without it — retire the row so the open-arc
            // slot frees and the anchor cooldown stops the engine re-asking
            // a plan the band is already running.
            sqlx::query(
                r#"
                UPDATE arcs AS arc
                SET status = 'retired', updated_at = now()
                WHERE arc.workspace_id = $1
                  AND arc.status = 'proposed'
                  AND EXISTS (
                      SELECT 1 FROM autopilot_actions AS action
                      WHERE action.workspace_id = arc.workspace_id
                        AND action.decision_id = $2
                        AND action.subject_kind = 'content_arc'
                        AND action.subject_id = arc.id
                        AND action.status = 'cancelled'
                  )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(decision_id.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: decision_id.into_uuid(),
                status: "handled_externally".into(),
                replayed: false,
            })
        })
        .await
    }
}

impl PostgresAutopilotRepository {
    pub(super) async fn load_growth_posture_impl(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GrowthPostureView, RepositoryError> {
        self.bounded(async {
            let row = sqlx::query_as::<_, (Option<String>, i64, Option<OffsetDateTime>)>(
                r#"
                SELECT posture, expected_version, set_at
                FROM growth_posture
                WHERE workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(match row {
                Some((posture, version, set_at)) => GrowthPostureView {
                    // A posture this build cannot parse is not a reason to
                    // guess permissively; it reads as unset and the safe
                    // defaults hold.
                    posture: posture.as_deref().and_then(GrowthPosture::parse),
                    expected_version: version,
                    set_at,
                },
                None => GrowthPostureView {
                    posture: None,
                    expected_version: 1,
                    set_at: None,
                },
            })
        })
        .await
    }

    pub(super) async fn set_growth_posture_impl(
        &self,
        workspace_id: WorkspaceId,
        command: SetGrowthPosture,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            if let Some(existing) = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "set_growth_autonomy_posture",
                "growth_posture",
                workspace_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({
                    "posture": command.posture.as_str(),
                    "expected_version": command.expected_version,
                }),
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: workspace_id.into_uuid(),
                    status: format!("posture_{}", command.posture.as_str()),
                    replayed: true,
                });
            }

            // Optimistic concurrency on the posture row itself. A missing row
            // is version 1, so a first application from a fresh workspace only
            // succeeds when nobody else raced one in.
            let current: Option<i64> = sqlx::query_scalar(
                r#"
                SELECT expected_version FROM growth_posture
                WHERE workspace_id = $1 FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let current_version = current.unwrap_or(1);
            if current_version != command.expected_version {
                return Err(RepositoryError::Conflict);
            }
            let next_version = current_version + 1;

            // One: every context level. The mapping lives in the application
            // layer where the context list lives; this loop applies it verbatim.
            for context in AutopilotContext::ALL {
                sqlx::query(
                    r#"
                    UPDATE autopilot_policies
                    SET enabled = true,
                        autonomy_level = $3,
                        -- The row's own revision counter, and the reason this
                        -- statement must touch it.
                        --
                        -- `set_authority` guards with `AND version = $expected`
                        -- — optimistic concurrency on exactly these columns.
                        -- Changing `autonomy_level` here without bumping it
                        -- left a stale editor's `expected_version` still
                        -- matching, so an edit prepared before the posture
                        -- change landed on top of it and silently reverted the
                        -- authority level. The posture dial is the control
                        -- that sets every authority surface at once; a
                        -- concurrent edit undoing it unnoticed is the failure
                        -- the version guard exists to prevent.
                        --
                        -- This is the policy row's counter, not the posture
                        -- row's `expected_version`. The two are separate
                        -- sequences, and writing one into the other could move
                        -- a policy version backwards.
                        version = version + 1
                    WHERE workspace_id = $1 AND context = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(context.as_str())
                .bind(autonomy_level_str(command.posture.context_level(context)))
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }

            // Two: the four class ceilings, with the posture named as why.
            for class in [
                ActionClass::FirstPartyReversible,
                ActionClass::OwnedAudience,
                ActionClass::ThirdParty,
                ActionClass::Paid,
            ] {
                sqlx::query(
                    r#"
                    INSERT INTO growth_autonomy (
                        workspace_id, action_class, ceiling, rationale
                    ) VALUES ($1, $2, $3, $4)
                    ON CONFLICT (workspace_id, action_class) DO UPDATE
                    SET ceiling = EXCLUDED.ceiling,
                        rationale = EXCLUDED.rationale,
                        version = growth_autonomy.version + 1
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(class.as_str())
                .bind(autonomy_level_str(command.posture.ceiling(class)))
                .bind(command.posture.ceiling_rationale())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }

            // Three: the envelope switches only. Budgets, cooldowns and blast
            // radius are the operator's tuned numbers; a posture flip that
            // silently reset them would be a regression wearing a feature's
            // clothes.
            let (agent_enabled, dry_run) = command.posture.envelope();
            sqlx::query(
                r#"
                INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run)
                VALUES ($1, $2, $3)
                ON CONFLICT (workspace_id) DO UPDATE
                SET agent_enabled = EXCLUDED.agent_enabled,
                    dry_run = EXCLUDED.dry_run,
                    version = growth_envelope.version + 1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(agent_enabled)
            .bind(dry_run)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            sqlx::query(
                r#"
                INSERT INTO growth_posture (
                    workspace_id, posture, expected_version, set_at
                ) VALUES ($1, $2, $3, now())
                ON CONFLICT (workspace_id) DO UPDATE
                SET posture = EXCLUDED.posture,
                    expected_version = EXCLUDED.expected_version,
                    set_at = now()
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.posture.as_str())
            .bind(next_version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: workspace_id.into_uuid(),
                status: format!("posture_{}", command.posture.as_str()),
                replayed: false,
            })
        })
        .await
    }

    /// Releases a synced post's whole relay ladder at once (P.5).
    ///
    /// Every rung the post already asked for — the owned-audience push and one
    /// community relay per admitted community — shares the idempotency prefix
    /// `action:relay:{source}:`; releasing by that prefix moves the whole
    /// spread in one statement, because the thing the operator approved was
    /// the spread. The release marks `operator:relay_ladder` so a revoke
    /// cancels only what the ladder freed, and applies the same outward hold
    /// an individual approval does: the window is what makes "stop"
    /// meaningful. A rung decided later — a community admitted after the
    /// approval — asks on its own: the ladder is the spread the operator
    /// could read, not a standing yes to whatever the post might still
    /// become.
    pub(super) async fn approve_relay_ladder_operator(
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
                "approve_autopilot_relay_ladder",
                "content_source",
                source_id,
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"requested_status": "approved"}),
            )
            .await?;
            if let Some(existing) = replay {
                // The approval is a point-in-time release — the operator
                // actions row already records it. Reporting the rungs still
                // parked would pretend the release never happened; reporting
                // the queued count now would count rungs a revoke already
                // cancelled. "approved" is the honest answer: it did happen.
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: source_id,
                    status: "approved".to_owned(),
                    replayed: true,
                });
            }
            // A foreign or misspelt source id is a not-found, not a released
            // count of zero.
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM content_sources \
                 WHERE workspace_id=$1 AND id=$2 AND source_kind='social_post')",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !exists {
                return Err(RepositoryError::NotFound);
            }
            let now = OffsetDateTime::now_utc();
            let released: Vec<Uuid> = sqlx::query_scalar(
                r#"
                UPDATE autopilot_actions
                SET status='queued', approved_at=$3, approved_by='operator:relay_ladder',
                    -- O.2: an outward send waits out its hold window before a
                    -- worker may claim it — the ladder is the approval, not a
                    -- shortcut past the window that makes revoking meaningful.
                    available_at = now() + CASE
                        WHEN action_class IN ('owned_audience', 'third_party', 'paid')
                        THEN make_interval(secs => $4::double precision)
                        ELSE INTERVAL '0'
                    END
                WHERE workspace_id=$1
                  AND context='content_supply'
                  AND idempotency_key LIKE 'action:relay:' || $2::text || ':%'
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

    /// P.4: one "yes" over a show's whole growth ladder.
    ///
    /// Approving writes the live approval row every later snapshot read honours
    /// and releases the rungs already parked on the event in one statement —
    /// the thing the operator approved was the ladder, so a half-released
    /// ladder is the one state they could not reason about. The release marks
    /// `operator:show_ladder` so a revoke cancels only what the ladder freed,
    /// and applies the same outward hold an individual approval does: the
    /// window is what makes "stop" meaningful.
    pub(super) async fn approve_show_ladder_operator(
        &self,
        workspace_id: WorkspaceId,
        event_id: EventId,
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
                "approve_show_ladder",
                "event",
                event_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"requested_status": "approved"}),
            )
            .await?;
            if let Some(existing) = replay {
                // Report the live state, not the remembered one — an approve
                // replayed after a revoke must not answer "approved".
                let live = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM show_ladder_approvals \
                     WHERE workspace_id=$1 AND event_id=$2 AND revoked_at IS NULL)",
                )
                .bind(workspace_id.into_uuid())
                .bind(event_id.into_uuid())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: event_id.into_uuid(),
                    status: if live { "approved" } else { "revoked" }.to_owned(),
                    replayed: true,
                });
            }
            // A foreign or misspelt event id is a not-found, not a constraint
            // violation surfaced as a 500.
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM events WHERE workspace_id=$1 AND id=$2)",
            )
            .bind(workspace_id.into_uuid())
            .bind(event_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !exists {
                return Err(RepositoryError::NotFound);
            }
            let now = OffsetDateTime::now_utc();
            sqlx::query(
                "INSERT INTO show_ladder_approvals \
                 (workspace_id, event_id, approved_by, approved_at) \
                 VALUES ($1,$2,'admin_api_key',$3) \
                 ON CONFLICT (workspace_id, event_id) WHERE revoked_at IS NULL \
                 DO UPDATE SET approved_by='admin_api_key'",
            )
            .bind(workspace_id.into_uuid())
            .bind(event_id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let released: Vec<Uuid> = sqlx::query_scalar(
                r#"
                UPDATE autopilot_actions
                SET status='queued', approved_at=$3, approved_by='operator:show_ladder',
                    -- O.2: an outward send waits out its hold window before a
                    -- worker may claim it — the ladder is the approval, not a
                    -- shortcut past the window that makes revoking meaningful.
                    available_at = now() + CASE
                        WHEN action_class IN ('owned_audience', 'third_party', 'paid')
                        THEN make_interval(secs => $4::double precision)
                        ELSE INTERVAL '0'
                    END
                WHERE workspace_id=$1 AND context='show_growth'
                  AND subject_kind='event' AND subject_id=$2
                  AND status='awaiting_approval'
                  -- Broad ladder approval automates repeatable owned work.
                  -- Relationship-sensitive rungs always remain individually
                  -- reviewable; sparse booking history is not delegated consent.
                  AND COALESCE(payload ->> 'lever', '') NOT IN (
                      'partner_cross_promo',
                      'grassroots_scene_relay',
                      'social_proof_relay'
                  )
                  AND (approval_expires_at IS NULL OR approval_expires_at > $3)
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(event_id.into_uuid())
            .bind(now)
            .bind(OUTWARD_HOLD_SECONDS)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // A parked rung can carry an open crew assignment — close it, or a
            // reminder keeps asking somebody to approve what already queued.
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
                target_id: event_id.into_uuid(),
                status: format!("approved:{}", released.len()),
                replayed: false,
            })
        })
        .await
    }

    /// Cancels the still-queued rungs a relay-ladder approval released (P.5).
    ///
    /// "Stop the rest of this post's spread": rungs the ladder freed that
    /// have not started yet are cancelled — `operator:relay_ladder` is the
    /// whole provenance, so a rung a person approved on its own keeps its
    /// approval and a rung already running or finished keeps its record.
    /// Unlike the per-action cancel this takes a queued rung regardless of
    /// attempt count: the rung never emitted, and "stop the spread" means
    /// even the one retrying. Relay keys are versionless, so the stop is
    /// final for this post — a re-approval afterwards finds nothing parked.
    /// There is no ladder row to close: the release was point-in-time, and a
    /// second revoke honestly finds nothing left to cancel.
    pub(super) async fn revoke_relay_ladder_operator(
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
                "revoke_autopilot_relay_ladder",
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
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM content_sources \
                 WHERE workspace_id=$1 AND id=$2 AND source_kind='social_post')",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !exists {
                return Err(RepositoryError::NotFound);
            }
            let now = OffsetDateTime::now_utc();
            let cancelled: Vec<Uuid> = sqlx::query_scalar(
                "UPDATE autopilot_actions \
                 SET status='cancelled', finished_at=$3 \
                 WHERE workspace_id=$1 \
                   AND context='content_supply' \
                   AND idempotency_key LIKE 'action:relay:' || $2::text || ':%' \
                   AND status='queued' AND approved_by='operator:relay_ladder' \
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
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: source_id,
                status: format!("revoked:{}", cancelled.len()),
                replayed: false,
            })
        })
        .await
    }

    /// Withdraws the ladder approval (P.4).
    ///
    /// "Stop the remaining ladder" is what revoke means: rungs the ladder
    /// released or pre-authorized that have not started yet are cancelled —
    /// both carry the `operator:show_ladder` provenance. A rung already
    /// running or finished keeps its record, and a rung a person approved on
    /// its own is untouched. The approval row stays for the ledger;
    /// `revoked_at IS NULL` is the whole live/revoked distinction.
    pub(super) async fn revoke_show_ladder_operator(
        &self,
        workspace_id: WorkspaceId,
        event_id: EventId,
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
                "revoke_show_ladder",
                "event",
                event_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"requested_status": "revoked"}),
            )
            .await?;
            if let Some(existing) = replay {
                let live = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM show_ladder_approvals \
                     WHERE workspace_id=$1 AND event_id=$2 AND revoked_at IS NULL)",
                )
                .bind(workspace_id.into_uuid())
                .bind(event_id.into_uuid())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: event_id.into_uuid(),
                    status: if live { "approved" } else { "revoked" }.to_owned(),
                    replayed: true,
                });
            }
            // Same 404 contract as approve: a foreign or misspelt event is a
            // not-found, not a conflict that implies a ladder existed.
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM events WHERE workspace_id=$1 AND id=$2)",
            )
            .bind(workspace_id.into_uuid())
            .bind(event_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !exists {
                return Err(RepositoryError::NotFound);
            }
            let now = OffsetDateTime::now_utc();
            let revoked = sqlx::query(
                "UPDATE show_ladder_approvals \
                 SET revoked_at=$3, revoked_by='admin_api_key' \
                 WHERE workspace_id=$1 AND event_id=$2 AND revoked_at IS NULL",
            )
            .bind(workspace_id.into_uuid())
            .bind(event_id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if revoked.rows_affected() == 0 {
                return Err(RepositoryError::Conflict);
            }
            let cancelled: Vec<Uuid> = sqlx::query_scalar(
                "UPDATE autopilot_actions \
                 SET status='cancelled', finished_at=$3 \
                 WHERE workspace_id=$1 AND context='show_growth' \
                   AND subject_kind='event' AND subject_id=$2 \
                   AND status='queued' AND approved_by='operator:show_ladder' \
                 RETURNING id",
            )
            .bind(workspace_id.into_uuid())
            .bind(event_id.into_uuid())
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
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: event_id.into_uuid(),
                status: format!("revoked:{}", cancelled.len()),
                replayed: false,
            })
        })
        .await
    }
}
