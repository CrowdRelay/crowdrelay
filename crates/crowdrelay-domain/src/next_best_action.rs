//! One prioritized queue across every Autopilot context.
//!
//! Seventeen contexts each producing their own list is not an operator tool —
//! it is seventeen inboxes. This module answers the only question that matters
//! at the start of a working day: of everything the system currently knows,
//! what should a human do next?
//!
//! The ranking is **lexicographic, not a weighted score**. A weighted sum is
//! easy to write and impossible to explain: an operator cannot tell why a
//! suggestion landed where it did, and a small weight change silently reorders
//! everything. Ordered tiers mean every entry can state the one factor that
//! decided its position, which is also what makes the Phase 7 learning
//! adjustment auditable rather than magic.
//!
//! Order, highest first, exactly as the plan fixed it:
//!
//! 1. authority state — someone blocked on a human outranks a note to self
//! 2. deadline proximity
//! 3. value tier of the affected metric
//! 4. measured effect of the same action kind in the past
//! 5. confidence
//! 6. deviation magnitude
//!
//! Integer arithmetic only; magnitudes in basis points.

use serde::{Deserialize, Serialize};

use crate::{autonomy::Confidence, growth_metrics::MetricValueTier};

/// Hard cap on the queue. The point is the top handful an operator can
/// actually work through, not a complete inventory of everything outstanding —
/// a thirty-item list is a backlog, and a backlog gets ignored wholesale.
pub const MAX_QUEUE_ENTRIES: usize = 10;

/// What the authority ladder currently says about one finding, from the
/// operator's point of view rather than the system's.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityState {
    /// Executing on its own. Ranked last deliberately: nobody is blocked on it,
    /// and surfacing it would spend the top of a human's queue on work that is
    /// already handled.
    AutoExecuting,
    /// Recorded, but the context has no authority to act. Informational.
    Observed,
    /// The system would act and is asking first.
    Recommended,
    /// Blocked on a human. Nothing moves until someone answers.
    AwaitingApproval,
}

impl AuthorityState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoExecuting => "auto_executing",
            Self::Observed => "observed",
            Self::Recommended => "recommended",
            Self::AwaitingApproval => "awaiting_approval",
        }
    }

    /// Maps a persisted disposition. Returns `None` for a denied decision:
    /// denial means the gate refused it, and putting it in a queue of things to
    /// do would invite someone to override a policy from a list view.
    #[must_use]
    pub fn from_disposition(disposition: &str) -> Option<Self> {
        match disposition {
            "require_approval" => Some(Self::AwaitingApproval),
            "recommend_only" => Some(Self::Recommended),
            "observe_only" => Some(Self::Observed),
            "auto_execute" => Some(Self::AutoExecuting),
            _ => None,
        }
    }

    /// What happens if this entry is ignored.
    ///
    /// Every one of these is a statement about the system's own behaviour, not
    /// a prediction about the business. "You will lose ticket sales" would be
    /// an invented consequence; "the approval expires and nothing runs" is what
    /// the code actually does.
    #[must_use]
    pub const fn consequence(self, has_deadline: bool) -> &'static str {
        match (self, has_deadline) {
            (Self::AwaitingApproval, true) => {
                "the approval expires before the deadline and the action is never executed"
            }
            (Self::AwaitingApproval, false) => {
                "the approval expires and the action is never executed"
            }
            (Self::Recommended, true) => "the deadline passes with no action recorded",
            (Self::Recommended, false) => "the finding stays open and no action is recorded",
            (Self::Observed, _) => {
                "nothing runs; the context has no authority to act on this finding"
            }
            (Self::AutoExecuting, _) => "the action proceeds without you",
        }
    }

    const fn tier(self) -> u8 {
        match self {
            Self::AwaitingApproval => 3,
            Self::Recommended => 2,
            Self::Observed => 1,
            Self::AutoExecuting => 0,
        }
    }
}

/// The measured record of one action kind, from
/// `viryaos_autopilot_outcomes.effect_assessment`.
///
/// Always `None` until Phase 5 records growth outcomes. The slot exists now so
/// Phase 7 changes the *data* feeding the comparator rather than the comparator
/// itself — reordering the tiers later would invalidate every explanation an
/// operator had already read.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MeasuredEffect {
    pub improved: u32,
    pub neutral: u32,
    pub worsened: u32,
}

impl MeasuredEffect {
    /// Net measured record, clamped into a small ordered band so a single
    /// lucky outcome cannot outrank a deadline or a value tier.
    const fn tier(self) -> u8 {
        if self.improved > self.worsened {
            2
        } else if self.worsened > self.improved {
            0
        } else {
            1
        }
    }
}

/// Hours-until-deadline bucketed into ordered urgency bands.
///
/// Bucketed rather than compared raw so that two entries 40 and 41 hours out do
/// not swap places every hour the queue is rebuilt.
const fn deadline_tier(hours_until_deadline: Option<i64>) -> u8 {
    match hours_until_deadline {
        // A deadline already past cannot be met. It is not urgent, it is over,
        // and ranking it top would put unrecoverable work above recoverable.
        Some(hours) if hours < 0 => 0,
        Some(hours) if hours <= 24 => 5,
        Some(hours) if hours <= 72 => 4,
        Some(hours) if hours <= 168 => 3,
        Some(hours) if hours <= 336 => 2,
        Some(_) => 1,
        // No deadline at all sits above an expired one and below every live
        // one: there is nothing to miss, but also nothing forcing the timing.
        None => 1,
    }
}

/// One thing the system currently knows, as it enters the ranking.
///
/// Assembled from rows that already exist — a decision, its disposition, its
/// action payload — so nothing here is a new stored fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueueCandidate {
    pub context: &'static str,
    pub decision_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub authority: AuthorityState,
    pub confidence: Confidence,
    pub reason: String,
    pub recommended_action: String,
    /// Hours until the subject's own date. `None` when it has none.
    pub hours_until_deadline: Option<i64>,
    /// Value tier of the metric this finding affects, where one is known.
    pub value_tier: Option<MetricValueTier>,
    /// Measured deviation or overdue ratio in basis points, where the finding
    /// carried one. Never a currency amount: the system does not know what a
    /// stalled channel is worth, and inventing a figure would be the most
    /// convincing lie in the whole queue.
    pub deviation_basis_points: Option<u32>,
    pub measured_effect: Option<MeasuredEffect>,
}

/// Why an entry landed where it did — the single factor that decided its
/// position against the entry directly below it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RankFactor {
    Authority,
    Deadline,
    ValueTier,
    MeasuredEffect,
    Confidence,
    Magnitude,
    /// Every ordered factor tied; position is the deterministic tie-break.
    Tie,
}

impl RankFactor {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authority => "authority",
            Self::Deadline => "deadline",
            Self::ValueTier => "value_tier",
            Self::MeasuredEffect => "measured_effect",
            Self::Confidence => "confidence",
            Self::Magnitude => "magnitude",
            Self::Tie => "tie",
        }
    }
}

/// One ranked entry, carrying the evidence that justifies its position.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RankedAction {
    pub candidate: QueueCandidate,
    /// 1-based position in the queue.
    pub position: u8,
    /// What decided this entry's position against the next one down. The last
    /// entry reports what separated it from the one above.
    pub ranked_by: RankFactor,
    /// What happens if this entry is ignored.
    pub consequence: &'static str,
}

/// The ordered comparison key. Higher sorts first on every component.
const fn rank_key(candidate: &QueueCandidate) -> [u32; 6] {
    [
        candidate.authority.tier() as u32,
        deadline_tier(candidate.hours_until_deadline) as u32,
        match candidate.value_tier {
            Some(tier) => tier.weight() as u32,
            // An unknown tier must not beat a known-downstream one, and must
            // not be pushed below known-vanity either: absent evidence is not
            // evidence of low value.
            None => MetricValueTier::Intermediate.weight() as u32,
        },
        match candidate.measured_effect {
            Some(effect) => effect.tier() as u32,
            None => 1,
        },
        candidate.confidence.basis_points() as u32,
        match candidate.deviation_basis_points {
            Some(value) => value,
            None => 0,
        },
    ]
}

/// The first component where two keys differ, as an operator-readable factor.
fn separating_factor(left: &[u32; 6], right: &[u32; 6]) -> RankFactor {
    const FACTORS: [RankFactor; 6] = [
        RankFactor::Authority,
        RankFactor::Deadline,
        RankFactor::ValueTier,
        RankFactor::MeasuredEffect,
        RankFactor::Confidence,
        RankFactor::Magnitude,
    ];
    left.iter()
        .zip(right.iter())
        .zip(FACTORS.iter())
        .find_map(|((left, right), factor)| (left != right).then_some(*factor))
        .unwrap_or(RankFactor::Tie)
}

/// Ranks every candidate and returns at most [`MAX_QUEUE_ENTRIES`].
///
/// Ties break on `(subject_id, decision_kind)` so the same evidence always
/// produces the same queue. A queue that reshuffles between two reads of the
/// same data is one an operator stops trusting.
#[must_use]
pub fn rank_next_best_actions(mut candidates: Vec<QueueCandidate>) -> Vec<RankedAction> {
    candidates.sort_by(|left, right| {
        rank_key(right)
            .cmp(&rank_key(left))
            .then_with(|| left.subject_id.cmp(&right.subject_id))
            .then_with(|| left.decision_kind.cmp(&right.decision_kind))
    });
    candidates.truncate(MAX_QUEUE_ENTRIES);

    let keys: Vec<[u32; 6]> = candidates.iter().map(rank_key).collect();
    candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            // Compared against the entry below, or against the one above for
            // the last entry: either way the answer is what separates this
            // position from its neighbour.
            let ranked_by = keys
                .get(index)
                .zip(keys.get(index + 1))
                .or_else(|| {
                    index
                        .checked_sub(1)
                        .and_then(|above| keys.get(above).zip(keys.get(index)))
                })
                .map_or(RankFactor::Tie, |(left, right)| {
                    separating_factor(left, right)
                });
            let consequence = candidate
                .authority
                .consequence(candidate.hours_until_deadline.is_some());
            RankedAction {
                candidate,
                position: u8::try_from(index + 1).unwrap_or(u8::MAX),
                ranked_by,
                consequence,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn candidate(authority: AuthorityState, id: u128) -> QueueCandidate {
        QueueCandidate {
            context: "growth_debt",
            decision_kind: "raise_growth_debt_relationship_quiet".to_owned(),
            subject_kind: "booking_target".to_owned(),
            subject_id: Uuid::from_u128(id),
            authority,
            confidence: Confidence::saturating_from_basis_points(6_000),
            reason: "a warm relationship has gone quiet".to_owned(),
            recommended_action: "revive_quiet_relationship".to_owned(),
            hours_until_deadline: None,
            value_tier: None,
            deviation_basis_points: None,
            measured_effect: None,
        }
    }

    #[test]
    fn work_blocked_on_a_human_outranks_work_that_needs_nobody() {
        let queue = rank_next_best_actions(vec![
            candidate(AuthorityState::AutoExecuting, 1),
            candidate(AuthorityState::Observed, 2),
            candidate(AuthorityState::AwaitingApproval, 3),
            candidate(AuthorityState::Recommended, 4),
        ]);
        let order: Vec<_> = queue
            .iter()
            .map(|entry| entry.candidate.authority)
            .collect();
        assert_eq!(
            order,
            vec![
                AuthorityState::AwaitingApproval,
                AuthorityState::Recommended,
                AuthorityState::Observed,
                AuthorityState::AutoExecuting,
            ]
        );
        assert_eq!(queue[0].position, 1);
        assert_eq!(queue[0].ranked_by, RankFactor::Authority);
    }

    #[test]
    fn a_closer_deadline_outranks_a_distant_one_at_equal_authority() {
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                hours_until_deadline: Some(400),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                hours_until_deadline: Some(12),
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(queue[0].candidate.hours_until_deadline, Some(12));
        assert_eq!(queue[0].ranked_by, RankFactor::Deadline);
    }

    #[test]
    fn an_expired_deadline_does_not_win_the_queue() {
        // Unrecoverable work above recoverable work is the worst possible
        // ordering: it spends the top slot on something nobody can act on.
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                hours_until_deadline: Some(-5),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                hours_until_deadline: Some(300),
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(queue[0].candidate.hours_until_deadline, Some(300));
    }

    #[test]
    fn authority_outranks_a_closer_deadline() {
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                hours_until_deadline: Some(6),
                ..candidate(AuthorityState::Observed, 1)
            },
            QueueCandidate {
                hours_until_deadline: Some(300),
                ..candidate(AuthorityState::AwaitingApproval, 2)
            },
        ]);
        assert_eq!(
            queue[0].candidate.authority,
            AuthorityState::AwaitingApproval
        );
    }

    #[test]
    fn a_downstream_metric_outranks_a_vanity_one() {
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                value_tier: Some(MetricValueTier::Vanity),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                value_tier: Some(MetricValueTier::Downstream),
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(
            queue[0].candidate.value_tier,
            Some(MetricValueTier::Downstream)
        );
        assert_eq!(queue[0].ranked_by, RankFactor::ValueTier);
    }

    #[test]
    fn an_unknown_value_tier_is_not_treated_as_a_low_one() {
        // Absent evidence is not evidence of low value.
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                value_tier: Some(MetricValueTier::Vanity),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                value_tier: None,
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(queue[0].candidate.value_tier, None);
    }

    #[test]
    fn a_measured_worse_record_loses_to_a_measured_better_one() {
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                measured_effect: Some(MeasuredEffect {
                    improved: 0,
                    neutral: 1,
                    worsened: 4,
                }),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                measured_effect: Some(MeasuredEffect {
                    improved: 4,
                    neutral: 1,
                    worsened: 0,
                }),
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(queue[0].candidate.subject_id, Uuid::from_u128(2));
        assert_eq!(queue[0].ranked_by, RankFactor::MeasuredEffect);
    }

    #[test]
    fn a_measured_record_never_outranks_a_deadline_or_a_value_tier() {
        // Ordering is lexicographic: a good past record cannot buy its way past
        // a closer deadline, which is what a weighted score would allow.
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                hours_until_deadline: Some(400),
                measured_effect: Some(MeasuredEffect {
                    improved: 9,
                    neutral: 0,
                    worsened: 0,
                }),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                hours_until_deadline: Some(12),
                measured_effect: Some(MeasuredEffect {
                    improved: 0,
                    neutral: 0,
                    worsened: 9,
                }),
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(queue[0].candidate.hours_until_deadline, Some(12));
    }

    #[test]
    fn confidence_then_magnitude_break_the_remaining_ties() {
        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                confidence: Confidence::saturating_from_basis_points(5_000),
                deviation_basis_points: Some(90_000),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                confidence: Confidence::saturating_from_basis_points(9_000),
                deviation_basis_points: Some(1),
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(queue[0].candidate.confidence.basis_points(), 9_000);
        assert_eq!(queue[0].ranked_by, RankFactor::Confidence);

        let queue = rank_next_best_actions(vec![
            QueueCandidate {
                deviation_basis_points: Some(1_000),
                ..candidate(AuthorityState::Recommended, 1)
            },
            QueueCandidate {
                deviation_basis_points: Some(80_000),
                ..candidate(AuthorityState::Recommended, 2)
            },
        ]);
        assert_eq!(queue[0].candidate.deviation_basis_points, Some(80_000));
        assert_eq!(queue[0].ranked_by, RankFactor::Magnitude);
    }

    #[test]
    fn the_queue_is_capped() {
        let candidates = (0..40)
            .map(|index| candidate(AuthorityState::Recommended, index))
            .collect();
        let queue = rank_next_best_actions(candidates);
        assert_eq!(queue.len(), MAX_QUEUE_ENTRIES);
        assert_eq!(queue[MAX_QUEUE_ENTRIES - 1].position, 10);
    }

    #[test]
    fn identical_evidence_always_produces_the_same_queue() {
        let build = || {
            vec![
                candidate(AuthorityState::Recommended, 9),
                candidate(AuthorityState::Recommended, 3),
                candidate(AuthorityState::Recommended, 7),
            ]
        };
        let first = rank_next_best_actions(build());
        let second = rank_next_best_actions(build());
        assert_eq!(first, second);
        assert_eq!(first[0].candidate.subject_id, Uuid::from_u128(3));
        assert_eq!(first[0].ranked_by, RankFactor::Tie);
    }

    #[test]
    fn a_denied_decision_never_enters_the_queue() {
        assert_eq!(AuthorityState::from_disposition("deny"), None);
        assert_eq!(
            AuthorityState::from_disposition("require_approval"),
            Some(AuthorityState::AwaitingApproval)
        );
    }

    #[test]
    fn every_entry_states_what_happens_if_it_is_ignored() {
        let queue = rank_next_best_actions(vec![
            candidate(AuthorityState::AwaitingApproval, 1),
            QueueCandidate {
                hours_until_deadline: Some(48),
                ..candidate(AuthorityState::Recommended, 2)
            },
            candidate(AuthorityState::Observed, 3),
        ]);
        for entry in &queue {
            assert!(!entry.consequence.is_empty());
            // Consequences describe the system, never a business outcome the
            // domain cannot know.
            for invented in ["revenue", "sales", "fans will", "PLN", "EUR"] {
                assert!(!entry.consequence.contains(invented));
            }
        }
        assert!(queue[1].consequence.contains("deadline"));
    }
}
