//! The Brain — deterministic growth intelligence core.
//!
//! The brain is the Bayesian reasoning engine that sits above the autopilot's
//! rule-based scheduler. It owns the causal model, the exploration memory,
//! the portfolio optimizer, and the experiment engine. The autopilot feeds
//! the brain candidates from all contexts; the brain scores them, selects
//! the optimal portfolio, and learns from outcomes.
//!
//! # Architecture
//!
//! The brain is built on five mathematical foundations:
//!
//! 1. **Causal Model** — P(incremental_fan | template, context) with proper
//!    Bayesian posteriors (Normal-Normal conjugate model). Replaces the old
//!    EMA + pseudo-variance with mathematically honest uncertainty.
//! 2. **EFE Scoring** — Expected Free Energy balances pragmatic value
//!    (expected fans) against epistemic value (information gain × uncertainty)
//!    and exploration novelty. Lower EFE = better opportunity.
//! 3. **Exploration Memory** — Go-Explore archive with recency-weighted
//!    visits and cross-template generalization. Old explorations decay so
//!    the brain re-explores contexts it hasn't visited recently.
//! 4. **Portfolio Optimizer** — Global candidate pool with submodular greedy
//!    selection. Accounts for audience overlap, fatigue, and marginal value.
//!    DO NOTHING is always a candidate.
//! 5. **Experiment Engine** — Treatment/control assignment with propensity
//!    logging for off-policy evaluation. The brain runs controlled
//!    experiments to measure causal uplift, not just correlation.
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

pub mod audience;
pub mod bayesian;
pub mod causal_model;
pub mod change_point;
pub mod efe;
pub mod experiment;
pub mod exploration;
pub mod fan_network;
pub mod hypothesis;
pub mod metacognition;
pub mod north_star;
pub mod opportunity;
pub mod opportunity_graph;
pub mod options;
pub mod portfolio;
pub mod simulation;
pub mod snapshot;
pub mod standing;
pub mod strategy;
pub mod strategy_learning;
pub mod voi;
pub mod world_model;

// Re-export the most commonly used types at the crate root.
pub use audience::{AudienceKey, estimate_overlap, marginal_value};
pub use causal_model::{
    CausalModel, DEFAULT_EXPECTED_FANS, DEFAULT_EXPECTED_SIGNAL, DispatchContext,
    DispatchPrediction, PRIOR_VARIANCE, PredictionOutcome,
};
pub use change_point::{ChangeDirection, ChangePoint, ChangePointDetector};
pub use efe::{
    EfeWeights, GrowthOpportunity, adaptive_temperature, compute_efe, information_gain,
    softmax_dispatch,
};
pub use experiment::{
    DEFAULT_TREATMENT_PROBABILITY, ExperimentEngine, MIN_CONFIDENCE_FOR_EXPERIMENT,
    PropensityRecord, TreatmentAssignment,
};
pub use exploration::{CROSS_TEMPLATE_FACTOR, ExplorationMemory, VISIT_DECAY, context_hash};
pub use fan_network::{FanNetworkModel, NetworkEffect, RecruitmentChannel};
pub use hypothesis::{Hypothesis, HypothesisRegistry, HypothesisStatus};
pub use metacognition::{MetacognitionMonitor, MetacognitiveState};
pub use north_star::NorthStarMetric;
pub use opportunity::{OpportunityAction, OpportunityId, OpportunityState, TrackedOpportunity};
pub use opportunity_graph::{
    DependencyKind, NodeStatus, OpportunityEdge, OpportunityGraph, OpportunityNode,
};
pub use options::{ActionOption, OptionPlanner, OptionStatus, OptionStep, OptionStepStatus};
pub use portfolio::{
    PortfolioCandidate, PortfolioConfig, PortfolioOptimizer, PortfolioRejection,
    PortfolioSelection, RejectionReason,
};
pub use simulation::{MonthlyPrediction, SimulationResult, WorldSimulation};
pub use snapshot::{
    CommunityEngagementSummary, GrowthIntelligencePolicy, GrowthIntelligenceSnapshot,
    RecentInsight, UnengagedTarget,
};
pub use standing::{
    AgentTier, agent_standing_policy, effective_agent_cooldown, effective_agent_tier,
};
pub use strategy::{GrowthStrategy, PathwayRecord};
pub use strategy_learning::{StrategyLearner, StrategyOutcome};
pub use voi::{expected_information_gain, exploration_bonus, option_value, value_of_information};
pub use world_model::{GrowthTarget, GrowthTargetProgress, GrowthTrend, TargetStatus, WorldModel};

// Re-export shared domain types the brain depends on.
pub use crowdrelay_domain::learning::{Standing, StandingPolicy};
