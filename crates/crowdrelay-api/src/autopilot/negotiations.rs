// Negotiations read model (P.7) — the assist surface between a promoter's
// answer and the counter. Every live terms row joined to the opportunity it
// prices, the ladder the agent argued from, and the move parked in
// `awaiting_approval` for it.
//
// Read model, not a pipeline: the ladder is written at terms-record time,
// the pending move by the evaluator, and this only reports them. The loop
// stays human at both ends — the operator records the promoter's position
// through the terms endpoint, and approves the move through the ordinary
// approval path.

/// The negotiation table: live conversations first, then the settled record.
#[derive(Debug, Serialize)]
pub struct NegotiationsView {
    /// Unsettled terms rows, soonest response deadline first.
    pub live: Vec<NegotiationEntry>,
    /// Recently settled rows — the record the next negotiation reads.
    pub settled: Vec<NegotiationEntry>,
}

#[derive(Debug, Serialize)]
pub struct NegotiationEntry {
    pub opportunity_id: Uuid,
    pub title: String,
    pub organization: String,
    /// The counterparty's own address on the tenant's opportunity — the
    /// workspace's row, so it rides like any contact the band already has.
    pub contact_email: Option<String>,
    pub opportunity_kind: String,
    /// The opportunity's own status — a negotiation on a `submitted` row is
    /// a different conversation than one on `replied`.
    pub opportunity_status: String,
    pub state: String,
    pub currency: String,
    /// What the promoter has on the table right now.
    pub offered_fee_minor: i64,
    /// The frozen ladder: below `walk_away_minor` the answer is no,
    /// `target_minor` is the fee worth playing for, `opening_ask_minor` is
    /// where the conversation opened.
    pub walk_away_minor: i64,
    pub target_minor: i64,
    pub opening_ask_minor: i64,
    /// Which input produced the walk-away — `cost`, `market`, or
    /// `counterparty_history` — and the two external numbers behind it.
    pub floor_basis: String,
    pub prior_fee_minor: Option<i64>,
    pub market_floor_minor: Option<i64>,
    /// The agent's last ask, and how many it has made.
    pub countered_fee_minor: Option<i64>,
    pub counter_rounds: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub responds_by: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub settled_at: Option<OffsetDateTime>,
    pub settled_reason: Option<String>,
    /// The move the evaluator parked for a human — a drafted counter or an
    /// accept — when one is waiting. `null` when nothing is proposed.
    pub pending_move: Option<PendingMove>,
}

#[derive(Debug, Serialize)]
pub struct PendingMove {
    pub action_id: Uuid,
    /// `counter_live_opportunity_terms` or `accept_live_opportunity_terms`.
    pub kind: String,
    /// The proposed amount — the counter's ask or the accepted fee.
    pub amount_minor: Option<i64>,
    /// Which counter round the proposal is. Zero on an accept.
    pub round: i64,
}

#[derive(Debug, FromRow)]
struct NegotiationRow {
    opportunity_id: Uuid,
    title: String,
    organization: String,
    contact_email: Option<String>,
    opportunity_kind: String,
    opportunity_status: String,
    state: String,
    currency: String,
    offered_fee_minor: i64,
    walk_away_minor: i64,
    target_minor: i64,
    opening_ask_minor: i64,
    floor_basis: String,
    prior_fee_minor: Option<i64>,
    market_floor_minor: Option<i64>,
    countered_fee_minor: Option<i64>,
    counter_rounds: i32,
    responds_by: OffsetDateTime,
    settled_at: Option<OffsetDateTime>,
    settled_reason: Option<String>,
    pending_action_id: Option<Uuid>,
    pending_action_kind: Option<String>,
    pending_amount_minor: Option<i64>,
    pending_round: Option<i64>,
}

const LIVE_LIMIT: i64 = 40;
const SETTLED_LIMIT: i64 = 20;

pub async fn negotiations(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let workspace_id = state.ops.workspace_id().into_uuid();
    let pool = &state.database;

    // The pending-move lateral reads the action queue the approval screen
    // does: one awaiting-approval terms move per negotiation, newest first.
    // action_kind carries the persisted vocabulary ('opportunity.terms.*'),
    // while the serde `kind` tag inside the payload is what the console's
    // move badge reads. Ordering partitions live-first: unsettled rows by
    // the promoter's deadline ascending, then settled rows newest-close
    // first; each partition is capped in SQL so a busy live book cannot
    // starve the record.
    let rows = sqlx::query_as::<_, NegotiationRow>(
        r#"
        SELECT ranked.opportunity_id,
               ranked.title, ranked.organization, ranked.contact_email,
               ranked.opportunity_kind, ranked.opportunity_status,
               ranked.state, ranked.currency, ranked.offered_fee_minor,
               ranked.walk_away_minor, ranked.target_minor, ranked.opening_ask_minor,
               ranked.floor_basis, ranked.prior_fee_minor, ranked.market_floor_minor,
               ranked.countered_fee_minor, ranked.counter_rounds,
               ranked.responds_by, ranked.settled_at, ranked.settled_reason,
               ranked.pending_action_id, ranked.pending_action_kind,
               ranked.pending_amount_minor, ranked.pending_round
        FROM (
            SELECT terms.opportunity_id,
                   opportunity.title, opportunity.organization, opportunity.contact_email,
                   opportunity.opportunity_kind, opportunity.status AS opportunity_status,
                   terms.state, terms.currency, terms.offered_fee_minor,
                   terms.walk_away_minor, terms.target_minor, terms.opening_ask_minor,
                   terms.floor_basis, terms.prior_fee_minor, terms.market_floor_minor,
                   terms.countered_fee_minor, terms.counter_rounds,
                   terms.responds_by, terms.settled_at, terms.settled_reason,
                   move.id AS pending_action_id,
                   move.payload ->> 'kind' AS pending_action_kind,
                   COALESCE(
                       (move.payload ->> 'ask_minor')::bigint,
                       (move.payload ->> 'fee_minor')::bigint
                   ) AS pending_amount_minor,
                   COALESCE((move.payload ->> 'round')::bigint, 0) AS pending_round,
                   ROW_NUMBER() OVER (
                       PARTITION BY (terms.settled_at IS NOT NULL)
                       ORDER BY
                           CASE WHEN terms.settled_at IS NULL THEN terms.responds_by END ASC,
                           terms.settled_at DESC,
                           terms.opportunity_id
                   ) AS bucket_rank
            FROM team_opportunity_terms AS terms
            JOIN team_opportunities AS opportunity
              ON opportunity.workspace_id = terms.workspace_id
             AND opportunity.id = terms.opportunity_id
            LEFT JOIN LATERAL (
                SELECT action.id, action.payload
                FROM autopilot_actions AS action
                WHERE action.workspace_id = terms.workspace_id
                  AND action.subject_kind = 'team_opportunity'
                  AND action.subject_id = terms.opportunity_id
                  AND action.status = 'awaiting_approval'
                  AND (action.approval_expires_at IS NULL
                       OR action.approval_expires_at > now())
                  AND action.action_kind IN (
                      'opportunity.terms.counter', 'opportunity.terms.accept',
                      'opportunity.counterparty_report.issue'
                  )
                ORDER BY action.created_at DESC, action.id DESC
                LIMIT 1
            ) AS move ON true
            WHERE terms.workspace_id = $1
              AND (terms.settled_at IS NULL
                   OR terms.settled_at > now() - interval '90 days')
        ) AS ranked
        WHERE ranked.bucket_rank <= (CASE
            WHEN ranked.settled_at IS NULL THEN $2
            ELSE $3
        END)::bigint
        ORDER BY (ranked.settled_at IS NOT NULL) ASC,
                 CASE WHEN ranked.settled_at IS NULL THEN ranked.responds_by END ASC,
                 ranked.settled_at DESC,
                 ranked.opportunity_id
        "#,
    )
    .bind(workspace_id)
    .bind(LIVE_LIMIT)
    .bind(SETTLED_LIMIT)
    .fetch_all(pool)
    .await;

    match rows {
        Ok(rows) => {
            let mut live = Vec::new();
            let mut settled = Vec::new();
            for row in rows {
                if row.settled_at.is_some() {
                    settled.push(negotiation_row_to_entry(row));
                } else {
                    live.push(negotiation_row_to_entry(row));
                }
            }
            private_json(StatusCode::OK, NegotiationsView { live, settled })
        }
        Err(_) => {
            tracing::warn!("could not load negotiations view");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

fn negotiation_row_to_entry(row: NegotiationRow) -> NegotiationEntry {
    NegotiationEntry {
        opportunity_id: row.opportunity_id,
        title: row.title,
        organization: row.organization,
        contact_email: row.contact_email,
        opportunity_kind: row.opportunity_kind,
        opportunity_status: row.opportunity_status,
        state: row.state,
        currency: row.currency,
        offered_fee_minor: row.offered_fee_minor,
        walk_away_minor: row.walk_away_minor,
        target_minor: row.target_minor,
        opening_ask_minor: row.opening_ask_minor,
        floor_basis: row.floor_basis,
        prior_fee_minor: row.prior_fee_minor,
        market_floor_minor: row.market_floor_minor,
        countered_fee_minor: row.countered_fee_minor,
        counter_rounds: row.counter_rounds,
        responds_by: row.responds_by,
        settled_at: row.settled_at,
        settled_reason: row.settled_reason,
        pending_move: row.pending_action_id.map(|action_id| PendingMove {
            action_id,
            kind: row.pending_action_kind.unwrap_or_default(),
            amount_minor: row.pending_amount_minor,
            round: row.pending_round.unwrap_or(0),
        }),
    }
}

#[cfg(test)]
mod tests {
    use crowdrelay_application::autopilot::AutopilotActionPayload;
    use crowdrelay_domain::TeamOpportunityId;

    // The lateral's `action_kind` filter must name the persisted vocabulary
    // from `AutopilotActionPayload::action_kind()` — the serde `kind` tags
    // (counter_live_opportunity_terms) never reach the column. Pin it so a
    // vocabulary drift cannot silently blank the pending move again.
    #[test]
    fn pending_move_filter_matches_persisted_action_kinds() {
        let counter = AutopilotActionPayload::CounterLiveOpportunityTerms {
            opportunity_id: TeamOpportunityId::new(),
            ask_minor: 1,
            currency: "PLN".to_string(),
            round: 1,
        };
        let accept = AutopilotActionPayload::AcceptLiveOpportunityTerms {
            opportunity_id: TeamOpportunityId::new(),
            fee_minor: 1,
            currency: "PLN".to_string(),
        };
        assert_eq!(counter.action_kind(), "opportunity.terms.counter");
        assert_eq!(accept.action_kind(), "opportunity.terms.accept");
    }
}
