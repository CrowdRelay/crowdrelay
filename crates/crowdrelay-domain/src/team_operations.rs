//! Human handoff routing for ViryaOS.
//!
//! This is intentionally not a second task-management product. Bounded contexts
//! remain authoritative for approvals, show checklists and opportunities; this
//! module only selects a suitable human owner for work the Autopilot cannot or
//! must not complete itself.

use serde::{Deserialize, Serialize};

use crate::WorkspaceMemberId;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamSkill {
    General,
    Operations,
    Booking,
    Approval,
    Technical,
    Visual,
    Video,
    Photography,
    Social,
    EnglishCopy,
    PolishCopy,
    People,
}

impl TeamSkill {
    /// Every declared skill — `content_engine` validates catalogue `skill`
    /// text against this list so a format can never route to a skill nobody
    /// can hold.
    pub const ALL: &'static [Self] = &[
        Self::General,
        Self::Operations,
        Self::Booking,
        Self::Approval,
        Self::Technical,
        Self::Visual,
        Self::Video,
        Self::Photography,
        Self::Social,
        Self::EnglishCopy,
        Self::PolishCopy,
        Self::People,
    ];

    /// The catalogue-side inverse of `as_str` — anything the seed or a
    /// roster row writes must round-trip through this, or a format routes
    /// to a skill nobody can hold.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|skill| skill.as_str() == value)
            .copied()
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Operations => "operations",
            Self::Booking => "booking",
            Self::Approval => "approval",
            Self::Technical => "technical",
            Self::Visual => "visual",
            Self::Video => "video",
            Self::Photography => "photography",
            Self::Social => "social",
            Self::EnglishCopy => "english_copy",
            Self::PolishCopy => "polish_copy",
            Self::People => "people",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamMemberRoutingSnapshot {
    pub member_id: WorkspaceMemberId,
    pub member_key: String,
    pub active: bool,
    pub skills: Vec<TeamSkill>,
    /// Current unresolved assignments; primary fairness signal.
    pub open_assignments: u16,
    /// Assignments created in the recent balancing window.
    pub recent_assignments: u16,
    /// 100 = normal capacity. Lower values allow temporary load reduction.
    pub capacity_basis_points: u16,
    /// How reliably this member finishes work of this kind, unprompted.
    ///
    /// Measured, not declared: the share of their past assignments that were
    /// completed, weighted down for each reminder it took. 10_000 is "always
    /// finishes without being chased"; 0 is "assignments given to this person
    /// go unanswered".
    ///
    /// Routing used to be skill fit and current load only, so a member who
    /// never completed a task kept receiving it forever — the queue looked
    /// balanced while the work sat still.
    ///
    /// What this cannot see is *why*. A task finished promptly and one finished
    /// after three reminders are distinguishable; enjoyment and obligation are
    /// not. Reminder count is the honest proxy: work someone wants to do rarely
    /// needs chasing, whatever the reason they want to do it.
    pub follow_through_basis_points: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeamAssignmentNeed {
    pub primary_skill: TeamSkill,
    pub secondary_skill: Option<TeamSkill>,
    pub allow_generalist: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamAssignmentDecision {
    pub member_id: WorkspaceMemberId,
    pub member_key: String,
    pub route_score: i32,
}

/// Capability first, fairness second. Stable member-key tie breaking keeps
/// retries deterministic while still distributing work as workloads change.
/// What a member with no completed history scores.
///
/// Neutral rather than zero: a new member has not failed to do anything, and
/// starting them at the bottom would mean never giving them the first task that
/// would prove them either way.
pub const NEUTRAL_FOLLOW_THROUGH_BASIS_POINTS: u16 = 5_000;

/// How many open assignments one person may hold before the system stops
/// adding to the pile.
///
/// The load penalty below spreads work across people who *could* do it. It was
/// never a ceiling, and in the shape this product actually ships in there is
/// nobody to spread to: a solo act is its own social, visual and copy skill, so
/// every task routes to one person and the penalty subtracts from a score that
/// wins anyway. The operator's own words for the result were "flooded with
/// tasks I don't really fully understand".
///
/// Seven is a working week's worth of small jobs, and it is chosen to be
/// obviously finite rather than to be precise. What matters is that a ceiling
/// exists: past it, the honest answer is that the crew is full, and handing
/// somebody a twelfth task they will not read is worse than telling the system
/// to wait.
///
/// This is the cadence rule from the plan applied to people rather than to
/// content. The machine issues a rhythm somebody can actually hold.
pub const MAX_OPEN_ASSIGNMENTS_PER_MEMBER: u16 = 7;

/// Why nobody was assigned.
///
/// Distinguished from "no decision" on purpose. A caller that cannot tell the
/// difference between *nobody has this skill* and *everybody who has it is
/// buried* will do the same thing in both cases, and those want opposite
/// responses: the first needs a person hired or a skill widened, the second
/// needs the work to wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamAssignmentRefusal {
    /// No active member holds the skill, at any load.
    NobodyWithTheSkill,
    /// Somebody could do it, and everybody who could is at the ceiling.
    /// Carries the smallest open-assignment count on the bench, so a caller can
    /// say *how* full rather than only *that* it is full.
    EveryoneAtCapacity { lowest_open_assignments: u16 },
}

impl TeamAssignmentRefusal {
    /// What to tell the operator. Written for somebody deciding what to do
    /// next, not for a log.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::NobodyWithTheSkill => {
                "nobody on the crew has this skill and is available — widen the skill on \
                 somebody, or this kind of work has no owner"
                    .to_owned()
            }
            Self::EveryoneAtCapacity {
                lowest_open_assignments,
            } => format!(
                "everybody who could take this already holds {lowest_open_assignments} open \
                 tasks, and the ceiling is {MAX_OPEN_ASSIGNMENTS_PER_MEMBER}. This one waits \
                 until something closes — a task nobody reads is not delegation."
            ),
        }
    }
}

/// Picks an assignee, or says why it did not.
///
/// # Errors
///
/// Refuses when nobody active holds the skill, and when everybody who holds it
/// is already at [`MAX_OPEN_ASSIGNMENTS_PER_MEMBER`].
pub fn route_team_assignment(
    members: &[TeamMemberRoutingSnapshot],
    need: TeamAssignmentNeed,
) -> Result<TeamAssignmentDecision, TeamAssignmentRefusal> {
    // Who *could* do it, ignoring load. Computed first so the refusal can tell
    // "no skill" apart from "no room" — a caller that cannot distinguish those
    // will respond to a hiring problem by waiting, and to a workload problem by
    // hiring.
    let capable = members
        .iter()
        .filter(|member| member.active && member.capacity_basis_points > 0)
        .filter(|member| {
            member.skills.contains(&need.primary_skill)
                || need
                    .secondary_skill
                    .is_some_and(|skill| member.skills.contains(&skill))
                || (need.allow_generalist && member.skills.contains(&TeamSkill::General))
        })
        .collect::<Vec<_>>();

    if capable.is_empty() {
        return Err(TeamAssignmentRefusal::NobodyWithTheSkill);
    }

    select_team_assignee(members, need).ok_or_else(|| TeamAssignmentRefusal::EveryoneAtCapacity {
        lowest_open_assignments: capable
            .iter()
            .map(|member| member.open_assignments)
            .min()
            .unwrap_or(MAX_OPEN_ASSIGNMENTS_PER_MEMBER),
    })
}

#[must_use]
pub fn select_team_assignee(
    members: &[TeamMemberRoutingSnapshot],
    need: TeamAssignmentNeed,
) -> Option<TeamAssignmentDecision> {
    members
        .iter()
        .filter(|member| member.active && member.capacity_basis_points > 0)
        // The ceiling. The load penalty below spreads work between people who
        // could take it; it never stopped work reaching somebody who already
        // has too much, because a penalty subtracts from a score that still
        // wins when there is nobody to lose to.
        .filter(|member| member.open_assignments < MAX_OPEN_ASSIGNMENTS_PER_MEMBER)
        .filter_map(|member| {
            let primary = member.skills.contains(&need.primary_skill);
            let secondary = need
                .secondary_skill
                .is_some_and(|skill| member.skills.contains(&skill));
            let general = need.allow_generalist && member.skills.contains(&TeamSkill::General);
            if !primary && !secondary && !general {
                return None;
            }

            let skill_score = if primary {
                10_000
            } else if secondary {
                7_000
            } else {
                4_000
            };
            let load_penalty = i32::from(member.open_assignments).saturating_mul(900)
                + i32::from(member.recent_assignments).saturating_mul(250);
            let capacity_bonus = i32::from(member.capacity_basis_points.min(10_000)) / 10;
            // Deliberately smaller than the gap between a primary and a
            // secondary skill (3_000). Follow-through decides between people
            // who can both do the work; it never hands a specialist's task to
            // someone unqualified just because they answer quickly.
            let follow_through_bonus =
                i32::from(member.follow_through_basis_points.min(10_000)) * 2_500 / 10_000;
            Some(TeamAssignmentDecision {
                member_id: member.member_id,
                member_key: member.member_key.clone(),
                route_score: skill_score + capacity_bonus + follow_through_bonus - load_penalty,
            })
        })
        .max_by(|left, right| {
            left.route_score
                .cmp(&right.route_score)
                // Reverse lexicographic tie-break so `max_by` chooses the stable
                // smallest key. No RNG means retries cannot reshuffle ownership.
                .then_with(|| right.member_key.cmp(&left.member_key))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Work should stop going to whoever never does it.
    ///
    /// Routing was skill fit and current load only. A member who left every
    /// assignment unfinished carried no open ones, so they looked *idle* and
    /// kept winning the next task — the queue balanced while nothing moved.
    #[test]
    fn work_routes_away_from_a_member_who_does_not_finish_it() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: false,
        };
        let mut absent = member("a-never-finishes", vec![TeamSkill::Social], 0);
        absent.follow_through_basis_points = 0;
        // Deliberately carrying work, so load alone would favour the other one.
        let mut reliable = member("b-finishes", vec![TeamSkill::Social], 1);
        reliable.follow_through_basis_points = 10_000;

        let decision =
            select_team_assignee(&[absent, reliable], need).expect("a qualified member exists");
        assert_eq!(
            decision.member_key, "b-finishes",
            "the member who completes this work should win it despite the heavier queue"
        );
    }

    /// Follow-through breaks ties between the qualified; it does not override
    /// qualification. Someone eager but wrong for the task still loses.
    #[test]
    fn follow_through_never_outranks_skill_fit() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: Some(TeamSkill::General),
            allow_generalist: true,
        };
        let mut specialist = member("a-specialist", vec![TeamSkill::Social], 0);
        specialist.follow_through_basis_points = 0;
        let mut generalist = member("b-generalist", vec![TeamSkill::General], 0);
        generalist.follow_through_basis_points = 10_000;

        let decision = select_team_assignee(&[specialist, generalist], need)
            .expect("a qualified member exists");
        assert_eq!(
            decision.member_key, "a-specialist",
            "a perfect follow-through record must not hand a specialist task to a generalist"
        );
    }

    /// A new member has not failed at anything yet.
    #[test]
    fn an_unproven_member_is_neutral_not_last() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: false,
        };
        let mut unproven = member("a-new", vec![TeamSkill::Social], 0);
        unproven.follow_through_basis_points = NEUTRAL_FOLLOW_THROUGH_BASIS_POINTS;
        let mut poor = member("b-poor", vec![TeamSkill::Social], 0);
        poor.follow_through_basis_points = 0;

        let decision =
            select_team_assignee(&[unproven, poor], need).expect("a qualified member exists");
        assert_eq!(
            decision.member_key, "a-new",
            "someone with no record should be tried before someone with a bad one"
        );
    }

    fn member(key: &str, skills: Vec<TeamSkill>, open: u16) -> TeamMemberRoutingSnapshot {
        TeamMemberRoutingSnapshot {
            member_id: WorkspaceMemberId::new(),
            member_key: key.to_owned(),
            active: true,
            skills,
            open_assignments: open,
            recent_assignments: 0,
            capacity_basis_points: 10_000,
            // Neutral by default so existing cases test the rules they were
            // written for. A member with no history scores neutral in
            // production too — see `NEUTRAL_FOLLOW_THROUGH_BASIS_POINTS`.
            follow_through_basis_points: NEUTRAL_FOLLOW_THROUGH_BASIS_POINTS,
        }
    }

    // ── The ceiling ─────────────────────────────────────────────────────────
    //
    // The load penalty spread work between people who could take it. It was
    // never a ceiling, and a solo act is its own social, visual and copy skill
    // — so every task routed to one person and the penalty subtracted from a
    // score that won anyway. The operator's word for the result was "flooded".

    #[test]
    fn a_buried_member_stops_receiving_work_even_with_nobody_to_hand_it_to() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: false,
        };
        let only_person = vec![member(
            "solo",
            vec![TeamSkill::Social],
            MAX_OPEN_ASSIGNMENTS_PER_MEMBER,
        )];
        assert_eq!(
            route_team_assignment(&only_person, need),
            Err(TeamAssignmentRefusal::EveryoneAtCapacity {
                lowest_open_assignments: MAX_OPEN_ASSIGNMENTS_PER_MEMBER,
            }),
            "the only capable person was at the ceiling and still got the task"
        );

        // One below the ceiling still takes it: the rule is a bound, not a
        // discouragement, and an off-by-one here costs the crew a task's worth
        // of capacity every week.
        let with_room = vec![member(
            "solo",
            vec![TeamSkill::Social],
            MAX_OPEN_ASSIGNMENTS_PER_MEMBER - 1,
        )];
        assert!(route_team_assignment(&with_room, need).is_ok());
    }

    #[test]
    fn a_full_bench_reads_differently_from_an_empty_one() {
        // These want opposite responses — one needs a skill widened, the other
        // needs the work to wait — so a caller must be able to tell them apart.
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: false,
        };
        let wrong_skills = vec![member("visual-only", vec![TeamSkill::Visual], 0)];
        assert_eq!(
            route_team_assignment(&wrong_skills, need),
            Err(TeamAssignmentRefusal::NobodyWithTheSkill)
        );

        let buried = vec![
            member(
                "a",
                vec![TeamSkill::Social],
                MAX_OPEN_ASSIGNMENTS_PER_MEMBER,
            ),
            member(
                "b",
                vec![TeamSkill::Social],
                MAX_OPEN_ASSIGNMENTS_PER_MEMBER + 4,
            ),
        ];
        assert_eq!(
            route_team_assignment(&buried, need),
            Err(TeamAssignmentRefusal::EveryoneAtCapacity {
                // The least-buried person, so the operator hears how far off
                // the crew is rather than only that it is off.
                lowest_open_assignments: MAX_OPEN_ASSIGNMENTS_PER_MEMBER,
            })
        );
    }

    #[test]
    fn a_member_with_room_wins_over_one_at_the_ceiling_regardless_of_skill_fit() {
        // Capability normally beats availability by 3,000 points. The ceiling
        // is not a penalty, so it outranks that: a specialist who cannot take
        // the work is not a candidate at all.
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: true,
        };
        let members = vec![
            member(
                "buried-specialist",
                vec![TeamSkill::Social],
                MAX_OPEN_ASSIGNMENTS_PER_MEMBER,
            ),
            member("free-generalist", vec![TeamSkill::General], 0),
        ];
        let decision = route_team_assignment(&members, need).expect("somebody can take it");
        assert_eq!(decision.member_key, "free-generalist");
    }

    #[test]
    fn a_refusal_tells_the_operator_what_to_do_about_it() {
        for refusal in [
            TeamAssignmentRefusal::NobodyWithTheSkill,
            TeamAssignmentRefusal::EveryoneAtCapacity {
                lowest_open_assignments: 9,
            },
        ] {
            let message = refusal.message();
            assert!(message.len() > 40, "too terse: {message}");
            assert!(
                !message.contains("Err(") && !message.contains("None"),
                "leaks Rust at the operator: {message}"
            );
        }
    }

    #[test]
    fn skill_fit_beats_random_assignment() {
        let members = vec![
            member(
                "member_1",
                vec![TeamSkill::General, TeamSkill::Technical],
                0,
            ),
            member(
                "member_2",
                vec![TeamSkill::Visual, TeamSkill::Video, TeamSkill::Social],
                1,
            ),
        ];
        let selected = select_team_assignee(
            &members,
            TeamAssignmentNeed {
                primary_skill: TeamSkill::Video,
                secondary_skill: Some(TeamSkill::Social),
                allow_generalist: true,
            },
        )
        .expect("suitable member");
        assert_eq!(selected.member_key, "member_2");
    }

    #[test]
    fn fair_load_balancing_avoids_overusing_generalist() {
        let members = vec![
            member("member_1", vec![TeamSkill::General, TeamSkill::Booking], 5),
            member("member_4", vec![TeamSkill::Booking, TeamSkill::People], 1),
        ];
        let selected = select_team_assignee(
            &members,
            TeamAssignmentNeed {
                primary_skill: TeamSkill::Booking,
                secondary_skill: Some(TeamSkill::People),
                allow_generalist: true,
            },
        )
        .expect("suitable member");
        assert_eq!(selected.member_key, "member_4");
    }
}
