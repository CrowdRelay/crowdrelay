//! The scout shortlist: every tracked opportunity with the link it stands
//! on, what it would cost, and the newest decision it produced.
//!
//! Two things here are worth stating because neither is visible from the
//! Rust.
//!
//! * **Rejected rows stay on the list.** `stale_reason` names why a row
//!   cannot be worked right now — no destination, a stale observation, a
//!   passed deadline, a closed or ineligible status. A shortlist that only
//!   shows the good rows makes the operator re-derive the rejections by
//!   hand, which is how a dead lead gets scouted twice.
//! * **The link is the finding.** A discovery without a usable destination is
//!   not a finding at all, so `no_destination` sorts ahead of every staleness
//!   rule: the row never had dated source evidence to go stale in the first
//!   place.

use crowdrelay_domain::{
    live_opportunities::scout_observation_stale_after,
    tour_economics::{ShowLogistics, estimate_show_cost},
};

use super::*;

#[derive(FromRow)]
struct ShortlistRow {
    opportunity_id: Uuid,
    opportunity_kind: String,
    source: String,
    external_key: String,
    title: String,
    organization: String,
    destination_url: Option<String>,
    source_observed_at: Option<OffsetDateTime>,
    deadline: Option<OffsetDateTime>,
    status: String,
    status_reason: Option<String>,
    eligible: bool,
    expected_fee_minor: i64,
    estimated_cost_minor: i64,
    application_fee_minor: i64,
    currency: String,
    distance_km: Option<i32>,
    nights_away: Option<i32>,
    fit_basis_points: i32,
    reputation_basis_points: i32,
    confidence_basis_points: i32,
    latest_decision_id: Option<Uuid>,
    latest_decision_kind: Option<String>,
    latest_decision_disposition: Option<String>,
}

/// Why a row cannot be worked right now. Order is precedence: a closed row
/// is closed regardless of its link, and a row with no destination never had
/// dated source evidence that could go stale.
fn stale_reason(row: &ShortlistRow, now: OffsetDateTime) -> Option<&'static str> {
    if matches!(row.status.as_str(), "won" | "lost" | "dismissed") {
        return Some("closed");
    }
    if !row.eligible {
        return Some("ineligible");
    }
    if row
        .destination_url
        .as_deref()
        .is_none_or(|url| url.trim().is_empty())
    {
        return Some("no_destination");
    }
    let observed = row.source_observed_at;
    if observed.is_none() || observed.is_some_and(|at| now - at > scout_observation_stale_after()) {
        return Some("stale_observation");
    }
    // A deadline that passed matters only while nothing has been sent — the
    // same rule the live evaluator applies, so the two surfaces agree on
    // which rows still count as open.
    if !matches!(row.status.as_str(), "submitted" | "replied")
        && row.deadline.is_some_and(|deadline| deadline <= now)
    {
        return Some("deadline_passed");
    }
    None
}

pub(in crate::autopilot) async fn load_opportunity_shortlist(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<crowdrelay_application::autopilot::OpportunityShortlist, RepositoryError> {
    {
        let rows = sqlx::query_as::<_, ShortlistRow>(
            r#"
            SELECT
                opportunity.id AS opportunity_id,
                opportunity.opportunity_kind,
                opportunity.source,
                opportunity.external_key,
                opportunity.title,
                opportunity.organization,
                opportunity.destination_url,
                opportunity.source_observed_at,
                opportunity.deadline,
                opportunity.status,
                opportunity.status_reason,
                opportunity.eligible,
                opportunity.expected_fee_minor,
                opportunity.estimated_cost_minor,
                opportunity.application_fee_minor,
                opportunity.currency,
                opportunity.distance_km,
                opportunity.nights_away,
                opportunity.fit_basis_points,
                opportunity.reputation_basis_points,
                opportunity.confidence_basis_points,
                decision.id AS latest_decision_id,
                decision.decision_kind AS latest_decision_kind,
                decision.disposition AS latest_decision_disposition
            FROM viryaos_team_opportunities AS opportunity
            LEFT JOIN LATERAL (
                SELECT candidate.id, candidate.decision_kind, candidate.disposition
                FROM viryaos_autopilot_decisions AS candidate
                WHERE candidate.workspace_id = opportunity.workspace_id
                  AND candidate.subject_kind = 'team_opportunity'
                  AND candidate.subject_id = opportunity.id
                ORDER BY candidate.evaluated_at DESC, candidate.id DESC
                LIMIT 1
            ) AS decision ON true
            WHERE opportunity.workspace_id = $1
            ORDER BY opportunity.created_at DESC, opportunity.id
            LIMIT $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(MAX_SNAPSHOTS_PER_CONTEXT * 4)
        .fetch_all(&repo.pool)
        .await
        .map_err(map_sqlx)?;

        // Costed from the same engine the evaluator uses: a figure typed into
        // the row is shown, but the flag says whether the number is a costed
        // trip or a guess.
        let tour_policy = repo.load_tour_economics(workspace_id).await?;

        let mut stale_count = 0_i64;
        let mut closed_count = 0_i64;
        let mut ineligible_count = 0_i64;
        let entries = rows
            .iter()
            .map(|row| {
                let reason = stale_reason(row, now);
                if reason.is_some() {
                    stale_count += 1;
                }
                if matches!(row.status.as_str(), "won" | "lost" | "dismissed") {
                    closed_count += 1;
                }
                if !row.eligible {
                    ineligible_count += 1;
                }
                let costed = estimate_show_cost(
                    &ShowLogistics {
                        distance_km: row.distance_km.and_then(|km| u32::try_from(km).ok()),
                        nights_away: row.nights_away.and_then(|nights| u8::try_from(nights).ok()),
                        offered_fee_minor: row.expected_fee_minor,
                        application_fee_minor: row.application_fee_minor,
                    },
                    &tour_policy,
                );
                Ok(
                    crowdrelay_application::autopilot::OpportunityShortlistEntry {
                        opportunity_id: row.opportunity_id,
                        kind: row.opportunity_kind.clone(),
                        source: row.source.clone(),
                        external_key: row.external_key.clone(),
                        title: row.title.clone(),
                        organization: row.organization.clone(),
                        destination_url: row.destination_url.clone(),
                        source_observed_at: row.source_observed_at,
                        deadline: row.deadline,
                        status: row.status.clone(),
                        status_reason: row.status_reason.clone(),
                        eligible: row.eligible,
                        // The columns default to 0 and cannot tell "free" from
                        // "never entered" — a scout row's money is unentered
                        // until a human types it. `None` keeps unknown reading
                        // as unknown instead of as free.
                        expected_fee_minor: (row.expected_fee_minor > 0)
                            .then_some(row.expected_fee_minor),
                        estimated_cost_minor: Some(
                            costed
                                .cost()
                                .map_or(row.estimated_cost_minor, |cost| cost.total_cost_minor),
                        )
                        .filter(|value| *value > 0),
                        application_fee_minor: (row.application_fee_minor > 0)
                            .then_some(row.application_fee_minor),
                        currency: row.currency.clone(),
                        fit_basis_points: u16::try_from(row.fit_basis_points)
                            .map_err(|_| RepositoryError::Unexpected)?,
                        reputation_basis_points: u16::try_from(row.reputation_basis_points)
                            .map_err(|_| RepositoryError::Unexpected)?,
                        confidence_basis_points: u16::try_from(row.confidence_basis_points)
                            .map_err(|_| RepositoryError::Unexpected)?,
                        stale_reason: reason.map(str::to_owned),
                        latest_decision_id: row.latest_decision_id,
                        latest_decision_kind: row.latest_decision_kind.clone(),
                        latest_decision_disposition: row.latest_decision_disposition.clone(),
                        costed_from_logistics: costed.cost().is_some(),
                    },
                )
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;

        Ok(crowdrelay_application::autopilot::OpportunityShortlist {
            generated_at: now,
            entries,
            stale_count,
            closed_count,
            ineligible_count,
            degraded: Vec::new(),
        })
    }
}
