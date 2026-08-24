//! Relationship-aware outreach bounded context.
//!
//! External intelligence may create an opportunity, but only a verified,
//! operator-owned target can ever be selected. The domain has no email address
//! and cannot send anything; it only decides whether an initial touch or bounded
//! follow-up is appropriate.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{
    OutreachOpportunityId, OutreachTargetId, autonomy::Confidence, free_reach::FreeReachPolicy,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutreachTargetKind {
    Playlist,
    Radio,
    Press,
    Creator,
    SupportSlot,
    Endorsement,
    MediaPatronage,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutreachReplyDisposition {
    None,
    /// A reply exists, but no semantic classification was required.
    Received,
    Positive,
    Declined,
    DoNotContact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OutreachSnapshot {
    pub opportunity_id: OutreachOpportunityId,
    pub target_id: OutreachTargetId,
    pub target_kind: OutreachTargetKind,
    pub target_version: i64,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub relevance_basis_points: u16,
    pub evidence_confidence: Confidence,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    /// Latest outbound touch for this specific opportunity.
    pub last_outreach_at: Option<OffsetDateTime>,
    /// Latest outbound touch to this relationship across any opportunity.
    pub target_last_outreach_at: Option<OffsetDateTime>,
    pub followup_count: u16,
    pub last_reply: OutreachReplyDisposition,
    pub in_flight: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
// Stored configs predate the wave knobs, and a policy row that fails to parse
// takes the whole context down rather than one field.
#[serde(default)]
pub struct OutreachPolicy {
    pub minimum_relevance_basis_points: u16,
    pub minimum_relationship_confidence_basis_points: u16,
    pub initial_cooldown_days: u32,
    pub followup_after_days: u32,
    pub declined_cooldown_days: u32,
    pub maximum_followups: u16,
    /// How free-reach pitches are batched for approval. Nested here rather than
    /// given a context of its own because it is the same operator setting: how
    /// this workspace approaches people it does not know, and how much of that
    /// a human is asked to read at once.
    pub waves: FreeReachPolicy,
}

impl Default for OutreachPolicy {
    fn default() -> Self {
        Self {
            minimum_relevance_basis_points: 7_000,
            minimum_relationship_confidence_basis_points: 7_500,
            initial_cooldown_days: 90,
            followup_after_days: 5,
            declined_cooldown_days: 180,
            maximum_followups: 1,
            waves: FreeReachPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutreachPhase {
    Initial,
    FollowUp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachDecision {
    Hold(OutreachHoldReason),
    Request {
        phase: OutreachPhase,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    IneligibleTarget,
    StaleOpportunity,
    LowRelevance,
    InFlight,
    AlreadyReplied,
    Cooldown,
    FollowUpNotDue,
    FollowUpLimit,
}

#[must_use]
pub fn evaluate_outreach(
    snapshot: OutreachSnapshot,
    policy: OutreachPolicy,
    now: OffsetDateTime,
) -> OutreachDecision {
    if !policy_is_valid(policy) {
        return OutreachDecision::Hold(OutreachHoldReason::InvalidPolicy);
    }
    if snapshot.observed_at > now
        || snapshot.expires_at <= snapshot.observed_at
        || snapshot.target_version <= 0
    {
        return OutreachDecision::Hold(OutreachHoldReason::InvalidSnapshot);
    }
    if !snapshot.active || !snapshot.verified || !snapshot.accepts_outreach {
        return OutreachDecision::Hold(OutreachHoldReason::IneligibleTarget);
    }
    if snapshot.expires_at <= now {
        return OutreachDecision::Hold(OutreachHoldReason::StaleOpportunity);
    }
    if snapshot.relevance_basis_points < policy.minimum_relevance_basis_points
        || snapshot.evidence_confidence.basis_points()
            < policy.minimum_relationship_confidence_basis_points
    {
        return OutreachDecision::Hold(OutreachHoldReason::LowRelevance);
    }
    if snapshot.in_flight {
        return OutreachDecision::Hold(OutreachHoldReason::InFlight);
    }
    match snapshot.last_reply {
        OutreachReplyDisposition::Received
        | OutreachReplyDisposition::Positive
        | OutreachReplyDisposition::DoNotContact => {
            return OutreachDecision::Hold(OutreachHoldReason::AlreadyReplied);
        }
        OutreachReplyDisposition::Declined => {
            if snapshot.last_outreach_at.is_none_or(|at| {
                now - at < Duration::days(i64::from(policy.declined_cooldown_days))
            }) {
                return OutreachDecision::Hold(OutreachHoldReason::Cooldown);
            }
        }
        OutreachReplyDisposition::None => {}
    }

    let Some(last_outreach) = snapshot.last_outreach_at else {
        if snapshot.target_last_outreach_at.is_some_and(|at| {
            at > now || now - at < Duration::days(i64::from(policy.initial_cooldown_days))
        }) {
            return OutreachDecision::Hold(OutreachHoldReason::Cooldown);
        }
        return request(OutreachPhase::Initial, snapshot);
    };
    if last_outreach > now {
        return OutreachDecision::Hold(OutreachHoldReason::InvalidSnapshot);
    }
    if snapshot.followup_count >= policy.maximum_followups {
        return OutreachDecision::Hold(OutreachHoldReason::FollowUpLimit);
    }
    if now - last_outreach < Duration::days(i64::from(policy.followup_after_days)) {
        return OutreachDecision::Hold(OutreachHoldReason::FollowUpNotDue);
    }
    request(OutreachPhase::FollowUp, snapshot)
}

fn request(phase: OutreachPhase, snapshot: OutreachSnapshot) -> OutreachDecision {
    let relevance_bonus = snapshot
        .relevance_basis_points
        .saturating_sub(7_000)
        .min(1_000);
    let confidence = snapshot
        .evidence_confidence
        .basis_points()
        .min(9_000)
        .saturating_add(relevance_bonus);
    OutreachDecision::Request {
        phase,
        confidence: Confidence::saturating_from_basis_points(confidence.min(10_000)),
    }
}

fn policy_is_valid(policy: OutreachPolicy) -> bool {
    (1..=10_000).contains(&policy.minimum_relevance_basis_points)
        && (1..=10_000).contains(&policy.minimum_relationship_confidence_basis_points)
        && policy.initial_cooldown_days > policy.followup_after_days
        && policy.declined_cooldown_days >= policy.initial_cooldown_days
        && policy.followup_after_days > 0
        && policy.maximum_followups <= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }
    fn eligible() -> OutreachSnapshot {
        OutreachSnapshot {
            opportunity_id: OutreachOpportunityId::new(),
            target_id: OutreachTargetId::new(),
            target_kind: OutreachTargetKind::Radio,
            target_version: 1,
            active: true,
            verified: true,
            accepts_outreach: true,
            relevance_basis_points: 8_500,
            evidence_confidence: Confidence::saturating_from_basis_points(9_000),
            observed_at: now() - Duration::hours(1),
            expires_at: now() + Duration::days(14),
            last_outreach_at: None,
            target_last_outreach_at: None,
            followup_count: 0,
            last_reply: OutreachReplyDisposition::None,
            in_flight: false,
        }
    }

    #[test]
    fn unverified_target_can_never_be_contacted() {
        let mut data = eligible();
        data.verified = false;
        assert_eq!(
            evaluate_outreach(data, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::IneligibleTarget)
        );
    }
    #[test]
    fn one_bounded_followup_becomes_due() {
        let mut data = eligible();
        data.last_outreach_at = Some(now() - Duration::days(6));
        assert!(matches!(
            evaluate_outreach(data, OutreachPolicy::default(), now()),
            OutreachDecision::Request {
                phase: OutreachPhase::FollowUp,
                ..
            }
        ));
    }
    #[test]
    fn any_received_reply_stops_followups_without_needing_semantic_classification() {
        let mut data = eligible();
        data.last_outreach_at = Some(now() - Duration::days(6));
        data.last_reply = OutreachReplyDisposition::Received;
        assert_eq!(
            evaluate_outreach(data, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::AlreadyReplied),
        );
    }

    #[test]
    fn a_new_opportunity_respects_relationship_level_initial_cooldown() {
        let mut snapshot = eligible();
        snapshot.target_last_outreach_at = Some(now() - Duration::days(10));
        assert_eq!(
            evaluate_outreach(snapshot, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::Cooldown)
        );
    }
}
