//! Durable storage for claimed playlist placements.
//!
//! Two things here decide whether the anti-scam core is worth having.
//!
//! * **A withdrawal suppresses the operator, not the playlist.** One person
//!   often runs dozens of lists. Suppressing the one they pulled the track from
//!   and leaving the rest pitchable is how the same curator is approached again
//!   next week through a different name, which is the failure this whole phase
//!   exists to prevent. The suppression and the settlement are one transaction.
//! * **A read that failed changes nothing.** `checks_completed` only ever
//!   advances on a read that actually happened, so a dead credential cannot
//!   burn through the verification schedule and leave a claim looking checked.

use super::*;
use crowdrelay_domain::OutreachOpportunityId;

#[derive(sqlx::FromRow)]
struct PlacementRow {
    opportunity_id: Uuid,
    state: String,
    claimed_at: OffsetDateTime,
    last_observation: Option<String>,
    last_checked_at: Option<OffsetDateTime>,
    checks_completed: i16,
    playlist_external_id: String,
    track_external_id: String,
}

impl PostgresAutopilotRepository {
    pub(super) async fn load_playlist_placements_impl(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<Vec<PlaylistPlacementSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, PlacementRow>(
                r#"
                SELECT opportunity_id, state, claimed_at, last_observation, last_checked_at,
                       checks_completed, playlist_external_id, track_external_id
                FROM viryaos_playlist_placements
                WHERE workspace_id = $1
                  AND settled_at IS NULL
                ORDER BY claimed_at
                LIMIT $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let mut snapshots = Vec::with_capacity(rows.len());
            for row in rows {
                snapshots.push(PlaylistPlacementSnapshot {
                    placement: PlacementSnapshot {
                        opportunity_id: OutreachOpportunityId::from_uuid(row.opportunity_id),
                        state: PlacementState::parse(&row.state)
                            .ok_or(RepositoryError::Unexpected)?,
                        claimed_at: row.claimed_at,
                        // A stored value the domain cannot parse is a row no
                        // code path wrote, and guessing at it would decide a
                        // scam signal on a shrug.
                        last_observation: match row.last_observation.as_deref() {
                            None => None,
                            Some(value) => Some(
                                PlacementObservation::parse(value)
                                    .ok_or(RepositoryError::Unexpected)?,
                            ),
                        },
                        last_checked_at: row.last_checked_at,
                        checks_completed: u8::try_from(row.checks_completed)
                            .map_err(|_| RepositoryError::Unexpected)?,
                    },
                    playlist_external_id: row.playlist_external_id,
                    track_external_id: row.track_external_id,
                });
            }
            Ok(snapshots)
        })
        .await
    }

    pub(super) async fn settle_playlist_placement_impl(
        &self,
        workspace_id: WorkspaceId,
        settlement: PlacementSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        if !settlement.state.settled() {
            // `verified` is what a placement looks like between checkpoints,
            // and writing it as settled would stop the re-checks that make it
            // mean anything.
            return Err(RepositoryError::Unexpected);
        }
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let target_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                UPDATE viryaos_playlist_placements AS placement
                SET state = $3, settled_at = $4, version = version + 1
                FROM viryaos_outreach_opportunities AS opportunity
                WHERE placement.workspace_id = $1
                  AND placement.opportunity_id = $2
                  AND placement.settled_at IS NULL
                  AND opportunity.workspace_id = placement.workspace_id
                  AND opportunity.id = placement.opportunity_id
                RETURNING opportunity.target_id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(settlement.opportunity_id.into_uuid())
            .bind(settlement.state.as_str())
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let Some(target_id) = target_id else {
                // Another cycle settled it first. The first reason written is
                // the true one.
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(());
            };

            if suppresses_identity(settlement.state) {
                // Every target this curator runs, matched on the identity where
                // there is one and on the target itself where there is not.
                // Falling back to the single target is what keeps an unknown
                // identity from suppressing everybody with a NULL.
                sqlx::query(
                    r#"
                    UPDATE viryaos_outreach_targets AS target
                    SET do_not_contact = true,
                        accepts_outreach = false,
                        last_reply_disposition = 'do_not_contact',
                        last_reply_at = $3,
                        version = version + 1
                    WHERE target.workspace_id = $1
                      AND (
                          target.id = $2
                          OR (
                              target.curator_identity IS NOT NULL
                              AND target.curator_identity = (
                                  SELECT owner.curator_identity
                                  FROM viryaos_outreach_targets AS owner
                                  WHERE owner.workspace_id = $1 AND owner.id = $2
                              )
                          )
                      )
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(target_id)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                // Their open opportunities go with them. Leaving one active
                // would pitch a suppressed curator on the next cycle.
                sqlx::query(
                    r#"
                    UPDATE viryaos_outreach_opportunities AS opportunity
                    SET active = false
                    FROM viryaos_outreach_targets AS target
                    WHERE opportunity.workspace_id = $1
                      AND target.workspace_id = opportunity.workspace_id
                      AND target.id = opportunity.target_id
                      AND target.do_not_contact
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }

    /// Records a curator's claim, or the result of one public read of it.
    pub(super) async fn record_playlist_placement_operator(
        &self,
        workspace_id: WorkspaceId,
        command: RecordPlaylistPlacement,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        if command.playlist_external_id.trim().is_empty()
            || command.track_external_id.trim().is_empty()
        {
            return Err(RepositoryError::Conflict);
        }
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let recorded = command
                .observation
                .map_or("claimed", PlacementObservation::as_str);
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "record_autopilot_playlist_placement",
                "outreach_opportunity",
                command.opportunity_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({
                    "opportunity_id": command.opportunity_id,
                    "playlist_external_id": command.playlist_external_id,
                    "track_external_id": command.track_external_id,
                    "observation": recorded,
                }),
            )
            .await?;
            if let Some(existing) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: command.opportunity_id.into_uuid(),
                    status: recorded.into(),
                    replayed: true,
                });
            }

            let Some(observation) = command.observation else {
                // The claim. It opens the row and counts toward nothing.
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO viryaos_playlist_placements (
                        workspace_id, opportunity_id, playlist_external_id, track_external_id
                    ) VALUES ($1,$2,$3,$4)
                    ON CONFLICT (workspace_id, opportunity_id) DO NOTHING
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(command.opportunity_id.into_uuid())
                .bind(&command.playlist_external_id)
                .bind(&command.track_external_id)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if inserted.rows_affected() != 1 {
                    // A curator who claims twice is claiming about the same
                    // thing, and a second row would let one pitch be counted
                    // twice.
                    return Err(RepositoryError::Conflict);
                }
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id,
                    target_id: command.opportunity_id.into_uuid(),
                    status: recorded.into(),
                    replayed: false,
                });
            };

            let row = sqlx::query_as::<_, (String, i16)>(
                "SELECT state, checks_completed FROM viryaos_playlist_placements \
                 WHERE workspace_id=$1 AND opportunity_id=$2 AND settled_at IS NULL \
                 FOR UPDATE",
            )
            .bind(workspace_id.into_uuid())
            .bind(command.opportunity_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?;
            let current = PlacementSnapshot {
                opportunity_id: command.opportunity_id,
                state: PlacementState::parse(&row.0).ok_or(RepositoryError::Unexpected)?,
                claimed_at: OffsetDateTime::now_utc(),
                last_observation: None,
                last_checked_at: None,
                checks_completed: u8::try_from(row.1).map_err(|_| RepositoryError::Unexpected)?,
            };
            let (state, counts) = apply_observation(current, observation);
            // An unreadable read advances no checkpoint and records no
            // observation: it is evidence of nothing, and the schedule must not
            // be burned through by a dead credential.
            // The read is folded in here; a settling state is not. `state` and
            // `settled_at` have to move together — the schema refuses one
            // without the other — and the settlement path is the only thing
            // that writes both, because it is also what suppresses the curator.
            sqlx::query(
                r#"
                UPDATE viryaos_playlist_placements
                SET state = CASE WHEN $7 THEN state ELSE $3 END,
                    last_observation = CASE WHEN $4 THEN $5 ELSE last_observation END,
                    last_checked_at = CASE WHEN $4 THEN $6 ELSE last_checked_at END,
                    checks_completed = LEAST(checks_completed + CASE WHEN $4 THEN 1 ELSE 0 END, 3),
                    version = version + 1
                WHERE workspace_id = $1 AND opportunity_id = $2 AND settled_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.opportunity_id.into_uuid())
            .bind(state.as_str())
            .bind(counts)
            .bind(observation.as_str())
            .bind(OffsetDateTime::now_utc())
            .bind(state.settled())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            // Suppression runs through the settlement path, so a withdrawal
            // reported here and one the cycle found behave identically.
            if state.settled() {
                self.settle_playlist_placement_impl(
                    workspace_id,
                    PlacementSettlement {
                        opportunity_id: command.opportunity_id,
                        state,
                    },
                    OffsetDateTime::now_utc(),
                )
                .await?;
            }
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: command.opportunity_id.into_uuid(),
                status: state.as_str().into(),
                replayed: false,
            })
        })
        .await
    }
}
