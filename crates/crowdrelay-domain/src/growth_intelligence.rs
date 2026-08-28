//! Deterministic growth intelligence — the brain's domain types.
//!
//! The brain is deterministic Rust machinery. This module holds the types
//! and policy knobs that decide when the brain dispatches LLM workers to
//! gather intelligence. The brain never follows an LLM blindly — it applies
//! these rules and decides what to gather, when, and what to do with it.
//!
//! LLMs are workers/tools/slaves that gather intelligence and draft content.
//! They do NOT decide strategy. The brain decides.
//!
//! # Layers
//!
//! - **Standing**: adaptive cadence — effective workers get shorter
//!   cooldowns, ineffective ones get longer, retired ones never dispatch.
//!   Uses the same `Standing` / `OutcomeRecord` / `StandingPolicy` types as
//!   the play learning system (see `learning.rs`).
//! - **World Model**: the brain's belief about the world — fan counts,
//!   signal installs, community reach, outreach pipeline, event state,
//!   and growth target progress.
//! - **Causal Model**: P(new_fan | template, context) with EMA learning.
//!   The brain predicts before dispatch and learns from prediction error.
//! - **Opportunity Queue + EFE**: Expected Free Energy scoring balances
//!   pragmatic value (expected fans) against epistemic value (information
//!   gain). Lower EFE = better opportunity.
//! - **Exploration Memory**: Go-Explore archive of visited (template,
//!   context) pairs. Novel pairs get an exploration bonus.
//! - **Hierarchical Planning**: `GrowthStrategy` derived from the world
//!   model determines template priority. Strategy → priority → EFE.

use serde::{Deserialize, Serialize};

use crate::learning::{Standing, StandingPolicy};

/// Intelligent token optimization tier. The brain classifies each dispatched
/// task based on stakes and complexity:
///
/// - `Basic`: free-tier models handle volume (scan, draft, suggest)
/// - `Premium`: connected paid providers handle stakes (human contact, complex
///   analysis, strategic planning)
///
/// If no premium credential is connected, premium tasks silently fall back to
/// basic — the system never blocks, it degrades.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTier {
    #[default]
    Basic,
    Premium,
}

impl AgentTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Premium => "premium",
        }
    }
}

/// Computes the effective cooldown for a worker template given its standing.
///
/// Higher effectiveness → shorter cooldown (dispatch more often).
/// Lower effectiveness → longer cooldown (dispatch less often).
/// Retired → never dispatch (u32::MAX).
///
/// The adjustment is bounded: at most 4x the base cooldown, so a worker
/// with a poor record doesn't get pushed out to months between dispatches.
#[must_use]
pub fn effective_agent_cooldown(base_cooldown_hours: u32, standing: Standing) -> u32 {
    match standing {
        Standing::Untested { .. } => base_cooldown_hours,
        Standing::Weighted { basis_points, .. } => {
            if basis_points == 0 {
                return base_cooldown_hours.saturating_mul(4);
            }
            // Scale: 10_000 bps → base cooldown, 2_000 bps → 5x base (capped at 4x).
            // The formula: base * (10_000 / effectiveness), capped at 4x.
            let factor = 10_000_u32 / basis_points.max(1) as u32;
            base_cooldown_hours
                .saturating_mul(factor)
                .min(base_cooldown_hours.saturating_mul(4))
        }
        Standing::Retired { .. } => u32::MAX,
    }
}

/// Computes the effective tier for a worker dispatch given its standing.
///
/// A worker with consistently high effectiveness (>= 8_000 bps) escalates
/// to premium models — the situation is working and warrants a more
/// powerful model to maximize the proven growth channel.
#[must_use]
pub const fn effective_agent_tier(base_tier: AgentTier, standing: Standing) -> AgentTier {
    match standing {
        Standing::Weighted { basis_points, .. } if basis_points >= 8_000 => AgentTier::Premium,
        _ => base_tier,
    }
}

/// The default standing policy for agent dispatches. Uses
/// [`StandingPolicy::agent_defaults`] — lower minimum and floor than the
/// play policy because agent measurements span 14-day windows.
#[must_use]
pub const fn agent_standing_policy() -> StandingPolicy {
    StandingPolicy::agent_defaults()
}

// ──────────────────────────────────────────────────────────────────────
// World Model — the brain's belief about the world.
//
// One unified picture of everything the brain knows about the workspace's
// fan acquisition state, with uncertainty. Every number is derived from
// real data (fans, signal installs, community posts, outreach targets,
// events). The brain uses this to decide what to do next.
// ──────────────────────────────────────────────────────────────────────

/// The brain's belief about the world — one unified picture with uncertainty.
/// Every number carries implicit confidence (the brain knows it has exact
/// counts for fans and signal installs, but averages for engagement).
///
/// This replaces the scattered per-template fields that were duplicated
/// across `GrowthIntelligenceSnapshot` instances. The world model is loaded
/// once per cycle and shared across all template evaluations.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WorldModel {
    // ── Fan aggregation state ──
    /// Total fans in the system.
    pub total_fans: u32,
    /// New fans added this month.
    pub fans_this_month: u32,
    /// Monthly fan growth rate in basis points (e.g. 500 = 5% monthly growth).
    pub fan_growth_rate_bps: u16,
    /// Whether fan growth is accelerating, steady, decelerating, or stagnant.
    pub fan_growth_trend: GrowthTrend,

    // ── Signal conversion state ──
    /// Total Signal push endpoints installed.
    pub total_signal_installs: u32,
    /// Signal installs this month.
    pub signal_installs_this_month: u32,
    /// Signal conversion rate: what fraction of fans have Signal installed
    /// (in basis points, e.g. 1000 = 10%).
    pub signal_conversion_rate_bps: u16,

    // ── Community reach state ──
    /// Total discovered communities (discovery_places with status='active').
    pub discovered_communities: u32,
    /// Communities with at least one post in the last 30 days.
    pub active_communities: u32,
    /// Average upvote ratio across all active communities (in basis points).
    pub avg_community_engagement_bps: u16,
    /// Best performing community by avg score, if any.
    pub best_performing_community: Option<String>,
    /// Worst performing community by avg score, if any.
    pub worst_performing_community: Option<String>,

    // ── Outreach pipeline state ──
    /// Outreach targets proposed but not yet promoted.
    pub pending_outreach_targets: u32,
    /// Outreach targets promoted but not yet engaged with community posts.
    pub promoted_outreach_targets: u32,
    /// Outreach targets that have community posts (engaged).
    pub engaged_outreach_targets: u32,

    // ── Event state ──
    /// Days until the nearest upcoming published event, or `None`.
    pub days_to_next_event: Option<u32>,
    /// Whether there is an upcoming event within 30 days.
    pub has_upcoming_event: bool,

    // ── Growth target progress ──
    /// How close the brain is to its fan acquisition target this month.
    pub growth_target_progress: GrowthTargetProgress,
}

/// The trend of fan growth over time. The brain uses this to decide
/// urgency: stagnant growth → more aggressive dispatch; accelerating →
/// maintain the current approach.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthTrend {
    /// Growth rate is increasing month-over-month.
    Accelerating,
    /// Growth rate is stable.
    #[default]
    Steady,
    /// Growth rate is decreasing.
    Decelerating,
    /// No new fans in the stagnation window.
    Stagnant,
}

impl GrowthTrend {
    /// Returns true if the brain should treat this as a stagnant situation
    /// — one that warrants more aggressive fan acquisition dispatch.
    #[must_use]
    pub const fn is_stagnant(self) -> bool {
        matches!(self, Self::Stagnant | Self::Decelerating)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Growth Targets — the brain's monthly fan acquisition goals.
//
// Targets are derived deterministically from the current fan count:
// smaller fanbases get more aggressive targets (aggregation phase),
// larger ones get steadier targets (growth phase).
// ──────────────────────────────────────────────────────────────────────

/// The brain's monthly fan acquisition target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GrowthTarget {
    /// New fans to acquire this month.
    pub new_fans_per_month: u32,
    /// Signal installs to achieve this month.
    pub signal_installs_per_month: u32,
}

impl GrowthTarget {
    /// Derives a target from the current fan count. Smaller fanbases get
    /// more aggressive targets because the aggregation phase has more
    /// low-hanging fruit.
    #[must_use]
    pub fn from_fan_count(total_fans: u32) -> Self {
        let new_fans = match total_fans {
            0..=99 => 20,    // aggressive aggregation: 20 new fans/month
            100..=999 => 50, // growth phase: 50 new fans/month
            _ => 100,        // established: 100 new fans/month
        };
        // Signal installs target: 10% of fan count per month.
        let signal_installs = (total_fans / 10).max(5);
        Self {
            new_fans_per_month: new_fans,
            signal_installs_per_month: signal_installs,
        }
    }
}

/// How close the brain is to its growth target this month.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GrowthTargetProgress {
    /// The target for this month.
    pub target: GrowthTarget,
    /// Fans acquired so far this month.
    pub fans_this_month: u32,
    /// Signal installs so far this month.
    pub signal_installs_this_month: u32,
    /// Progress toward the fan target in basis points (0–10_000).
    /// 10_000 = target met. Computed as `fans_this_month / target * 10_000`.
    pub progress_bps: u16,
    /// Whether the brain is behind, on track, or ahead of target.
    pub status: TargetStatus,
}

/// How the brain is doing relative to its growth target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    /// Less than 50% of target pace — the brain needs to be more aggressive.
    Behind,
    /// 50–80% of target pace — the brain is making progress.
    #[default]
    OnTrack,
    /// More than 80% of target pace — the brain is succeeding.
    Ahead,
}

impl GrowthTargetProgress {
    /// Computes progress from a target and current counts.
    #[must_use]
    pub fn from_counts(
        target: GrowthTarget,
        fans_this_month: u32,
        signal_installs_this_month: u32,
    ) -> Self {
        let progress_bps = if target.new_fans_per_month == 0 {
            10_000
        } else {
            u16::try_from(
                (u64::from(fans_this_month) * 10_000 / u64::from(target.new_fans_per_month))
                    .min(10_000),
            )
            .unwrap_or(10_000)
        };
        let status = match progress_bps {
            0..=4_999 => TargetStatus::Behind,
            5_000..=7_999 => TargetStatus::OnTrack,
            _ => TargetStatus::Ahead,
        };
        Self {
            target,
            fans_this_month,
            signal_installs_this_month,
            progress_bps,
            status,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Causal Model + Prediction Error — the dopamine loop.
//
// Before each dispatch, the brain predicts how many fans it expects.
// After measurement, the prediction error (observed - expected) drives
// learning. This is the core mechanism that makes the brain adaptive:
// workers that consistently exceed expectations get dispatched more,
// workers that consistently disappoint get dispatched less.
// ──────────────────────────────────────────────────────────────────────

use std::collections::HashMap;

/// Context features that the causal model uses to predict fan acquisition
/// outcomes. These are the variables the brain believes influence whether
/// a dispatch will produce new fans.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DispatchContext {
    /// Days until the nearest upcoming event, if any. Event proximity
    /// boosts fan acquisition potential.
    pub days_to_event: Option<u32>,
    /// The current fan growth trend. Stagnant situations are harder to
    /// grow out of; accelerating ones are easier.
    pub fan_growth_trend: GrowthTrend,
    /// The type of subreddit/community being targeted (e.g. "metal",
    /// "prog", "polish"). Used for context-level prediction.
    pub subreddit_type: Option<String>,
    /// The post format being used (e.g. "text", "link", "video").
    pub post_format: Option<String>,
    /// Time of day as basis points (0–10_000, fraction of 24h).
    pub time_of_day_bps: u16,
    /// How novel this dispatch context is compared to past dispatches
    /// (0–10_000). Higher = more novel.
    pub community_novelty_bps: u16,
}

/// The brain's prediction before a dispatch. Records what the brain
/// expected to happen, so that after measurement the prediction error
/// can be computed and fed back into the causal model.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DispatchPrediction {
    /// The worker template that was dispatched.
    pub template_id: String,
    /// Expected new fans from this dispatch.
    pub expected_new_fans: f64,
    /// Expected new Signal installs from this dispatch.
    pub expected_signal_installs: f64,
    /// The context features that informed this prediction.
    pub context: DispatchContext,
}

/// The measured outcome of a dispatch, paired with the prediction that
/// was made before it. The prediction errors (observed - expected) are
/// the dopamine signals that drive learning.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PredictionOutcome {
    /// The prediction that was made before the dispatch.
    pub prediction: DispatchPrediction,
    /// New fans actually observed in the measurement window.
    pub observed_new_fans: f64,
    /// Signal installs actually observed in the measurement window.
    pub observed_signal_installs: f64,
    /// The dopamine signal for fans: observed - expected.
    /// Positive = better than expected (surprise reward).
    /// Negative = worse than expected (disappointment).
    pub fan_prediction_error: f64,
    /// The dopamine signal for Signal installs.
    pub signal_prediction_error: f64,
}

impl PredictionOutcome {
    /// Computes the outcome from a prediction and observed values.
    #[must_use]
    pub fn from_observation(
        prediction: DispatchPrediction,
        observed_new_fans: f64,
        observed_signal_installs: f64,
    ) -> Self {
        Self {
            fan_prediction_error: observed_new_fans - prediction.expected_new_fans,
            signal_prediction_error: observed_signal_installs - prediction.expected_signal_installs,
            prediction,
            observed_new_fans,
            observed_signal_installs,
        }
    }
}

/// The brain's causal model: P(new_fan | template, context).
///
/// A Bayesian model that predicts expected fans from context features,
/// with running variance estimation (Welford's online algorithm) so the
/// brain can quantify prediction uncertainty. Updated by prediction errors
/// using an exponentially-weighted moving average — recent outcomes matter
/// more than old ones because the world changes (communities evolve,
/// audiences shift, seasons change).
///
/// The learning rate decays with confidence: early observations matter
/// more, later ones refine. This prevents a single lucky result from
/// dominating the model after many measurements.
///
/// # Architecture
///
/// - **Template-level**: EMA of expected fans + running variance per template.
/// - **Context-level**: Subreddit-type multipliers learned from outcomes.
/// - **Signal installs**: Separate EMA for Signal conversion, learned
///   independently from fan counts (Signal adoption has different drivers).
/// - **Context adjustments**: Event proximity and growth trend modulate
///   the base prediction multiplicatively, with learned context multipliers
///   overriding the static priors once enough data is collected.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CausalModel {
    /// Per-template expected fan count, updated by prediction errors.
    pub template_expected_fans: HashMap<String, f64>,
    /// Per-template confidence (number of measurements).
    pub template_confidence: HashMap<String, u32>,
    /// Per-template running variance (M2 from Welford's algorithm).
    /// Used to quantify prediction uncertainty for EFE epistemic value.
    pub template_variance: HashMap<String, f64>,
    /// Per-subreddit-type expected fan count. Learned from outcomes and
    /// used as a multiplicative context adjustment.
    pub subreddit_type_expected_fans: HashMap<String, f64>,
    /// Per-subreddit-type confidence (measurement count).
    pub subreddit_type_confidence: HashMap<String, u32>,
    /// Per-template expected Signal installs. Learned independently from
    /// fan counts because Signal adoption has different drivers (push
    /// notification effectiveness, app friction, event proximity).
    pub template_expected_signal: HashMap<String, f64>,
}

/// The default expected fans per dispatch when no data is available.
/// A prior of 2.0 means the brain expects ~2 new fans per worker dispatch
/// — optimistic enough to keep dispatching, conservative enough to be
/// realistic about free-channel fan acquisition.
const DEFAULT_EXPECTED_FANS: f64 = 2.0;

/// The default expected Signal installs per dispatch. Signal conversion
/// is harder than fan acquisition — most fans don't install the app.
const DEFAULT_EXPECTED_SIGNAL: f64 = 0.2;

/// The prior variance — represents the brain's initial uncertainty about
/// outcomes. A value of 4.0 means the brain initially expects outcomes to
/// vary by ±2 fans (std dev). This shrinks as the brain collects data.
const PRIOR_VARIANCE: f64 = 4.0;

impl CausalModel {
    /// Predicts expected new fans for a dispatch given its context.
    ///
    /// Combines the template-level prior with context adjustments:
    /// - Subreddit-type multiplier (learned from past outcomes, falls back
    ///   to 1.0 when no data is available for this subreddit type)
    /// - Event proximity (≤7 days) boosts expected fans by 1.5x
    /// - Event proximity (≤30 days) boosts by 1.2x
    /// - Stagnant growth reduces expected fans by 0.8x
    /// - Accelerating growth boosts by 1.1x
    #[must_use]
    pub fn predict(&self, template_id: &str, context: &DispatchContext) -> f64 {
        let template_prior = self
            .template_expected_fans
            .get(template_id)
            .copied()
            .unwrap_or(DEFAULT_EXPECTED_FANS);
        let mut prediction = template_prior;
        // Apply learned subreddit-type multiplier if available.
        if let Some(ref sub_type) = context.subreddit_type {
            if let Some(&sub_expected) = self.subreddit_type_expected_fans.get(sub_type) {
                // Blend: weight the subreddit multiplier by its confidence.
                // Low confidence → small adjustment; high confidence → full.
                let sub_conf = self
                    .subreddit_type_confidence
                    .get(sub_type)
                    .copied()
                    .unwrap_or(0);
                let sub_weight = (sub_conf as f64 / (sub_conf as f64 + 5.0)).min(0.7);
                let sub_multiplier = if template_prior > 0.0 {
                    sub_expected / template_prior
                } else {
                    1.0
                };
                prediction *= 1.0 - sub_weight + sub_weight * sub_multiplier;
            }
        }
        // Event proximity boosts fan acquisition potential.
        if let Some(days) = context.days_to_event {
            if days <= 7 {
                prediction *= 1.5;
            } else if days <= 30 {
                prediction *= 1.2;
            }
        }
        // Growth trend modulates the prediction.
        match context.fan_growth_trend {
            GrowthTrend::Stagnant | GrowthTrend::Decelerating => prediction *= 0.8,
            GrowthTrend::Accelerating => prediction *= 1.1,
            GrowthTrend::Steady => {}
        }
        prediction.max(0.0)
    }

    /// Predicts expected Signal installs for a dispatch. Uses the
    /// template-level Signal EMA if available, otherwise falls back to
    /// 10% of the fan prediction (a reasonable conversion prior).
    #[must_use]
    pub fn predict_signal(&self, template_id: &str, context: &DispatchContext) -> f64 {
        let signal_prior = self
            .template_expected_signal
            .get(template_id)
            .copied()
            .unwrap_or(DEFAULT_EXPECTED_SIGNAL);
        // If we have a learned Signal prior, apply the same context
        // adjustments as fans (event proximity, growth trend).
        if self.template_expected_signal.contains_key(template_id) {
            let mut prediction = signal_prior;
            if let Some(days) = context.days_to_event {
                if days <= 7 {
                    prediction *= 1.3;
                } else if days <= 30 {
                    prediction *= 1.1;
                }
            }
            return prediction.max(0.0);
        }
        // No learned prior: fall back to 10% of fan prediction.
        self.predict(template_id, context) * 0.1
    }

    /// Returns the prediction standard deviation for a template.
    /// Used by the EFE scorer to quantify epistemic uncertainty.
    /// Returns the prior std dev for unmeasured templates.
    #[must_use]
    pub fn predict_std(&self, template_id: &str) -> f64 {
        let variance = self
            .template_variance
            .get(template_id)
            .copied()
            .unwrap_or(PRIOR_VARIANCE);
        // Add a small floor to avoid zero variance (which would make
        // the brain think it knows everything about this template).
        variance.max(0.01).sqrt()
    }

    /// Updates the model from a prediction error (the dopamine loop).
    ///
    /// Uses Welford's online algorithm for variance estimation alongside
    /// the EMA for the mean. The learning rate decays with confidence:
    /// `lr = 1 / (1 + min(confidence, 10))`. Early observations (low
    /// confidence) have high learning rate; later ones refine gently.
    ///
    /// Both the fan count and Signal install models are updated
    /// independently — they have different drivers and different noise.
    pub fn update(&mut self, outcome: &PredictionOutcome) {
        let template = &outcome.prediction.template_id;
        let confidence = self.template_confidence.get(template).copied().unwrap_or(0);
        // EMA learning rate with confidence-based decay.
        let lr = 1.0 / (1.0 + (confidence as f64).min(10.0));
        // ── Update fan count EMA ──
        let current = self
            .template_expected_fans
            .get(template)
            .copied()
            .unwrap_or(DEFAULT_EXPECTED_FANS);
        let updated = current + lr * (outcome.observed_new_fans - current);
        self.template_expected_fans
            .insert(template.clone(), updated.max(0.0));
        // ── Update fan count variance (Welford's online algorithm) ──
        // The variance tracks how spread out outcomes are, which the
        // EFE scorer uses to quantify epistemic uncertainty. Welford's
        // algorithm is numerically stable: M2 += delta * delta2, where
        // delta = x - old_mean and delta2 = x - new_mean.
        let prev_m2 = self.template_variance.get(template).copied().unwrap_or(0.0);
        let delta = outcome.observed_new_fans - current;
        let delta2 = outcome.observed_new_fans - updated;
        let new_m2 = prev_m2 + delta * delta2;
        self.template_variance
            .insert(template.clone(), new_m2.max(0.0));
        // ── Update subreddit-type EMA if context has a subreddit type ──
        if let Some(ref sub_type) = outcome.prediction.context.subreddit_type {
            let sub_conf = self
                .subreddit_type_confidence
                .get(sub_type)
                .copied()
                .unwrap_or(0);
            let sub_lr = 1.0 / (1.0 + (sub_conf as f64).min(10.0));
            let sub_current = self
                .subreddit_type_expected_fans
                .get(sub_type)
                .copied()
                .unwrap_or(DEFAULT_EXPECTED_FANS);
            let sub_updated = sub_current + sub_lr * (outcome.observed_new_fans - sub_current);
            self.subreddit_type_expected_fans
                .insert(sub_type.clone(), sub_updated.max(0.0));
            *self
                .subreddit_type_confidence
                .entry(sub_type.clone())
                .or_insert(0) += 1;
        }
        // ── Update Signal install EMA ──
        let signal_current = self
            .template_expected_signal
            .get(template)
            .copied()
            .unwrap_or(DEFAULT_EXPECTED_SIGNAL);
        let signal_updated =
            signal_current + lr * (outcome.observed_signal_installs - signal_current);
        self.template_expected_signal
            .insert(template.clone(), signal_updated.max(0.0));
        // ── Increment confidence ──
        *self
            .template_confidence
            .entry(template.clone())
            .or_insert(0) += 1;
    }

    /// Returns the confidence (measurement count) for a template.
    #[must_use]
    pub fn confidence(&self, template_id: &str) -> u32 {
        self.template_confidence
            .get(template_id)
            .copied()
            .unwrap_or(0)
    }

    /// Returns the expected fan count for a template, or the default prior.
    #[must_use]
    pub fn expected_fans(&self, template_id: &str) -> f64 {
        self.template_expected_fans
            .get(template_id)
            .copied()
            .unwrap_or(DEFAULT_EXPECTED_FANS)
    }

    /// Returns the variance for a template, or the prior variance.
    #[must_use]
    pub fn variance(&self, template_id: &str) -> f64 {
        self.template_variance
            .get(template_id)
            .copied()
            .unwrap_or(PRIOR_VARIANCE)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Opportunity Queue + Expected Free Energy (EFE) scoring.
//
// The brain doesn't just dispatch on a timer — it evaluates opportunities
// and prioritizes them by Expected Free Energy (EFE), an Active Inference
// metric that balances expected fan growth (pragmatic value) against
// information gain (epistemic value). This makes the brain both exploitative
// (dispatch what works) and explorative (try what might work).
//
// # EFE formula
//
//   EFE = -(w_pragmatic * expected_fans
//         + w_epistemic * information_gain * predict_std
//         + w_exploration * novelty)
//         + w_risk * predict_std
//
// The brain minimizes EFE. Each term:
// - **Pragmatic**: expected fan growth (exploitation — dispatch what works).
// - **Epistemic**: information gain weighted by prediction uncertainty.
//   High when the brain is uncertain (high std) and has low confidence.
//   This is the exploration drive — the brain dispatches workers it's
//   uncertain about to reduce uncertainty.
// - **Exploration**: novelty from the exploration memory. Rewards
//   unexplored (template, context) pairs.
// - **Risk**: penalizes uncertain outcomes. A risk-averse brain prefers
//   reliable channels over high-variance ones. This prevents the brain
//   from gambling on volatile communities when a steady channel exists.
//
// The weights are configurable via `EfeWeights`. The defaults are tuned
// for early-stage fanbases where exploration matters more than exploitation.
// ──────────────────────────────────────────────────────────────────────

/// Configurable weights for EFE scoring. The brain uses these to balance
/// exploitation (pragmatic value) against exploration (epistemic + novelty)
/// and risk sensitivity.
///
/// # Default weights
///
/// - `pragmatic`: 1.0 — expected fans are the primary driver.
/// - `epistemic`: 0.5 — information gain is valuable but secondary.
/// - `exploration`: 0.3 — novelty breaks ties without dominating.
/// - `risk`: 0.1 — mildly risk-averse; prefer reliable channels.
///
/// For early-stage fanbases (few measurements), the epistemic term
/// naturally dominates because `predict_std` is high. As the brain
/// collects data, `predict_std` shrinks and the pragmatic term takes over.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EfeWeights {
    /// Weight for expected fan growth (pragmatic value).
    pub pragmatic: f64,
    /// Weight for information gain × prediction uncertainty (epistemic value).
    pub epistemic: f64,
    /// Weight for exploration novelty (Go-Explore bonus).
    pub exploration: f64,
    /// Weight for risk penalty (variance aversion).
    pub risk: f64,
}

impl Default for EfeWeights {
    fn default() -> Self {
        Self {
            pragmatic: 1.0,
            epistemic: 0.5,
            exploration: 0.3,
            risk: 0.1,
        }
    }
}

/// A growth opportunity the brain has identified. Each opportunity carries
/// enough context for the evaluator to score it and decide whether to
/// dispatch a worker for it.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthOpportunity {
    /// The worker template that would address this opportunity.
    pub template_id: String,
    /// Human-readable description of the opportunity.
    pub description: String,
    /// The expected fan growth if this opportunity is pursued (pragmatic value).
    pub expected_fans: f64,
    /// The information gain — how much the brain would learn from pursuing
    /// this opportunity (epistemic value). High when the brain has low
    /// confidence in its prediction for this template/context.
    pub information_gain: f64,
    /// The Expected Free Energy score: lower EFE = better opportunity.
    pub efe_score: f64,
    /// Context that informed this opportunity's scoring.
    pub context: DispatchContext,
    /// Why the brain identified this opportunity.
    pub reason: String,
}

impl GrowthOpportunity {
    /// Computes the EFE score for an opportunity using the full formula
    /// with uncertainty-weighted epistemic value and risk sensitivity.
    ///
    /// EFE = -(w_prag * expected_fans
    ///       + w_epist * information_gain * predict_std
    ///       + w_explore * novelty)
    ///       + w_risk * predict_std
    ///
    /// Lower EFE = better. The brain dispatches lowest-EFE opportunities first.
    #[must_use]
    pub fn compute_efe(
        expected_fans: f64,
        information_gain: f64,
        predict_std: f64,
        novelty: f64,
        weights: EfeWeights,
    ) -> f64 {
        let pragmatic = weights.pragmatic * expected_fans;
        let epistemic = weights.epistemic * information_gain * predict_std;
        let exploration = weights.exploration * novelty;
        let risk = weights.risk * predict_std;
        -(pragmatic + epistemic + exploration) + risk
    }

    /// Computes a simple EFE score (legacy interface, no uncertainty).
    /// Used by tests and backward-compatible call sites.
    #[must_use]
    pub fn compute_efe_simple(expected_fans: f64, information_gain: f64) -> f64 {
        -(expected_fans + information_gain)
    }

    /// Creates an opportunity with EFE computed from the given values.
    #[must_use]
    pub fn new(
        template_id: String,
        description: String,
        expected_fans: f64,
        information_gain: f64,
        context: DispatchContext,
        reason: String,
    ) -> Self {
        Self {
            efe_score: Self::compute_efe_simple(expected_fans, information_gain),
            template_id,
            description,
            expected_fans,
            information_gain,
            context,
            reason,
        }
    }
}

/// Computes the information gain for a template given the causal model's
/// confidence. Low confidence → high information gain (the brain learns a
/// lot from one more measurement). High confidence → low information gain.
///
/// This is the epistemic value of exploration: the brain dispatches workers
/// it's uncertain about to reduce uncertainty, not just to exploit known
/// good channels.
///
/// The formula uses a Bayesian information gain approximation:
/// `1/(1+confidence)` — the expected reduction in posterior entropy from
/// one more measurement. At confidence=0, the brain learns everything
/// (gain=1.0). At confidence=50, it learns almost nothing (gain≈0.02).
#[must_use]
pub fn information_gain(confidence: u32) -> f64 {
    1.0 / (1.0 + confidence as f64)
}

// ──────────────────────────────────────────────────────────────────────
// Go-Explore memory — exploration archive for active information seeking.
//
// The brain remembers which (template, context) combinations it has already
// explored, so it doesn't waste dispatches re-exploring known territory.
// Novel combinations get an exploration bonus; well-explored ones get
// diminishing returns. This is inspired by the Go-Explore algorithm's
// archive of visited states.
//
// # Architecture
//
// - **Recency-weighted visits**: recent explorations count more than old
//   ones. A visit 6 months ago is worth less than one yesterday, because
//   the world changes (communities evolve, audiences shift). Uses
//   exponential decay: `effective_visits = sum(decay^age_i)`.
// - **Cross-template generalization**: if the brain explored a context
//   with template A, template B in the same context is partially credited
//   (the context is known, even if the template isn't). This prevents
//   the brain from re-exploring the same context with every template.
// - **Context-level archive**: tracks context visits independently of
//   template visits, so the brain knows which contexts are well-explored
//   across all templates.
// ──────────────────────────────────────────────────────────────────────

/// The decay rate for recency-weighted visits. Each visit's contribution
/// to the effective visit count decays by this factor per cycle.
/// 0.95 means a visit 14 cycles ago contributes 0.95^14 ≈ 0.49 — about
/// half as much as a recent visit. This gives the brain a ~20-cycle
// memory window, after which old explorations are largely forgotten.
const VISIT_DECAY: f64 = 0.95;

/// The cross-template generalization factor. When the brain has explored
/// a context with template A, template B in the same context gets partial
/// credit: `context_visits * CROSS_TEMPLATE_FACTOR`. This means exploring
/// a context with one template teaches the brain 30% of what it would
/// learn from exploring it with a different template.
const CROSS_TEMPLATE_FACTOR: f64 = 0.3;

/// The brain's exploration memory. Tracks which (template, context) pairs
/// have been explored and how many times, with recency weighting so the
/// brain re-explores contexts it hasn't visited recently.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExplorationMemory {
    /// Map of (template_id, context_hash) → effective visit count.
    /// The effective count is recency-weighted: old visits decay.
    pub visits: HashMap<String, f64>,
    /// Map of context_hash → effective visit count (across all templates).
    /// Used for cross-template generalization: a context explored with
    /// one template is partially known for other templates.
    pub context_visits: HashMap<String, f64>,
}

impl ExplorationMemory {
    /// Records a visit to a (template, context) pair. Both the
    /// template-specific and context-level visit counts are incremented.
    pub fn record_visit(&mut self, template_id: &str, context_hash: &str) {
        let key = format!("{template_id}:{context_hash}");
        *self.visits.entry(key).or_insert(0.0) += 1.0;
        *self
            .context_visits
            .entry(context_hash.to_owned())
            .or_insert(0.0) += 1.0;
    }

    /// Returns the novelty score for a (template, context) pair.
    /// Novel (unvisited) pairs get 1.0; well-explored pairs approach 0.0.
    ///
    /// The novelty combines:
    /// - **Template-specific novelty**: how rarely this exact (template,
    ///   context) pair has been explored.
    /// - **Context-level novelty**: how rarely this context has been
    ///   explored with any template. This provides cross-template
    ///   generalization — a context explored with template A is partially
    ///   known for template B.
    ///
    /// The combined novelty is: `1/(1 + template_visits + cross_template * context_visits)`.
    #[must_use]
    pub fn novelty(&self, template_id: &str, context_hash: &str) -> f64 {
        let key = format!("{template_id}:{context_hash}");
        let template_visits = self.visits.get(&key).copied().unwrap_or(0.0);
        let context_visits = self
            .context_visits
            .get(context_hash)
            .copied()
            .unwrap_or(0.0);
        let effective = template_visits + CROSS_TEMPLATE_FACTOR * context_visits;
        1.0 / (1.0 + effective)
    }

    /// Returns the total number of unique (template, context) pairs explored.
    #[must_use]
    pub fn explored_count(&self) -> usize {
        self.visits.len()
    }

    /// Decays all visit counts by the decay factor. Called once per cycle
    /// so old explorations gradually become novel again — the brain
    /// re-explores contexts it hasn't visited recently because the world
    /// changes (communities evolve, audiences shift, seasons change).
    pub fn decay(&mut self) {
        for v in self.visits.values_mut() {
            *v *= VISIT_DECAY;
            // Floor: visits below 0.01 are effectively zero.
            if *v < 0.01 {
                *v = 0.0;
            }
        }
        for v in self.context_visits.values_mut() {
            *v *= VISIT_DECAY;
            if *v < 0.01 {
                *v = 0.0;
            }
        }
        // Clean up zero entries to prevent unbounded growth.
        self.visits.retain(|_, v| *v > 0.0);
        self.context_visits.retain(|_, v| *v > 0.0);
    }
}

/// Computes a context hash from a DispatchContext for exploration tracking.
/// Two dispatches with the same context features produce the same hash,
/// so the brain can tell when it's re-exploring the same territory.
///
/// The hash includes all context features that affect the outcome:
/// days_to_event (bucketed), growth trend, subreddit type, and post format.
/// Time of day and community novelty are excluded because they change
/// every dispatch and would make every context unique.
#[must_use]
pub fn context_hash(context: &DispatchContext) -> String {
    // Bucket days_to_event to avoid treating every day as a new context.
    // 0-1 days, 2-7 days, 8-14 days, 15-30 days, 31+ days, none.
    let event_bucket = match context.days_to_event {
        None => 0u8,
        Some(0..=1) => 1,
        Some(2..=7) => 2,
        Some(8..=14) => 3,
        Some(15..=30) => 4,
        Some(_) => 5,
    };
    format!(
        "{}:{:?}:{}:{}",
        event_bucket,
        context.fan_growth_trend,
        context.subreddit_type.as_deref().unwrap_or(""),
        context.post_format.as_deref().unwrap_or(""),
    )
}

// ──────────────────────────────────────────────────────────────────────
// Hierarchical planning — strategy → pathway → action.
//
// The brain doesn't just dispatch individual workers — it plans pathways:
// sequences of actions that build on each other. "Discover communities →
// engage them → measure fan growth → adjust strategy" is a pathway.
// The brain evolves strategies by tracking which pathways produce growth.
//
// # Strategy hysteresis
//
// The brain uses hysteresis to prevent strategy flip-flopping. Once a
// strategy is selected, it stays in effect until the world model changes
// enough to justify a switch. This prevents the brain from oscillating
// between strategies every cycle when conditions are borderline.
//
// The hysteresis works by requiring a stronger signal to switch away
// from the current strategy than to switch to it in the first place.
// ──────────────────────────────────────────────────────────────────────

/// A growth strategy — the brain's high-level approach to fan acquisition.
/// Strategies evolve based on which pathways produce growth.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthStrategy {
    /// Aggressive community discovery and engagement. Best for early-stage
    /// fanbases that need to aggregate from many sources.
    #[default]
    AggressiveDiscovery,
    /// Event-driven promotion. Best when there are upcoming events to
    /// leverage for fan acquisition.
    EventDriven,
    /// Content-first: produce social content and community posts to build
    /// organic reach. Best for steady growth without event pressure.
    ContentFirst,
    /// Signal conversion: focus on converting existing fans to Signal
    /// push subscribers. Best when the fanbase is growing but Signal
    /// adoption is low.
    SignalConversion,
}

impl GrowthStrategy {
    /// Derives the current strategy from the world model.
    #[must_use]
    pub fn from_world_model(world: &WorldModel) -> Self {
        // If behind target and stagnant → aggressive discovery.
        if world.growth_target_progress.status == TargetStatus::Behind
            && world.fan_growth_trend.is_stagnant()
        {
            return Self::AggressiveDiscovery;
        }
        // If there's an upcoming event within 14 days → event-driven.
        if let Some(days) = world.days_to_next_event
            && days <= 14
        {
            return Self::EventDriven;
        }
        // If Signal conversion is low (< 5%) → focus on Signal.
        if world.total_fans > 50 && world.signal_conversion_rate_bps < 500 {
            return Self::SignalConversion;
        }
        // Default: content-first for steady organic growth.
        Self::ContentFirst
    }

    /// Derives the strategy with hysteresis — the current strategy gets
    /// a "home field advantage" so the brain doesn't flip-flop between
    /// strategies every cycle when conditions are borderline.
    ///
    /// The hysteresis works by using relaxed thresholds to stay in the
    /// current strategy and stricter thresholds to switch to a new one.
    /// For example, EventDriven stays active until the event is >21 days
    /// away (relaxed), but switches to EventDriven only when the event
    /// is ≤14 days away (strict).
    #[must_use]
    pub fn from_world_model_with_hysteresis(world: &WorldModel, current: Option<Self>) -> Self {
        let candidate = Self::from_world_model(world);
        // If no current strategy, use the candidate.
        let Some(current) = current else {
            return candidate;
        };
        // If the candidate matches the current strategy, no change needed.
        if candidate == current {
            return current;
        }
        // Hysteresis: check if the current strategy should stay active
        // using relaxed thresholds.
        match current {
            Self::EventDriven => {
                // Stay event-driven until the event is >21 days away
                // (relaxed from the 14-day entry threshold).
                if let Some(days) = world.days_to_next_event {
                    if days <= 21 {
                        return Self::EventDriven;
                    }
                }
            }
            Self::AggressiveDiscovery => {
                // Stay aggressive until either the target is met (Ahead)
                // or the trend is no longer stagnant/decelerating.
                if world.growth_target_progress.status == TargetStatus::Behind
                    && world.fan_growth_trend.is_stagnant()
                {
                    return Self::AggressiveDiscovery;
                }
            }
            Self::SignalConversion => {
                // Stay in Signal conversion until the rate is above 7%
                // (relaxed from the 5% entry threshold).
                if world.total_fans > 50 && world.signal_conversion_rate_bps < 700 {
                    return Self::SignalConversion;
                }
            }
            Self::ContentFirst => {
                // Content-first is the default — it stays unless a
                // stronger signal overrides it (which the candidate
                // already captures).
            }
        }
        candidate
    }

    /// Returns the strategy's recommended template priority order.
    /// The evaluator uses this to prioritize which workers to dispatch
    /// when multiple are eligible.
    #[must_use]
    pub fn template_priority(self) -> &'static [&'static str] {
        match self {
            Self::AggressiveDiscovery => &[
                "reddit-scanner",
                "community-engager",
                "growth-strategist",
                "social-post",
                "signal-inviter",
                "press-pitch",
            ],
            Self::EventDriven => &[
                "press-pitch",
                "social-post",
                "signal-inviter",
                "community-engager",
                "reddit-scanner",
                "growth-strategist",
            ],
            Self::ContentFirst => &[
                "social-post",
                "community-engager",
                "growth-strategist",
                "reddit-scanner",
                "signal-inviter",
                "press-pitch",
            ],
            Self::SignalConversion => &[
                "signal-inviter",
                "social-post",
                "growth-strategist",
                "community-engager",
                "reddit-scanner",
                "press-pitch",
            ],
        }
    }

    /// Returns a human-readable name for the strategy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AggressiveDiscovery => "aggressive_discovery",
            Self::EventDriven => "event_driven",
            Self::ContentFirst => "content_first",
            Self::SignalConversion => "signal_conversion",
        }
    }

    /// Infers the strategy that was active when a template was dispatched,
    /// based on the template's position in each strategy's priority list.
    /// The strategy whose priority list ranks this template highest is the
    /// most likely one that was active. Used for hysteresis when the
    /// previous strategy isn't explicitly persisted.
    #[must_use]
    pub fn infer_from_template(template_id: &str) -> Self {
        let strategies = [
            Self::AggressiveDiscovery,
            Self::EventDriven,
            Self::ContentFirst,
            Self::SignalConversion,
        ];
        let mut best = Self::ContentFirst;
        let mut best_rank = usize::MAX;
        for strategy in strategies {
            let rank = strategy
                .template_priority()
                .iter()
                .position(|t| *t == template_id)
                .unwrap_or(usize::MAX);
            if rank < best_rank {
                best_rank = rank;
                best = strategy;
            }
        }
        best
    }
}

/// A pathway record — the brain's memory of which action sequences
/// produced fan growth. Used to evolve strategies over time.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PathwayRecord {
    /// The strategy that guided this pathway.
    pub strategy: GrowthStrategy,
    /// The sequence of template dispatches in this pathway.
    pub template_sequence: Vec<String>,
    /// Total fans acquired during this pathway's measurement window.
    pub fans_acquired: u32,
    /// Whether this pathway is still active or has been completed/abandoned.
    pub active: bool,
}

/// A recent unconsumed insight from an agent outcome. The brain reads these
/// before dispatching the next worker run and includes them in the dispatch
/// prompt so the worker knows what was already discovered. After the brain
/// factors an insight into its planning, it marks the row as consumed.
#[derive(Clone, Debug, Serialize)]
pub struct RecentInsight {
    /// The `agent_outcomes.id` — used to mark the row consumed after planning.
    pub outcome_id: uuid::Uuid,
    /// Which template produced this insight (derived from the task).
    pub template_id: String,
    /// The outcome kind: `campaign_insight`, `generic_insight`, `release_plan_note`.
    pub kind: String,
    /// A short headline from the insight payload, for inclusion in the prompt.
    pub headline: String,
    /// The detail/body of the insight, for inclusion in the prompt.
    pub detail: String,
    /// The recommended action, if any.
    pub recommended_action: Option<String>,
}

/// A snapshot of one worker template's dispatch state: when it last ran and
/// whether the workspace's current situation warrants a new dispatch. The
/// infra layer computes this from agent_service_tasks history and workspace
/// state; the deterministic evaluator consumes it.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthIntelligenceSnapshot {
    /// The worker template ID, e.g. "reddit-scanner", "press-pitch".
    pub template_id: String,
    /// Hours since the last agent run for this template, or `None` if never run.
    /// This counts **any** run, including ones that produced zero items. Used
    /// for the failed-run retry delay so the brain doesn't retry every cycle.
    pub hours_since_last_run: Option<u32>,
    /// Hours since the last **effective** run — one that produced an outcome
    /// with a non-empty `items` array, or `None` if no effective run exists.
    /// The cooldown is measured from this, so a failed/empty run does not
    /// reset the cooldown. If this is `None`, the brain treats the cooldown
    /// as elapsed (never had a successful run → dispatch immediately).
    pub hours_since_last_effective_run: Option<u32>,
    /// Whether there is an upcoming event within the press-pitch lead window.
    pub has_upcoming_event: bool,
    /// Days until the nearest upcoming event, or `None`.
    pub days_to_next_event: Option<u32>,
    /// Whether fan growth has been stagnant for the configured period.
    pub fan_growth_stagnant: bool,
    /// Number of unengaged outreach targets (accepted but not yet engaged).
    pub unengaged_outreach_targets: u32,
    /// The actual unengaged outreach targets (id, display name, subreddit)
    /// for the community-engager prompt. Only populated for the
    /// `community-engager` template snapshot. The brain feeds these into
    /// the dispatch prompt so the LLM can produce concrete social_post
    /// outcomes with `target_id` and `subreddit` fields.
    pub unengaged_targets: Vec<UnengagedTarget>,
    /// Unconsumed insights from recent worker runs, keyed by template_id.
    /// The brain feeds these into the next dispatch prompt and marks them
    /// consumed after planning. This closes the feedback loop: workers
    /// produce insights → brain reads them → brain feeds them forward →
    /// brain marks them consumed → retention deletes after 7 days.
    pub recent_insights: Vec<RecentInsight>,
    /// Latest community engagement performance per subreddit, from
    /// `community_post_metrics`. Only populated for the `community-engager`
    /// template snapshot. The brain uses this to avoid dispatching to
    /// subreddits with consistently poor engagement and to include
    /// performance context in the worker prompt.
    pub community_engagement_history: Vec<CommunityEngagementSummary>,
    /// The measured standing of this worker template from past dispatch
    /// outcomes. The brain uses this to adjust the dispatch cadence (effective
    /// workers get shorter cooldowns, ineffective ones get longer ones) and
    /// to retire workers that consistently produce no fan growth.
    pub standing: Standing,
    /// The brain's belief about the world — shared across all template
    /// snapshots in a cycle. Contains fan counts, signal installs, community
    /// reach, outreach pipeline, event state, and growth target progress.
    pub world_model: WorldModel,
}

/// A single unengaged outreach target that the community-engager should
/// draft a post for. Carries the concrete `target_id` and `subreddit` the
/// LLM needs to produce a `social_post` outcome with a valid
/// `community.engage.request` action.
#[derive(Clone, Debug, Serialize)]
pub struct UnengagedTarget {
    /// The `agent_outreach_targets.id` — becomes `target_id` in the
    /// social_post outcome item.
    pub target_id: uuid::Uuid,
    /// Human-readable name, e.g. "r/MetalPoland".
    pub display_name: String,
    /// Clean subreddit name without `r/` prefix, e.g. "MetalPoland".
    pub subreddit: String,
}

/// Aggregated performance of a single subreddit's recent community posts.
/// Derived from the latest `community_post_metrics` row per post, averaged
/// across all posts to that subreddit in the last 30 days.
#[derive(Clone, Debug, Serialize)]
pub struct CommunityEngagementSummary {
    /// The subreddit name (without `r/` prefix).
    pub subreddit: String,
    /// Number of posts to this subreddit in the window.
    pub post_count: u32,
    /// Average score across posts (Reddit's hotness ranking score).
    pub avg_score: f64,
    /// Average upvotes across posts.
    pub avg_upvotes: f64,
    /// Average comment count across posts.
    pub avg_comments: f64,
    /// Average upvote ratio across posts (0.0–1.0), if available.
    pub avg_upvote_ratio: Option<f64>,
}

/// Cooldown intervals (in hours) for each worker template. The brain will
/// not dispatch the same worker template more often than its cooldown.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct GrowthIntelligencePolicy {
    /// Hours between reddit-scanner dispatches. Default: 7 days.
    pub reddit_scanner_cooldown_hours: u32,
    /// Hours between community-engager dispatches. Default: 5 days.
    pub community_engager_cooldown_hours: u32,
    /// Hours between press-pitch dispatches. Default: 3 days.
    pub press_pitch_cooldown_hours: u32,
    /// Hours between social-post dispatches. Default: 2 days.
    pub social_post_cooldown_hours: u32,
    /// Hours between signal-inviter dispatches. Default: 7 days.
    pub signal_inviter_cooldown_hours: u32,
    /// Hours between growth-strategist (intelligence analyst) dispatches. Default: 1 day.
    pub growth_strategist_cooldown_hours: u32,
    /// Days before an event to start press outreach. Default: 30 days.
    pub press_pitch_event_lead_days: u32,
    /// Days of stagnant fan growth before dispatching community engagement. Default: 14 days.
    pub fan_growth_stagnant_days: u32,
    /// Minimum hours to wait before retrying a worker after a failed/empty
    /// run. Prevents retry storms on the 5-minute autopilot cycle when a
    /// worker keeps producing zero items. The hard cap (`max_actions_24h`
    /// in the autopilot policy table) is the ultimate backstop.
    /// Default: 1 hour.
    pub failed_run_retry_hours: u32,
}

impl Default for GrowthIntelligencePolicy {
    fn default() -> Self {
        Self {
            reddit_scanner_cooldown_hours: 168,    // 7 days
            community_engager_cooldown_hours: 120, // 5 days
            press_pitch_cooldown_hours: 72,        // 3 days
            social_post_cooldown_hours: 48,        // 2 days
            signal_inviter_cooldown_hours: 168,    // 7 days
            growth_strategist_cooldown_hours: 24,  // 1 day
            press_pitch_event_lead_days: 30,
            fan_growth_stagnant_days: 14,
            failed_run_retry_hours: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::{OutcomeRecord, RetirementReason, assess_standing};

    fn policy() -> StandingPolicy {
        StandingPolicy::agent_defaults()
    }

    #[test]
    fn untested_worker_runs_at_base_cadence() {
        let record = OutcomeRecord::default();
        let standing = assess_standing(record, policy());
        assert!(matches!(standing, Standing::Untested { measured: 0 }));
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn one_measurement_does_not_adjust_cadence() {
        // Below minimum_measured_record (2), the worker stays untested.
        let record = OutcomeRecord {
            improved: 1,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(standing, Standing::Untested { measured: 1 }));
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn effective_worker_gets_shorter_cooldown() {
        // 2 improved out of 2 measured → effectiveness = 10_000 bps → base cooldown.
        let record = OutcomeRecord {
            improved: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(standing, Standing::Weighted { .. }));
        // At max effectiveness, cooldown equals base.
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn ineffective_worker_gets_longer_cooldown() {
        // 0 improved, 2 neutral out of 2 → effectiveness = 5_000 bps → 2x base.
        let record = OutcomeRecord {
            neutral: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        if let Standing::Weighted { basis_points, .. } = standing {
            assert_eq!(basis_points, 5_000);
        } else {
            panic!("expected Weighted standing");
        }
        // 10_000 / 5_000 = 2 → 2x base cooldown.
        assert_eq!(effective_agent_cooldown(168, standing), 336);
    }

    #[test]
    fn cooldown_adjustment_is_capped_at_4x() {
        // Floor effectiveness is 2_000 bps → 10_000 / 2_000 = 5, but capped at 4x.
        let record = OutcomeRecord {
            worsened: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        if let Standing::Weighted { basis_points, .. } = standing {
            assert_eq!(basis_points, 2_000); // floor
        } else {
            panic!("expected Weighted standing");
        }
        // Capped at 4x base.
        assert_eq!(effective_agent_cooldown(168, standing), 168 * 4);
    }

    #[test]
    fn retired_worker_never_dispatches() {
        let record = OutcomeRecord {
            worsened: 3,
            consecutive_worsened: 3,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(
            standing,
            Standing::Retired {
                reason: RetirementReason::RepeatedlyWorsened
            }
        ));
        assert_eq!(effective_agent_cooldown(168, standing), u32::MAX);
    }

    #[test]
    fn operator_retired_worker_is_retired() {
        let record = OutcomeRecord {
            operator_retired: true,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(
            standing,
            Standing::Retired {
                reason: RetirementReason::OperatorRetired
            }
        ));
    }

    #[test]
    fn one_worsened_does_not_retire() {
        // A single bad result is noise, not a pattern.
        let record = OutcomeRecord {
            worsened: 1,
            consecutive_worsened: 1,
            improved: 1,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        // 2 measured, not retired.
        assert!(!standing.is_retired());
    }

    #[test]
    fn effective_worker_escalates_to_premium() {
        let record = OutcomeRecord {
            improved: 3,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert_eq!(
            effective_agent_tier(AgentTier::Basic, standing),
            AgentTier::Premium
        );
    }

    #[test]
    fn mediocre_worker_stays_at_base_tier() {
        let record = OutcomeRecord {
            neutral: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert_eq!(
            effective_agent_tier(AgentTier::Basic, standing),
            AgentTier::Basic
        );
    }

    // ── World Model + Growth Target tests ──

    #[test]
    fn growth_target_for_small_fanbase_is_aggressive() {
        let target = GrowthTarget::from_fan_count(50);
        assert_eq!(target.new_fans_per_month, 20);
        assert_eq!(target.signal_installs_per_month, 5);
    }

    #[test]
    fn growth_target_for_medium_fanbase_is_moderate() {
        let target = GrowthTarget::from_fan_count(500);
        assert_eq!(target.new_fans_per_month, 50);
        assert_eq!(target.signal_installs_per_month, 50);
    }

    #[test]
    fn growth_target_for_large_fanbase_is_steady() {
        let target = GrowthTarget::from_fan_count(5000);
        assert_eq!(target.new_fans_per_month, 100);
        assert_eq!(target.signal_installs_per_month, 500);
    }

    #[test]
    fn target_progress_behind_when_far_from_target() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 5, 1);
        assert_eq!(progress.progress_bps, 2_500); // 5/20 = 25%
        assert_eq!(progress.status, TargetStatus::Behind);
    }

    #[test]
    fn target_progress_on_track_when_halfway() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 12, 3);
        assert_eq!(progress.progress_bps, 6_000); // 12/20 = 60%
        assert_eq!(progress.status, TargetStatus::OnTrack);
    }

    #[test]
    fn target_progress_ahead_when_near_or_above_target() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 18, 4);
        assert_eq!(progress.progress_bps, 9_000); // 18/20 = 90%
        assert_eq!(progress.status, TargetStatus::Ahead);
    }

    #[test]
    fn target_progress_caps_at_10k_when_exceeding_target() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 50, 10);
        assert_eq!(progress.progress_bps, 10_000); // capped
        assert_eq!(progress.status, TargetStatus::Ahead);
    }

    #[test]
    fn stagnant_trend_is_urgent() {
        assert!(GrowthTrend::Stagnant.is_stagnant());
        assert!(GrowthTrend::Decelerating.is_stagnant());
        assert!(!GrowthTrend::Steady.is_stagnant());
        assert!(!GrowthTrend::Accelerating.is_stagnant());
    }

    #[test]
    fn zero_fan_target_does_not_divide_by_zero() {
        let target = GrowthTarget {
            new_fans_per_month: 0,
            signal_installs_per_month: 0,
        };
        let progress = GrowthTargetProgress::from_counts(target, 5, 1);
        assert_eq!(progress.progress_bps, 10_000); // target met (no target)
        assert_eq!(progress.status, TargetStatus::Ahead);
    }

    // ── Causal Model + Prediction Error tests ──

    #[test]
    fn causal_model_uses_default_prior_for_unknown_template() {
        let model = CausalModel::default();
        let ctx = DispatchContext::default();
        assert_eq!(
            model.predict("unknown-template", &ctx),
            DEFAULT_EXPECTED_FANS
        );
        assert_eq!(model.confidence("unknown-template"), 0);
    }

    #[test]
    fn causal_model_event_proximity_boosts_prediction() {
        let model = CausalModel::default();
        let ctx_close = DispatchContext {
            days_to_event: Some(5),
            ..Default::default()
        };
        let ctx_far = DispatchContext {
            days_to_event: Some(60),
            ..Default::default()
        };
        // Close event: 2.0 * 1.5 = 3.0
        assert!((model.predict("t", &ctx_close) - 3.0).abs() < 0.01);
        // Far event: no boost
        assert!((model.predict("t", &ctx_far) - 2.0).abs() < 0.01);
    }

    #[test]
    fn causal_model_stagnant_trend_reduces_prediction() {
        let model = CausalModel::default();
        let ctx = DispatchContext {
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        // Stagnant: 2.0 * 0.8 = 1.6
        assert!((model.predict("t", &ctx) - 1.6).abs() < 0.01);
    }

    #[test]
    fn causal_model_updates_from_prediction_error() {
        let mut model = CausalModel::default();
        let prediction = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 2.0,
            ..Default::default()
        };
        // Observed 5 fans — better than expected.
        let outcome = PredictionOutcome::from_observation(prediction, 5.0, 0.0);
        model.update(&outcome);
        // After one update: lr = 1/(1+0) = 1.0, so updated = 2 + 1*(5-2) = 5.0
        assert!((model.expected_fans("t") - 5.0).abs() < 0.01);
        assert_eq!(model.confidence("t"), 1);
    }

    #[test]
    fn causal_model_learning_rate_decays_with_confidence() {
        let mut model = CausalModel::default();
        // First update: lr = 1.0, jumps to observed value.
        let p1 = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 2.0,
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p1, 10.0, 0.0));
        assert!((model.expected_fans("t") - 10.0).abs() < 0.01);
        // After many updates with observed=0, the model moves toward 0
        // but the learning rate decays, so it never reaches 0 exactly.
        for _ in 0..20 {
            let p = DispatchPrediction {
                template_id: "t".to_owned(),
                expected_new_fans: 10.0,
                ..Default::default()
            };
            model.update(&PredictionOutcome::from_observation(p, 0.0, 0.0));
        }
        let final_val = model.expected_fans("t");
        assert!(final_val < 10.0, "model should have moved toward 0");
        assert!(final_val > 0.0, "but never reaches exactly 0");
        // After 20+ updates, lr is capped at 1/(1+10) ≈ 0.091 — small.
        // The model should be converging slowly.
        assert_eq!(model.confidence("t"), 21);
    }

    #[test]
    fn prediction_error_computes_dopamine_signal() {
        let prediction = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 3.0,
            expected_signal_installs: 1.0,
            ..Default::default()
        };
        let outcome = PredictionOutcome::from_observation(prediction, 7.0, 0.5);
        // Positive fan error: better than expected.
        assert!((outcome.fan_prediction_error - 4.0).abs() < 0.01);
        // Negative signal error: worse than expected.
        assert!((outcome.signal_prediction_error - (-0.5)).abs() < 0.01);
    }

    // ── Opportunity Queue + EFE scoring tests ──

    #[test]
    fn efe_score_balances_fans_and_information() {
        // Higher expected fans → lower (better) EFE.
        let high_fans = GrowthOpportunity::compute_efe(10.0, 0.5);
        let low_fans = GrowthOpportunity::compute_efe(2.0, 0.5);
        assert!(high_fans < low_fans);
        // Higher information gain → lower (better) EFE.
        let high_info = GrowthOpportunity::compute_efe(5.0, 1.0);
        let low_info = GrowthOpportunity::compute_efe(5.0, 0.1);
        assert!(high_info < low_info);
    }

    #[test]
    fn information_gain_decays_with_confidence() {
        // Zero confidence → maximum information gain.
        assert!((information_gain(0) - 1.0).abs() < 0.01);
        // Some confidence → less to learn.
        assert!((information_gain(10) - 0.09).abs() < 0.01);
        // High confidence → minimal new information.
        assert!(information_gain(50) < 0.03);
    }

    #[test]
    fn opportunity_new_computes_efe_automatically() {
        let opp = GrowthOpportunity::new(
            "reddit-scanner".to_owned(),
            "Scan for new communities".to_owned(),
            5.0,
            0.8,
            DispatchContext::default(),
            "Stagnant growth".to_owned(),
        );
        // EFE = -(5.0 + 0.8) = -5.8
        assert!((opp.efe_score - (-5.8)).abs() < 0.01);
    }

    // ── Exploration Memory tests ──

    #[test]
    fn exploration_memory_novel_for_unvisited() {
        let mem = ExplorationMemory::default();
        assert!((mem.novelty("reddit-scanner", "ctx1") - 1.0).abs() < 0.01);
    }

    #[test]
    fn exploration_memory_novelty_decreases_with_visits() {
        let mut mem = ExplorationMemory::default();
        mem.record_visit("reddit-scanner", "ctx1");
        assert!((mem.novelty("reddit-scanner", "ctx1") - 0.5).abs() < 0.01);
        mem.record_visit("reddit-scanner", "ctx1");
        assert!((mem.novelty("reddit-scanner", "ctx1") - (1.0 / 3.0)).abs() < 0.01);
    }

    #[test]
    fn exploration_memory_tracks_unique_pairs() {
        let mut mem = ExplorationMemory::default();
        mem.record_visit("a", "x");
        mem.record_visit("a", "x"); // same pair
        mem.record_visit("b", "y"); // different pair
        assert_eq!(mem.explored_count(), 2);
    }

    #[test]
    fn context_hash_distinguishes_different_contexts() {
        let ctx1 = DispatchContext {
            days_to_event: Some(5),
            ..Default::default()
        };
        let ctx2 = DispatchContext {
            days_to_event: Some(30),
            ..Default::default()
        };
        assert_ne!(context_hash(&ctx1), context_hash(&ctx2));
    }

    #[test]
    fn context_hash_same_for_same_context() {
        let ctx = DispatchContext {
            days_to_event: Some(5),
            fan_growth_trend: GrowthTrend::Stagnant,
            subreddit_type: Some("metal".to_owned()),
            post_format: Some("text".to_owned()),
            ..Default::default()
        };
        assert_eq!(context_hash(&ctx), context_hash(&ctx));
    }

    // ── Hierarchical Planning tests ──

    #[test]
    fn strategy_aggressive_discovery_when_behind_and_stagnant() {
        let world = WorldModel {
            fan_growth_trend: GrowthTrend::Stagnant,
            growth_target_progress: GrowthTargetProgress {
                status: TargetStatus::Behind,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::AggressiveDiscovery
        );
    }

    #[test]
    fn strategy_event_driven_when_event_close() {
        let world = WorldModel {
            days_to_next_event: Some(10),
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::EventDriven
        );
    }

    #[test]
    fn strategy_signal_conversion_when_adoption_low() {
        let world = WorldModel {
            total_fans: 100,
            signal_conversion_rate_bps: 200, // 2%
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::SignalConversion
        );
    }

    #[test]
    fn strategy_content_first_as_default() {
        let world = WorldModel {
            total_fans: 100,
            signal_conversion_rate_bps: 800, // 8% — above threshold
            ..Default::default()
        };
        assert_eq!(
            GrowthStrategy::from_world_model(&world),
            GrowthStrategy::ContentFirst
        );
    }

    #[test]
    fn strategy_template_priority_orders_templates() {
        let aggressive = GrowthStrategy::AggressiveDiscovery.template_priority();
        assert_eq!(aggressive[0], "reddit-scanner");
        let event = GrowthStrategy::EventDriven.template_priority();
        assert_eq!(event[0], "press-pitch");
        let content = GrowthStrategy::ContentFirst.template_priority();
        assert_eq!(content[0], "social-post");
        let signal = GrowthStrategy::SignalConversion.template_priority();
        assert_eq!(signal[0], "signal-inviter");
    }

    #[test]
    fn pathway_record_defaults_to_inactive() {
        let record = PathwayRecord::default();
        assert!(!record.active);
        assert!(record.template_sequence.is_empty());
        assert_eq!(record.fans_acquired, 0);
    }
}
