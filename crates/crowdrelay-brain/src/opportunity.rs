//! Opportunity identity — stable, hashable IDs for growth opportunities.
//!
//! The brain needs to track opportunities across cycles: it sees the same
//! "post to r/MetalMusic about the upcoming show" opportunity every cycle
//! until it's dispatched or expires. Without stable identities, the brain
//! can't deduplicate, track lifecycle, or build an opportunity graph.
//!
//! # Identity structure
//!
//! An opportunity identity is composed of:
//! - **Template**: which worker would address it (reddit-scanner, etc.)
//! - **Target**: the specific community/person/venue being targeted
//! - **Action kind**: what kind of action (scan, post, pitch, invite)
//! - **Context hash**: the dispatch context hash (event proximity, trend, etc.)
//!
//! This produces a stable string ID like:
//! `community-engager:r_MetalMusic:post:event_2:stagnant`

use serde::Serialize;
use std::fmt;

use crate::causal_model::DispatchContext;
use crate::exploration::context_hash;

/// The kind of action an opportunity represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityAction {
    /// Scan a community for intelligence (reddit-scanner).
    Scan,
    /// Post content to a community (community-engager, social-post).
    Post,
    /// Pitch to press/media (press-pitch).
    Pitch,
    /// Invite fans to install Signal (signal-inviter).
    Invite,
    /// Analyze growth strategy (growth-strategist).
    Analyze,
}

impl OpportunityAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Post => "post",
            Self::Pitch => "pitch",
            Self::Invite => "invite",
            Self::Analyze => "analyze",
        }
    }
}

/// A stable identity for a growth opportunity.
///
/// Two opportunities with the same identity are the same opportunity —
/// the brain should not dispatch both. The identity is deterministic:
/// given the same template, target, action, and context, the identity
/// is always the same.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub struct OpportunityId {
    /// The worker template that would address this opportunity.
    pub template_id: String,
    /// The specific target (subreddit name, venue name, etc.).
    pub target: String,
    /// The kind of action.
    pub action: OpportunityAction,
    /// The context hash (event proximity, trend, subreddit type, etc.).
    pub context_hash: String,
}

impl OpportunityId {
    /// Creates a new opportunity identity from its components.
    #[must_use]
    pub fn new(
        template_id: &str,
        target: &str,
        action: OpportunityAction,
        context: &DispatchContext,
    ) -> Self {
        Self {
            template_id: template_id.to_owned(),
            target: target.to_owned(),
            action,
            context_hash: context_hash(context),
        }
    }

    /// Creates an opportunity identity from a pre-computed context hash.
    #[must_use]
    pub fn with_context_hash(
        template_id: &str,
        target: &str,
        action: OpportunityAction,
        ctx_hash: &str,
    ) -> Self {
        Self {
            template_id: template_id.to_owned(),
            target: target.to_owned(),
            action,
            context_hash: ctx_hash.to_owned(),
        }
    }
}

impl fmt::Display for OpportunityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}",
            self.template_id,
            self.target,
            self.action.as_str(),
            self.context_hash
        )
    }
}

/// The lifecycle state of an opportunity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityState {
    /// The opportunity has been identified but not yet dispatched.
    #[default]
    Open,
    /// The brain has dispatched a worker to address this opportunity.
    Dispatched,
    /// The dispatch completed and measurement is in progress.
    Measuring,
    /// The measurement is complete and the outcome has been recorded.
    Resolved,
    /// The opportunity expired before the brain dispatched it.
    Expired,
    /// The brain decided not to pursue this opportunity (low EFE, budget).
    Skipped,
}

impl OpportunityState {
    /// Returns true if the opportunity is still actionable.
    #[must_use]
    pub const fn is_actionable(self) -> bool {
        matches!(self, Self::Open)
    }

    /// Returns true if the opportunity has been dispatched or resolved.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Expired | Self::Skipped)
    }
}

/// A tracked opportunity with its current lifecycle state.
#[derive(Clone, Debug, Serialize)]
pub struct TrackedOpportunity {
    pub id: OpportunityId,
    pub state: OpportunityState,
    /// EFE score at the time of identification (lower = better).
    pub efe_score: f64,
    /// When the opportunity was first identified.
    pub identified_at: time::OffsetDateTime,
    /// When the opportunity was dispatched, if applicable.
    pub dispatched_at: Option<time::OffsetDateTime>,
    /// The action_id if dispatched, for linking to measurements.
    pub action_id: Option<uuid::Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world_model::GrowthTrend;

    #[test]
    fn opportunity_id_is_deterministic() {
        let ctx = DispatchContext {
            days_to_event: Some(5),
            fan_growth_trend: GrowthTrend::Stagnant,
            subreddit_type: Some("metal".to_owned()),
            ..Default::default()
        };
        let id1 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        let id2 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        assert_eq!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_targets() {
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        let id2 = OpportunityId::new(
            "community-engager",
            "r_ProgMusic",
            OpportunityAction::Post,
            &ctx,
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_actions() {
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new(
            "reddit-scanner",
            "r_MetalMusic",
            OpportunityAction::Scan,
            &ctx,
        );
        let id2 = OpportunityId::new(
            "reddit-scanner",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_contexts() {
        let ctx1 = DispatchContext {
            days_to_event: Some(5),
            ..Default::default()
        };
        let ctx2 = DispatchContext {
            days_to_event: Some(30),
            ..Default::default()
        };
        let id1 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx1,
        );
        let id2 = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx2,
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_display_is_colon_separated() {
        let ctx = DispatchContext::default();
        let id = OpportunityId::new(
            "community-engager",
            "r_MetalMusic",
            OpportunityAction::Post,
            &ctx,
        );
        let display = id.to_string();
        assert!(display.starts_with("community-engager:r_MetalMusic:post:"));
    }

    #[test]
    fn opportunity_id_is_hashable() {
        use std::collections::HashSet;
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new("a", "b", OpportunityAction::Scan, &ctx);
        let id2 = OpportunityId::new("a", "b", OpportunityAction::Scan, &ctx);
        let id3 = OpportunityId::new("a", "c", OpportunityAction::Scan, &ctx);
        let mut set = HashSet::new();
        set.insert(id1);
        set.insert(id2);
        set.insert(id3);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn opportunity_state_open_is_actionable() {
        assert!(OpportunityState::Open.is_actionable());
        assert!(!OpportunityState::Dispatched.is_actionable());
        assert!(!OpportunityState::Resolved.is_actionable());
    }

    #[test]
    fn opportunity_state_terminal_states() {
        assert!(OpportunityState::Resolved.is_terminal());
        assert!(OpportunityState::Expired.is_terminal());
        assert!(OpportunityState::Skipped.is_terminal());
        assert!(!OpportunityState::Open.is_terminal());
        assert!(!OpportunityState::Dispatched.is_terminal());
        assert!(!OpportunityState::Measuring.is_terminal());
    }

    #[test]
    fn opportunity_action_as_str() {
        assert_eq!(OpportunityAction::Scan.as_str(), "scan");
        assert_eq!(OpportunityAction::Post.as_str(), "post");
        assert_eq!(OpportunityAction::Pitch.as_str(), "pitch");
        assert_eq!(OpportunityAction::Invite.as_str(), "invite");
        assert_eq!(OpportunityAction::Analyze.as_str(), "analyze");
    }

    #[test]
    fn with_context_hash_creates_id_without_context() {
        let id = OpportunityId::with_context_hash(
            "reddit-scanner",
            "r_MetalMusic",
            OpportunityAction::Scan,
            "1:Std:metal:text_report",
        );
        assert_eq!(id.context_hash, "1:Std:metal:text_report");
        assert_eq!(id.template_id, "reddit-scanner");
    }
}
