//! Durable storage for live negotiations.
//!
//! Two things here are worth stating because neither is visible from the Rust.
//!
//! * **The negotiation and the show are read together.** Every rule in
//!   `crowdrelay_domain::negotiation` needs both — the ladder comes from the
//!   terms row, the refusals come from the show — and reading them in two
//!   round trips would let a counter be drafted against one moment's offer and
//!   one other moment's calendar.
//! * **A settlement is guarded on being unsettled rather than on a row count.**
//!   Two cycles racing on the same closed window must leave one recorded
//!   reason, and the first one written is the true one.

use super::*;

#[derive(sqlx::FromRow)]
struct TermsRow {
    opportunity_id: Uuid,
    state: String,
    currency: String,
    offered_fee_minor: i64,
    walk_away_minor: i64,
    target_minor: i64,
    opening_ask_minor: i64,
    countered_fee_minor: Option<i64>,
    counter_rounds: i32,
    responds_by: OffsetDateTime,
}

impl PostgresAutopilotRepository {
    /// The operator's live-opportunity policy, or the defaults.
    ///
    /// An unconfigured workspace is a real state and its defaults are honest
    /// ones: a zero minimum margin makes the floor bare cost, which the
    /// negotiation then refuses to go under. Failing the read instead would
    /// stop an operator recording an offer at all.
    pub(in crate::autopilot) async fn live_opportunity_policy(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<LiveOpportunityPolicy, RepositoryError> {
        let config = self
            .bounded(async {
                sqlx::query_scalar::<_, serde_json::Value>(
                    "SELECT config FROM viryaos_autopilot_policies \
                     WHERE workspace_id=$1 AND context='live_opportunity'",
                )
                .bind(workspace_id.into_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx)
            })
            .await?;
        let Some(config) = config else {
            return Ok(LiveOpportunityPolicy::default());
        };
        serde_json::from_value(config).map_err(|_| RepositoryError::Unexpected)
    }

    pub(super) async fn load_live_opportunity_terms_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveTermsSnapshot>, RepositoryError> {
        let rows = self
            .bounded(async {
                sqlx::query_as::<_, TermsRow>(
                    r#"
                    SELECT opportunity_id, state, currency, offered_fee_minor,
                           walk_away_minor, target_minor, opening_ask_minor,
                           countered_fee_minor, counter_rounds, responds_by
                    FROM viryaos_team_opportunity_terms
                    WHERE workspace_id = $1
                      AND settled_at IS NULL
                    ORDER BY responds_by
                    LIMIT $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(MAX_SNAPSHOTS_PER_CONTEXT)
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)
            })
            .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        // The other half of the pipeline: an opportunity is only negotiated
        // after something has been sent. Same statement as the apply read, so
        // the costing, the calendar and the scarcity arithmetic cannot drift
        // between the two paths.
        let opportunities = self
            .load_live_opportunity_snapshots_for(workspace_id, now, &["submitted", "replied"])
            .await?;
        let mut snapshots = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(opportunity) = opportunities
                .iter()
                .find(|snapshot| snapshot.opportunity_id.into_uuid() == row.opportunity_id)
            else {
                // The opportunity was withdrawn, made ineligible or never
                // reached a sent state. Skipping is right: the negotiation has
                // nothing to be judged against, and inventing a snapshot for it
                // would be judging it against defaults.
                continue;
            };
            let state = TermsState::parse(&row.state).ok_or(RepositoryError::Unexpected)?;
            snapshots.push(LiveTermsSnapshot {
                terms: TermsSnapshot {
                    opportunity_id: TeamOpportunityId::from_uuid(row.opportunity_id),
                    state,
                    offered_fee_minor: row.offered_fee_minor,
                    ladder: TermsLadder {
                        walk_away_minor: row.walk_away_minor,
                        target_minor: row.target_minor,
                        opening_ask_minor: row.opening_ask_minor,
                    },
                    countered_fee_minor: row.countered_fee_minor,
                    counter_rounds: u8::try_from(row.counter_rounds)
                        .map_err(|_| RepositoryError::Unexpected)?,
                    responds_by: row.responds_by,
                },
                opportunity: *opportunity,
                currency: row.currency,
            });
        }
        Ok(snapshots)
    }

    pub(super) async fn settle_live_opportunity_terms_impl(
        &self,
        workspace_id: WorkspaceId,
        settlement: &TermsSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        // A settlement that is not a settlement would leave `settled_at` and
        // the state disagreeing, which the schema refuses — better to refuse it
        // here, where the caller can be named.
        if !settlement.state.settled() || matches!(settlement.state, TermsState::Accepted) {
            return Err(RepositoryError::Unexpected);
        }
        self.bounded(async {
            sqlx::query(
                r#"
                UPDATE viryaos_team_opportunity_terms
                SET state = $3, settled_at = $4, settled_reason = $5, version = version + 1
                WHERE workspace_id = $1
                  AND opportunity_id = $2
                  AND settled_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(settlement.opportunity_id.into_uuid())
            .bind(settlement.state.as_str())
            .bind(now)
            // An expiry has no refusal behind it: nobody decided anything, the
            // promoter stopped waiting. `window_closed` says that rather than
            // leaving the column null, which would read as a decline whose
            // reason somebody forgot to write.
            .bind(
                settlement
                    .reason
                    .map_or("window_closed", TermsRefusal::as_str),
            )
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}
