//! Human handoff routing for CrowdRelay.
//!
//! This is intentionally not a second task-management product. Bounded contexts
//! remain authoritative for approvals, show checklists and opportunities; this
//! module only selects a suitable human owner for work the Autopilot cannot or
//! must not complete itself.

use serde::{Deserialize, Serialize};

use crate::WorkspaceMemberId;

/// Asks one member may be handed in a rolling seven days when the tenant has
/// not set a number of their own.
///
/// # Why a default at all
///
/// The ceiling has always been the member's own number, and an unset one read
/// as uncapped. Nothing else about crew mail is uncapped by accident, and this
/// one was: `autopilot_policies.max_actions_24h` defaults to 50 across
/// 26 contexts, every `awaiting_approval` action becomes an assignment, and
/// every assignment owes a first notice plus up to three reminders. So the
/// only quantity with no ceiling was the one measured in a person's attention,
/// and it is the quantity that actually ran out: the operator, not the code,
/// was the throughput limit.
///
/// The envelope already bounds what the tenant's *audience* receives. This is
/// the same idea pointed at the crew, and it belongs here beside the router
/// that enforces it rather than in whichever reader looks first.
///
/// # Why ten
///
/// Ten asks is about two a day, which is what a band member with a job can
/// actually action. The number is deliberately about work rather than about
/// email — the sweep already digests a batch into one message, so the mail
/// count is not the thing that saturates a person; the number of decisions
/// waiting on them is.
///
/// It is a default, not a policy: a tenant who wants more sets more, and the
/// router honours whatever they set. `None` still means uncapped in
/// [`TeamMemberRoutingSnapshot`], because a reader that cannot express
/// "no ceiling" cannot report one.
pub const DEFAULT_WEEKLY_ASK_CEILING: u16 = 10;

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
    /// Assignments handed to this member in the last seven days — the
    /// weekly-ask side of §4i-6's "the machinery must not eat its own
    /// supply". `daily_briefing` rows never count: a read is not an ask.
    pub asks_last_7d: u16,
    /// The member's own weekly ceiling (§4i-6) — set by them, enforced by the
    /// router. `None` means uncapped; at the cap the work routes elsewhere or
    /// waits, and an ask nobody under cap can take is the system's to report,
    /// not the member's to absorb.
    pub weekly_ask_ceiling: Option<u16>,
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

/// Why an ask could not be routed (§4i-6). The distinction matters to the
/// person reading it: "nobody has the skill" is a roster gap, "everyone who
/// could is at their weekly ceiling" is the ceiling doing its job — the ask
/// waits rather than landing on somebody's plate uncounted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamRoutingRefusal {
    /// No active member has the primary, secondary, or generalist skill.
    NoSkillMatch,
    /// The skill exists on the roster but every member who holds it is at
    /// their weekly ask ceiling.
    AllEligibleAtCeiling,
    /// Nobody on the roster can take work at all — inactive or zero capacity.
    NoRoutableMembers,
}

impl TeamRoutingRefusal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoSkillMatch => "no_skill_match",
            Self::AllEligibleAtCeiling => "all_eligible_at_weekly_ceiling",
            Self::NoRoutableMembers => "no_routable_members",
        }
    }
}

/// Capability first, fairness second. Stable member-key tie breaking keeps
/// retries deterministic while still distributing work as workloads change.
/// What a member with no completed history scores.
///
/// Neutral rather than zero: a new member has not failed to do anything, and
/// starting them at the bottom would mean never giving them the first task that
/// would prove them either way.
pub const NEUTRAL_FOLLOW_THROUGH_BASIS_POINTS: u16 = 5_000;

#[must_use]
pub fn select_team_assignee(
    members: &[TeamMemberRoutingSnapshot],
    need: TeamAssignmentNeed,
) -> Option<TeamAssignmentDecision> {
    select_team_assignee_explained(members, need).ok()
}

/// `select_team_assignee` with the refusal spelled out. The sweep records the
/// reason so a waiting ask is visible as waiting — never silently dropped.
pub fn select_team_assignee_explained(
    members: &[TeamMemberRoutingSnapshot],
    need: TeamAssignmentNeed,
) -> Result<TeamAssignmentDecision, TeamRoutingRefusal> {
    let routable: Vec<&TeamMemberRoutingSnapshot> = members
        .iter()
        .filter(|member| member.active && member.capacity_basis_points > 0)
        .collect();
    if routable.is_empty() {
        return Err(TeamRoutingRefusal::NoRoutableMembers);
    }
    let skilled: Vec<&TeamMemberRoutingSnapshot> = routable
        .into_iter()
        .filter(|member| {
            member.skills.contains(&need.primary_skill)
                || need
                    .secondary_skill
                    .is_some_and(|skill| member.skills.contains(&skill))
                || (need.allow_generalist && member.skills.contains(&TeamSkill::General))
        })
        .collect();
    if skilled.is_empty() {
        return Err(TeamRoutingRefusal::NoSkillMatch);
    }
    // §4i-6: the weekly ask ceiling is the member's own number, and the
    // router honours it the same way it honours `active` — over the cap
    // is not a worse score, it is not eligible.
    let eligible: Vec<&TeamMemberRoutingSnapshot> = skilled
        .into_iter()
        .filter(|member| {
            member
                .weekly_ask_ceiling
                .is_none_or(|ceiling| member.asks_last_7d < ceiling)
        })
        .collect();
    if eligible.is_empty() {
        return Err(TeamRoutingRefusal::AllEligibleAtCeiling);
    }
    eligible
        .into_iter()
        .map(|member| {
            let primary = member.skills.contains(&need.primary_skill);
            let secondary = need
                .secondary_skill
                .is_some_and(|skill| member.skills.contains(&skill));
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
            TeamAssignmentDecision {
                member_id: member.member_id,
                member_key: member.member_key.clone(),
                route_score: skill_score + capacity_bonus + follow_through_bonus - load_penalty,
            }
        })
        .max_by(|left, right| {
            left.route_score
                .cmp(&right.route_score)
                // Reverse lexicographic tie-break so `max_by` chooses the stable
                // smallest key. No RNG means retries cannot reshuffle ownership.
                .then_with(|| right.member_key.cmp(&left.member_key))
        })
        .ok_or(TeamRoutingRefusal::NoSkillMatch)
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

    /// §4i-6: the weekly ceiling is a hard eligibility gate, not a score —
    /// a member at their cap cannot win an ask even on a perfect record.
    #[test]
    fn a_member_at_the_weekly_ceiling_is_not_eligible() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: false,
        };
        let mut capped = member("a-capped", vec![TeamSkill::Social], 0);
        capped.weekly_ask_ceiling = Some(3);
        capped.asks_last_7d = 3;
        let free = member("b-free", vec![TeamSkill::Social], 5);

        let decision =
            select_team_assignee(&[capped, free], need).expect("an uncapped member exists");
        assert_eq!(decision.member_key, "b-free");
    }

    /// The refusal names the reason: "everyone who could is at the ceiling"
    /// is a different operator action than "nobody has the skill".
    #[test]
    fn refusal_distinguishes_ceiling_from_missing_skill() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Video,
            secondary_skill: None,
            allow_generalist: false,
        };

        let mut capped = member("a-capped", vec![TeamSkill::Video], 0);
        capped.weekly_ask_ceiling = Some(2);
        capped.asks_last_7d = 2;
        assert_eq!(
            select_team_assignee_explained(&[capped], need),
            Err(TeamRoutingRefusal::AllEligibleAtCeiling)
        );

        let wrong_skill = member("b-wrong", vec![TeamSkill::Social], 0);
        assert_eq!(
            select_team_assignee_explained(&[wrong_skill], need),
            Err(TeamRoutingRefusal::NoSkillMatch)
        );

        let mut inactive = member("c-inactive", vec![TeamSkill::Video], 0);
        inactive.active = false;
        assert_eq!(
            select_team_assignee_explained(&[inactive], need),
            Err(TeamRoutingRefusal::NoRoutableMembers)
        );
    }

    /// Uncapped is the default: no ceiling set means the ceiling never bites.
    #[test]
    fn no_ceiling_means_uncapped() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: false,
        };
        let mut busy = member("a-busy", vec![TeamSkill::Social], 0);
        busy.asks_last_7d = 400;
        assert!(
            select_team_assignee(&[busy], need).is_some(),
            "no ceiling set must not cap anyone"
        );
    }

    /// `None` still means uncapped here, and the readers no longer produce
    /// `None` — they fall back to this number. The router has to actually
    /// stop at it, or the default is decoration.
    #[test]
    fn the_default_ceiling_stops_a_member_who_has_had_their_week() {
        let need = TeamAssignmentNeed {
            primary_skill: TeamSkill::Social,
            secondary_skill: None,
            allow_generalist: false,
        };
        let mut at_default = member("a-busy", vec![TeamSkill::Social], 0);
        at_default.weekly_ask_ceiling = Some(DEFAULT_WEEKLY_ASK_CEILING);
        at_default.asks_last_7d = DEFAULT_WEEKLY_ASK_CEILING;
        assert_eq!(
            select_team_assignee(&[at_default.clone()], need),
            None,
            "a member at the default ceiling is not eligible"
        );

        let mut under = at_default;
        under.asks_last_7d = DEFAULT_WEEKLY_ASK_CEILING - 1;
        assert!(
            select_team_assignee(&[under], need).is_some(),
            "one ask under the ceiling is still routable"
        );
    }

    /// The default is a working number, not a formality. A ceiling of one is
    /// unusable and one of several hundred is the uncapped behaviour wearing
    /// a number.
    #[test]
    fn the_default_ceiling_is_a_number_somebody_could_work_to() {
        assert!(
            (2..=50).contains(&DEFAULT_WEEKLY_ASK_CEILING),
            "a default of {DEFAULT_WEEKLY_ASK_CEILING} asks a week is not a week's work"
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
            asks_last_7d: 0,
            weekly_ask_ceiling: None,
            capacity_basis_points: 10_000,
            // Neutral by default so existing cases test the rules they were
            // written for. A member with no history scores neutral in
            // production too — see `NEUTRAL_FOLLOW_THROUGH_BASIS_POINTS`.
            follow_through_basis_points: NEUTRAL_FOLLOW_THROUGH_BASIS_POINTS,
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
