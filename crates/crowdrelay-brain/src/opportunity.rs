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

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::causal_model::DispatchContext;
use crate::exploration::context_hash;

/// The kind of action an opportunity represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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

    /// Infers the action kind from a worker template ID.
    /// Falls back to `Scan` for unknown templates (the safest
    /// read-only action).
    #[must_use]
    pub fn from_template(template_id: &str) -> Self {
        if template_id.starts_with("reddit-scanner") {
            Self::Scan
        } else if template_id.starts_with("community-engager") {
            Self::Post
        } else if template_id.starts_with("signal-inviter") {
            Self::Invite
        } else if template_id.starts_with("press-pitch") || template_id.starts_with("outreach") {
            Self::Pitch
        } else {
            Self::Scan
        }
    }
}

/// A stable identity for a growth opportunity.
///
/// Two opportunities with the same identity are the same opportunity —
/// the brain should not dispatch both. The identity is deterministic:
/// given the same template, target, action, and context, the identity
/// is always the same.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opportunity_id_is_stable() {
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new("t", "target", OpportunityAction::Post, &ctx);
        let id2 = OpportunityId::new("t", "target", OpportunityAction::Post, &ctx);
        assert_eq!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_targets() {
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new("t", "a", OpportunityAction::Post, &ctx);
        let id2 = OpportunityId::new("t", "b", OpportunityAction::Post, &ctx);
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_distinguishes_different_actions() {
        let ctx = DispatchContext::default();
        let id1 = OpportunityId::new("t", "target", OpportunityAction::Scan, &ctx);
        let id2 = OpportunityId::new("t", "target", OpportunityAction::Post, &ctx);
        assert_ne!(id1, id2);
    }

    #[test]
    fn opportunity_id_display_format() {
        let ctx = DispatchContext::default();
        let id = OpportunityId::new("t", "target", OpportunityAction::Post, &ctx);
        let s = id.to_string();
        assert!(s.starts_with("t:target:post:"));
    }

    #[test]
    fn from_template_infers_correct_action() {
        assert_eq!(
            OpportunityAction::from_template("reddit-scanner"),
            OpportunityAction::Scan
        );
        assert_eq!(
            OpportunityAction::from_template("community-engager"),
            OpportunityAction::Post
        );
        assert_eq!(
            OpportunityAction::from_template("signal-inviter"),
            OpportunityAction::Invite
        );
        assert_eq!(
            OpportunityAction::from_template("press-pitch"),
            OpportunityAction::Pitch
        );
        assert_eq!(
            OpportunityAction::from_template("unknown"),
            OpportunityAction::Scan
        );
    }

    #[test]
    fn with_context_hash_creates_id_without_context() {
        let id = OpportunityId::with_context_hash(
            "t",
            "target",
            OpportunityAction::Scan,
            "1:Std:metal:text_report",
        );
        assert_eq!(id.context_hash, "1:Std:metal:text_report");
    }
}
