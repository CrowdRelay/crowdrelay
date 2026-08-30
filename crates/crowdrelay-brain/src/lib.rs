//! The Brain — deterministic growth intelligence core.
//!
//! The brain is the Bayesian reasoning engine that sits above the autopilot's
//! rule-based scheduler. It owns the causal model, the exploration memory,
//! and the portfolio optimizer. The autopilot feeds the brain candidates from
//! all contexts; the brain scores them, selects the optimal portfolio, and
//! learns from outcomes.
//!
//! # Architecture
//!
//! The brain is built on four mathematical foundations:
//!
//! 1. **Causal Model** — P(incremental_fan | template, context) with a
//!    Gamma-Poisson (Negative Binomial) conjugate model for count data.
//!    Replaces the old EMA + pseudo-variance with mathematically honest
//!    uncertainty. The Signal install path still uses EMA by design —
//!    Signal adoption has different noise characteristics than fan counts.
//! 2. **EFE Scoring** — Expected Free Energy balances pragmatic value
//!    (expected fans) against epistemic value (information gain × uncertainty)
//!    and exploration novelty. Lower EFE = better opportunity.
//! 3. **Exploration Memory** — Go-Explore archive with recency-weighted
//!    visits and cross-template generalization. Old explorations decay so
//!    the brain re-explores contexts it hasn't visited recently.
//! 4. **Portfolio Optimizer** — Global candidate pool with submodular greedy
//!    selection. Accounts for audience overlap, fatigue, and marginal value.
//!    DO NOTHING is always a candidate.
//!
//! # North Star
//!
//! The brain optimizes for **incremental unique durable fans** — fans that
//! would NOT have arrived without the brain's actions, deduplicated across
//! channels, and still active after 30 days. This is NOT the same as
//! "total fan count after dispatch".
//!
//! # Brain vs Workers
//!
//! The brain is deterministic Rust machinery. It owns the strategy, decides
//! what intelligence to gather, and manages everything. LLMs are workers
//! that gather intelligence and draft content. The brain never follows an
//! LLM blindly — it aggregates intelligence, applies deterministic rules,
//! and decides.

pub mod bayesian;
pub mod calibration;
pub mod causal_model;
pub mod context_effect;
pub mod credit_ledger;
pub mod decision_value;
pub mod efe;
pub mod evidence;
pub mod experiment;
pub mod exploration;
pub mod opportunity;
pub mod portfolio;
pub mod reach;
pub mod resource_cost;
pub mod snapshot;
pub mod standing;
pub mod strategy;
pub mod strategy_learning;
pub mod tenant_preference;
pub mod world_model;

// Re-export the most commonly used types at the crate root.
pub use bayesian::{
    HierarchicalNegBinPosterior, HierarchicalPosterior, NegBinPosterior, NormalPosterior,
    TreatmentEffectPosterior, normal_cdf, normal_pdf,
};
pub use calibration::{
    CalibrationByRegime, CalibrationReport, CalibrationTracker, PredictionRecord, ReliabilityBucket,
};
pub use causal_model::{
    CausalModel, DEFAULT_EXPECTED_FANS, DEFAULT_EXPECTED_SIGNAL, DispatchContext,
    DispatchPrediction, MIN_TREATMENT_CONFIDENCE, PRIOR_VARIANCE, PredictionOutcome,
    TreatmentAwareStats,
};
pub use context_effect::ContextGLM;
pub use credit_ledger::{
    ActionExposure, AttributionMethod, AttributionResult, CreditAllocator, CreditEntry, FanOutcome,
    ProportionalCreditAllocator,
};
pub use decision_value::{DecisionValue, EstimationRegime};
pub use efe::{
    EfeWeights, GrowthOpportunity, adaptive_temperature, information_gain, softmax_dispatch,
};
pub use evidence::{EvidenceEvent, EvidenceEventType, EvidenceQuality, GrowthEvidence};
pub use experiment::{
    ExperimentAssignment, ExperimentDesign, ExperimentKind, ExperimentStatus, ExperimentUnitKind,
    FanProvenanceEvent, InterferencePolicy, ProvenanceEventKind, TreatmentAssignment,
};
pub use exploration::{CROSS_TEMPLATE_FACTOR, ExplorationMemory, VISIT_DECAY, context_hash};
pub use opportunity::{OpportunityAction, OpportunityId};
pub use portfolio::{
    DecisionMode, EfeSignal, PortfolioCandidate, PortfolioConfig, PortfolioOptimizer,
    PortfolioRejection, PortfolioSelection, RejectionReason, WaitCandidateValue,
};
pub use reach::{ReachChannel, ReachMetrics};
pub use resource_cost::{CostSource, ResourceCost};
pub use snapshot::{
    CommunityEngagementSummary, GrowthIntelligencePolicy, GrowthIntelligenceSnapshot,
    RecentInsight, UnengagedTarget,
};
pub use standing::{
    AgentTier, agent_standing_policy, effective_agent_cooldown, effective_agent_tier,
};
pub use strategy::GrowthStrategy;
pub use strategy_learning::{
    CONFIDENCE_SATURATION_EVALUATIONS, MIN_EVALUATIONS_FOR_RECOMMENDATION,
    StateConditionedStrategyPosterior, StrategyLearner, StrategyOutcome, StrategyPosterior,
};
pub use tenant_preference::{
    PresentationMetadata, TemplatePreference, TenantPreferencePolicy, TenantPreferencePosterior,
};
pub use world_model::{
    EventProximity, GrowthTarget, GrowthTargetProgress, GrowthTrend, TargetStatus, WorldModel,
};

// Re-export shared domain types the brain depends on.
pub use crowdrelay_domain::learning::{Standing, StandingPolicy};

#[cfg(test)]
mod scenario_tests;
