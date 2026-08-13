//! Human handoff routing for ViryaOS.
//!
//! This is intentionally not a second task-management product. Bounded contexts
//! remain authoritative for approvals, show checklists and opportunities; this
//! module only selects a suitable human owner for work the Autopilot cannot or
//! must not complete itself.

use serde::{Deserialize, Serialize};

use crate::WorkspaceMemberId;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
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
#[must_use]
pub fn select_team_assignee(
    members: &[TeamMemberRoutingSnapshot],
    need: TeamAssignmentNeed,
) -> Option<TeamAssignmentDecision> {
    members
        .iter()
        .filter(|member| member.active && member.capacity_basis_points > 0)
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
            Some(TeamAssignmentDecision {
                member_id: member.member_id,
                member_key: member.member_key.clone(),
                route_score: skill_score + capacity_bonus - load_penalty,
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

    fn member(key: &str, skills: Vec<TeamSkill>, open: u16) -> TeamMemberRoutingSnapshot {
        TeamMemberRoutingSnapshot {
            member_id: WorkspaceMemberId::new(),
            member_key: key.to_owned(),
            active: true,
            skills,
            open_assignments: open,
            recent_assignments: 0,
            capacity_basis_points: 10_000,
        }
    }

    #[test]
    fn skill_fit_beats_random_assignment() {
        let members = vec![
            member("wojtek", vec![TeamSkill::General, TeamSkill::Technical], 0),
            member(
                "lubek",
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
        assert_eq!(selected.member_key, "lubek");
    }

    #[test]
    fn fair_load_balancing_avoids_overusing_generalist() {
        let members = vec![
            member("wojtek", vec![TeamSkill::General, TeamSkill::Booking], 5),
            member("marcin", vec![TeamSkill::Booking, TeamSkill::People], 1),
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
        assert_eq!(selected.member_key, "marcin");
    }
}
