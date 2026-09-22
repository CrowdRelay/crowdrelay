//! The roster's pooled portfolio read (5.1): every member act's current
//! candidate set, named, re-ranked under the organisation's own
//! `PortfolioConfig` for the `/v1/admin/roster-plan/portfolio` handler.
//!
//! Sibling of `roster_source_roi`: pooled reads live outside the autopilot
//! module because the API layer consumes them, and a pool row here is exactly
//! what the act's eval wrote — the read re-ranks, it does not re-derive.
//!
//! The re-rank is the same `PortfolioOptimizer` the act's own cycle ran, fed
//! the union of every act's pool. That is what makes the answer honest: a
//! candidate one act rejected for `max_dispatches` competes again for the
//! roster's slots, and the response says when that happened.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crowdrelay_brain::{DecisionValue, PortfolioCandidate, PortfolioConfig, PortfolioOptimizer};
use crowdrelay_domain::WorkspaceId;

use crate::organization_settings::OrganizationSettingsRepository;

/// The roster's fairness damp when the organisation has not stated one (5.2).
///
/// 0.9 — each prior dispatch this week costs an act a tenth of its next
/// candidate's marginal value. Deliberately a *roster* default rather than
/// `PortfolioConfig`'s inert 1.0: the pooled rank is the place the term does
/// work, and "unstated" cannot mean "the fairness the row was built for is
/// off". An operator who wants the pure maximiser says so — `1` validates.
pub const ROSTER_FAIRNESS_DECAY: f64 = 0.9;

/// One act's pool row as stored — the read deserializes `decision_value` and
/// `opportunity_id` back into the brain's types and re-ranks.
#[derive(Debug)]
pub struct RosterPoolRow {
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub opportunity_key: String,
    pub opportunity_id: serde_json::Value,
    pub audience_key: String,
    pub source_context: String,
    pub action_key: String,
    pub decision_value: serde_json::Value,
    pub is_experimental: bool,
    pub selected: bool,
    pub rejection_reason: Option<String>,
    pub refreshed_at: OffsetDateTime,
}

/// Every member act's current pool, named — the roster read's raw material.
/// Acts with no rows appear in the response's act list anyway (see
/// [`roster_portfolio_plan`]) — an act whose eval has never run is a fact the
/// roster needs, not a row that is missing.
pub async fn roster_pool_rows(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<RosterPoolRow>, sqlx::Error> {
    sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            serde_json::Value,
            String,
            String,
            String,
            serde_json::Value,
            bool,
            bool,
            Option<String>,
            OffsetDateTime,
        ),
    >(
        r#"
        SELECT pool.workspace_id, workspace.name, pool.opportunity_key,
               pool.opportunity_id, pool.audience_key, pool.source_context,
               pool.action_key, pool.decision_value, pool.is_experimental,
               pool.selected, pool.rejection_reason, pool.refreshed_at
        FROM portfolio_pool AS pool
        JOIN workspaces AS workspace ON workspace.id = pool.workspace_id
        WHERE workspace.organization_id = $1
        ORDER BY workspace.name, pool.opportunity_key
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(
                    workspace_id,
                    workspace_name,
                    opportunity_key,
                    opportunity_id,
                    audience_key,
                    source_context,
                    action_key,
                    decision_value,
                    is_experimental,
                    selected,
                    rejection_reason,
                    refreshed_at,
                )| {
                    RosterPoolRow {
                        workspace_id,
                        workspace_name,
                        opportunity_key,
                        opportunity_id,
                        audience_key,
                        source_context,
                        action_key,
                        decision_value,
                        is_experimental,
                        selected,
                        rejection_reason,
                        refreshed_at,
                    }
                },
            )
            .collect()
    })
}

/// One member act and the state of the pool it published.
#[derive(Debug, Serialize)]
pub struct RosterActPool {
    pub workspace_id: Uuid,
    pub name: String,
    /// Candidates the act's latest cycle ranked. Zero means the eval has not
    /// run (or found nothing) since the pool existed — `refreshed_at` says
    /// which.
    pub pool_size: u32,
    pub refreshed_at: Option<OffsetDateTime>,
    /// How many of this act's candidates the pooled rank selected.
    pub selected: u32,
}

/// One ranked slot in the pooled answer.
#[derive(Debug, Serialize)]
pub struct RosterRankedCandidate {
    pub workspace_id: Uuid,
    pub act: String,
    pub opportunity_key: String,
    pub template_id: String,
    pub target: String,
    pub audience_key: String,
    pub source_context: String,
    /// What the candidate was worth on its own, in expected Y30 fans.
    pub intrinsic_y30: f64,
    /// What it was worth inside the winning portfolio — after overlap,
    /// fatigue and the uncalibrated-bridge discount.
    pub marginal_y30: Option<f64>,
    pub resource_cost_units: f64,
    /// What the act's dispatch share cost this candidate — the visible form
    /// of the knob.
    pub fairness_adjustment: f64,
    /// The act's trailing-week dispatch count the damp read.
    pub act_recent_dispatches: u32,
    pub is_experimental: bool,
    /// What the act's own cycle did with this candidate. `false` with a
    /// `local_rejection_reason` of `max_dispatches_reached` is the pooling
    /// payoff made visible: the act ran out of slots, the roster did not.
    pub locally_selected: bool,
    pub local_rejection_reason: Option<String>,
}

/// The pooled answer: the ranked selection plus enough provenance to check it.
#[derive(Debug, Serialize)]
pub struct RosterPortfolioPlan {
    /// Every member act, pooled or not — an absent pool is reported, not
    /// silently skipped.
    pub acts: Vec<RosterActPool>,
    /// The bounds the rank actually ran under — stated org values where set,
    /// the optimizer's defaults where not, each labeled.
    pub max_dispatches: u32,
    pub max_dispatches_source: &'static str,
    pub cost_budget: f64,
    pub cost_budget_source: &'static str,
    /// Pool rows that could not be re-ranked — the count is reported because
    /// a silently dropped candidate is a leaderboard that lies by omission.
    pub undecodable_rows: u32,
    /// How many pool rows competed and lost.
    pub rejected_count: u32,
    /// The fairness damp in force — stated or the roster default, labeled —
    /// so a reader can check "the knob" rather than trust it exists (§4h-6).
    pub fairness_decay: f64,
    pub fairness_decay_source: &'static str,
    pub selected: Vec<RosterRankedCandidate>,
}

/// Pools every member act's current candidate set and re-ranks it under the
/// organisation's stated portfolio limits — or the optimizer's defaults,
/// labeled, where none are stated.
///
/// Returns `Ok(None)` when the organisation has no member workspaces at all —
/// distinct from "members exist but nobody has published a pool yet", which
/// is a real answer the response should carry.
///
/// # Errors
///
/// Propagates the database error; a single malformed `decision_value` row is
/// counted in `undecodable_rows` rather than failing the whole read.
pub async fn roster_portfolio_plan(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Option<RosterPortfolioPlan>, sqlx::Error> {
    let members = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT workspace.id, workspace.name
        FROM workspaces AS workspace
        WHERE workspace.organization_id = $1
        ORDER BY workspace.name, workspace.id
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    if members.is_empty() {
        return Ok(None);
    }

    let rows = roster_pool_rows(pool, organization_id).await?;
    let limits = OrganizationSettingsRepository::new(pool.clone())
        .roster_portfolio_limits(organization_id)
        .await?;
    let defaults = PortfolioConfig::default();
    let config = PortfolioConfig {
        max_dispatches: limits
            .max_dispatches
            .map(u32::from)
            .unwrap_or(defaults.max_dispatches),
        cost_budget: limits.cost_budget.unwrap_or(defaults.cost_budget),
        // Unstated resolves to the roster default, not the single-workspace
        // no-op: the pooled rank is exactly where the fairness term exists to
        // work, and "nobody tuned it" must not read as "every act grows,
        // nobody is neglected" being switched off.
        fairness_decay: limits.fairness_decay.unwrap_or(ROSTER_FAIRNESS_DECAY),
        ..defaults
    };

    // 5.2: the fairness term's ledger half. `autopilot_actions` is the
    // record of what each act's brain actually spent this week — the trailing
    // seven days of it is the share the damp reads. A failed send still spent
    // the slot, so the count is every created action, not the successful
    // subset.
    let act_history: std::collections::HashMap<WorkspaceId, u32> =
        sqlx::query_as::<_, (Uuid, i64)>(
            r#"
            SELECT action.workspace_id, COUNT(*)
            FROM autopilot_actions AS action
            JOIN workspaces AS workspace ON workspace.id = action.workspace_id
            WHERE workspace.organization_id = $1
              AND action.created_at > now() - INTERVAL '7 days'
            GROUP BY action.workspace_id
            "#,
        )
        .bind(organization_id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(workspace_id, count)| {
            (
                WorkspaceId::from_uuid(workspace_id),
                u32::try_from(count).unwrap_or(u32::MAX),
            )
        })
        .collect();

    let mut undecodable_rows = 0u32;
    let mut candidates: Vec<PortfolioCandidate> = Vec::with_capacity(rows.len());
    for row in &rows {
        let (Ok(opportunity_id), Ok(decision_value)) = (
            serde_json::from_value::<crowdrelay_brain::OpportunityId>(row.opportunity_id.clone()),
            serde_json::from_value::<DecisionValue>(row.decision_value.clone()),
        ) else {
            undecodable_rows += 1;
            continue;
        };
        candidates.push(PortfolioCandidate {
            opportunity_id,
            act: WorkspaceId::from_uuid(row.workspace_id),
            audience_key: row.audience_key.clone(),
            source_context: row.source_context.clone(),
            action_key: row.action_key.clone(),
            generation_signal: None,
            is_experimental: row.is_experimental,
            decision_value,
        });
    }

    let selection = PortfolioOptimizer::new(config)
        .with_act_history(act_history.clone())
        .select(candidates);
    let name_of = |id: Uuid| -> String {
        members
            .iter()
            .find(|(member, _)| *member == id)
            .map(|(_, name)| name.clone())
            .unwrap_or_default()
    };
    let local_outcome = |key: &str| -> (bool, Option<String>) {
        rows.iter()
            .find(|row| row.opportunity_key == key)
            .map(|row| (row.selected, row.rejection_reason.clone()))
            .unwrap_or((false, None))
    };
    let selected: Vec<RosterRankedCandidate> = selection
        .selected
        .iter()
        .map(|candidate| {
            let key = candidate.opportunity_id.to_string();
            let act_uuid = candidate.act.into_uuid();
            let (locally_selected, local_rejection_reason) = local_outcome(&key);
            RosterRankedCandidate {
                workspace_id: act_uuid,
                act: name_of(act_uuid),
                opportunity_key: key.clone(),
                template_id: candidate.opportunity_id.template_id.clone(),
                target: candidate.opportunity_id.target.clone(),
                audience_key: candidate.audience_key.clone(),
                source_context: candidate.source_context.clone(),
                intrinsic_y30: candidate.decision_value.total(),
                marginal_y30: selection
                    .marginal_adjustments
                    .get(&key)
                    .map(|adjustments| adjustments.marginal_y30),
                resource_cost_units: candidate.decision_value.resource_cost.units,
                fairness_adjustment: selection
                    .marginal_adjustments
                    .get(&key)
                    .map_or(0.0, |adjustments| adjustments.fairness_adjustment),
                act_recent_dispatches: act_history.get(&candidate.act).copied().unwrap_or(0),
                is_experimental: candidate.is_experimental,
                locally_selected,
                local_rejection_reason,
            }
        })
        .collect();
    let mut selected_per_act: std::collections::HashMap<Uuid, u32> =
        std::collections::HashMap::new();
    for candidate in &selected {
        *selected_per_act.entry(candidate.workspace_id).or_insert(0) += 1;
    }
    let mut pool_per_act: std::collections::HashMap<Uuid, (u32, OffsetDateTime)> =
        std::collections::HashMap::new();
    for row in &rows {
        let entry = pool_per_act
            .entry(row.workspace_id)
            .or_insert((0, row.refreshed_at));
        entry.0 += 1;
        if row.refreshed_at > entry.1 {
            entry.1 = row.refreshed_at;
        }
    }
    let acts = members
        .iter()
        .map(|(id, name)| {
            let (pool_size, refreshed_at) = pool_per_act
                .get(id)
                .map(|(size, refreshed)| (*size, Some(*refreshed)))
                .unwrap_or((0, None));
            RosterActPool {
                workspace_id: *id,
                name: name.clone(),
                pool_size,
                refreshed_at,
                selected: selected_per_act.get(id).copied().unwrap_or(0),
            }
        })
        .collect();

    Ok(Some(RosterPortfolioPlan {
        acts,
        max_dispatches: config.max_dispatches,
        max_dispatches_source: if limits.max_dispatches.is_some() {
            "stated"
        } else {
            "default"
        },
        cost_budget: config.cost_budget,
        cost_budget_source: if limits.cost_budget.is_some() {
            "stated"
        } else {
            "default"
        },
        undecodable_rows,
        rejected_count: u32::try_from(selection.rejected.len()).unwrap_or(u32::MAX),
        fairness_decay: config.fairness_decay,
        fairness_decay_source: if limits.fairness_decay.is_some() {
            "stated"
        } else {
            "roster_default"
        },
        selected,
    }))
}
