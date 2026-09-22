//! The roster snapshot and the selector — who can take work, and whom the
//! router hands it to. The sweep in `team.rs` owns the handoff lifecycle;
//! this file owns the standing questions "what does the team look like right
//! now" and "which member should get this ask" (§4i-6).

use super::*;
use crowdrelay_domain::{
    WorkspaceMemberId,
    team_operations::{
        TeamAssignmentNeed, TeamMemberRoutingSnapshot, TeamRoutingRefusal, TeamSkill,
        select_team_assignee_explained,
    },
};

#[derive(Debug, FromRow)]
pub(in crate::autopilot) struct TeamRoutingRow {
    pub member_id: Uuid,
    pub member_key: String,
    pub display_name: String,
    pub normalized_email: String,
    pub active: bool,
    pub skills: Vec<String>,
    pub capacity_basis_points: i32,
    pub open_assignments: i64,
    pub recent_assignments: i64,
    /// Asks handed to the member in the last seven days (§4i-6) — the router
    /// holds a member at their own weekly ceiling, briefing rows excluded.
    pub asks_last_7d: i64,
    /// The workspace's weekly ask ceiling, applied per member. `None` means
    /// the tenant set no cap. Not a column — the sweep loads it once from
    /// `tenant_settings` and stamps it on every row, so the field defaults.
    #[sqlx(default)]
    pub weekly_ask_ceiling: Option<i32>,
    pub follow_through_basis_points: i32,
    /// Skills this member has settled history for, paired 1:1 with
    /// `skill_follow_through`. Two arrays rather than a map because SQLx
    /// decodes `text[]` and `int[]` directly.
    pub skill_follow_through_skills: Vec<String>,
    pub skill_follow_through: Vec<i32>,
}

pub(in crate::autopilot) async fn load_team_routing(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<TeamRoutingRow>, RepositoryError> {
    // §4i-6: the weekly ask ceiling is the tenant's own setting, and whatever
    // number they set binds every member the same way.
    //
    // An absent setting used to mean uncapped, which made a person's attention
    // the one quantity in this system with no ceiling — and the one that ran
    // out. It now falls back to `DEFAULT_WEEKLY_ASK_CEILING`, which the daily
    // briefing reports against too, so the number an operator reads and the
    // number the router enforces cannot disagree.
    //
    // A stored value outside the writable range is treated as unset rather
    // than as uncapped: the API refuses 0 for a stated reason, and a row that
    // got past it by hand must not be a wider grant than the API would give.
    let weekly_ask_ceiling = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings
         WHERE workspace_id = $1 AND key = 'team_weekly_ask_ceiling'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .and_then(|value| value.trim().parse::<i32>().ok())
    .filter(|ceiling| (1..=500).contains(ceiling))
    .or(Some(i32::from(
        crowdrelay_domain::team_operations::DEFAULT_WEEKLY_ASK_CEILING,
    )));
    let mut team = sqlx::query_as::<_, TeamRoutingRow>(
        r#"SELECT profile.member_id, profile.member_key, member.display_name,
                  member.normalized_email, profile.active, profile.skills,
                  profile.capacity_basis_points,
                  -- 'daily_briefing' and 'roster_weekly_brief' rows are
                  -- reads, not work: they are excluded everywhere in this
                  -- roster so the ones the sweeps hand members neither
                  -- consume capacity nor teach the follow-through metric
                  -- that the member "settles work without completing it".
                  COUNT(assignment.id) FILTER (
                      WHERE assignment.status='open'
                        AND assignment.source_kind NOT IN ('daily_briefing','roster_weekly_brief')
                  ) open_assignments,
                  COUNT(assignment.id) FILTER (
                      WHERE assignment.assigned_at >= $2 - INTERVAL '30 days'
                        AND assignment.source_kind NOT IN ('daily_briefing','roster_weekly_brief')
                  ) recent_assignments,
                  COUNT(assignment.id) FILTER (
                      WHERE assignment.assigned_at >= $2 - INTERVAL '7 days'
                        AND assignment.source_kind NOT IN ('daily_briefing','roster_weekly_brief')
                  ) asks_last_7d,
                  -- Follow-through: of the work this member was given and that
                  -- has had time to be done, how much did they actually finish,
                  -- and how much chasing did it take?
                  --
                  -- Each completion is worth 10000 minus 2500 per reminder, so
                  -- a task done unprompted counts fully and one that needed
                  -- three reminders counts for little. Anything settled and not
                  -- completed counts zero. Members with no settled history get
                  -- the neutral score instead of a zero they did not earn.
                  --
                  -- Only assignments older than a day are considered, so work
                  -- handed out this morning is not scored as ignored.
                  COALESCE((
                      SELECT AVG(
                          CASE WHEN history.completed_at IS NOT NULL
                               THEN GREATEST(0, 10000 - 2500 * LEAST(4, COALESCE(history.reminder_count, 0)))
                               ELSE 0
                          END
                      )::integer
                      FROM team_assignments history
                      WHERE history.workspace_id = profile.workspace_id
                        AND history.assignee_member_id = profile.member_id
                        AND history.status <> 'open'
                        AND history.source_kind NOT IN ('daily_briefing','roster_weekly_brief')
                        AND history.assigned_at < $2 - INTERVAL '1 day'
                  ), 5000) AS follow_through_basis_points,
                  -- The same measure, split by the skill the work needed.
                  -- Whole-member reliability answers "does this person finish
                  -- things"; routing needs "does this person finish *this*".
                  -- Someone who never gets round to press mail may be the
                  -- first to edit a video, and averaging the two hides both.
                  COALESCE(per_skill.skills, ARRAY[]::text[]) AS skill_follow_through_skills,
                  COALESCE(per_skill.scores, ARRAY[]::integer[]) AS skill_follow_through
           FROM team_profiles profile
           JOIN workspace_members member
             ON member.workspace_id=profile.workspace_id AND member.id=profile.member_id
           LEFT JOIN team_assignments assignment
             ON assignment.workspace_id=profile.workspace_id AND assignment.assignee_member_id=profile.member_id
           LEFT JOIN LATERAL (
               SELECT array_agg(skill.required_skill ORDER BY skill.required_skill) AS skills,
                      array_agg(skill.score ORDER BY skill.required_skill) AS scores
               FROM (
                   SELECT history.required_skill,
                          AVG(
                              CASE WHEN history.completed_at IS NOT NULL
                                   THEN GREATEST(0, 10000 - 2500 * LEAST(4, COALESCE(history.reminder_count, 0)))
                                   ELSE 0
                              END
                          )::integer AS score
                   FROM team_assignments history
                   WHERE history.workspace_id = profile.workspace_id
                     AND history.assignee_member_id = profile.member_id
                     AND history.status <> 'open'
                     AND history.source_kind NOT IN ('daily_briefing','roster_weekly_brief')
                     AND history.assigned_at < $2 - INTERVAL '1 day'
                   GROUP BY history.required_skill
               ) skill
           ) per_skill ON true
           WHERE profile.workspace_id=$1 AND profile.active AND member.status='active'
           -- `profile.workspace_id` is grouped because the follow-through
           -- subquery correlates on it. Postgres only infers functional
           -- dependency from a grouped primary key, and this table's key is
           -- (workspace_id, member_id) — grouping half of it left the other
           -- half ungrouped, and the scalar subquery in the SELECT list is
           -- evaluated after grouping, so the planner refused the whole
           -- statement with "subquery uses ungrouped column
           -- profile.workspace_id from outer query". Every autopilot cycle
           -- then reported a failed phase and no human handoff was ever
           -- assigned. The WHERE clause already pins the column to one value,
           -- so grouping by it changes no result.
           GROUP BY profile.workspace_id, profile.member_id, profile.member_key, member.display_name,
                    member.normalized_email, profile.active, profile.skills, profile.capacity_basis_points,
                    per_skill.skills, per_skill.scores
           ORDER BY profile.member_key"#,
    )
    .bind(workspace_id.into_uuid()).bind(now)
    .fetch_all(&mut **tx).await.map_err(map_sqlx)?;
    for member in &mut team {
        member.weekly_ask_ceiling = weekly_ask_ceiling;
    }
    Ok(team)
}

/// This member's follow-through on the skill actually being routed.
///
/// Falls back to their overall record when they have no settled history for
/// this skill, and to neutral when they have none at all. Whole-member
/// reliability is the weaker signal: the reason to measure at all is that
/// people are not uniformly diligent, and averaging across every kind of work
/// hides exactly the difference routing needs to see. Someone who never gets
/// round to press mail may be first to cut a video.
fn follow_through_for(member: &TeamRoutingRow, need: TeamAssignmentNeed) -> u16 {
    let wanted = need.primary_skill.as_str();
    member
        .skill_follow_through_skills
        .iter()
        .position(|skill| skill == wanted)
        .and_then(|index| member.skill_follow_through.get(index).copied())
        .map_or_else(
            || bounded_u16(i64::from(member.follow_through_basis_points)),
            |score| bounded_u16(i64::from(score)),
        )
}

/// Skill-fit-first, fairness-second selection over the live routing snapshot.
/// Shared by the scheduled routers and the `member_key="auto"` assignment path.
pub(in crate::autopilot) fn select_member_index(
    team: &[TeamRoutingRow],
    need: TeamAssignmentNeed,
) -> Option<usize> {
    select_member_index_explained(team, need).ok()
}

/// `select_member_index` with the refusal spelled out (§4i-6). Callers that
/// can act on the reason — the sweep records it — use this; callers that only
/// need a member-or-none keep the `Option` form.
pub(in crate::autopilot) fn select_member_index_explained(
    team: &[TeamRoutingRow],
    need: TeamAssignmentNeed,
) -> Result<usize, TeamRoutingRefusal> {
    let snapshots = team
        .iter()
        .map(|member| TeamMemberRoutingSnapshot {
            member_id: WorkspaceMemberId::from_uuid(member.member_id),
            member_key: member.member_key.clone(),
            active: member.active,
            skills: member
                .skills
                .iter()
                .filter_map(|skill| parse_team_skill(skill))
                .collect(),
            open_assignments: bounded_u16(member.open_assignments),
            recent_assignments: bounded_u16(member.recent_assignments),
            asks_last_7d: bounded_u16(member.asks_last_7d),
            weekly_ask_ceiling: member
                .weekly_ask_ceiling
                .and_then(|ceiling| u16::try_from(ceiling).ok()),
            capacity_basis_points: bounded_u16(i64::from(member.capacity_basis_points)),
            follow_through_basis_points: follow_through_for(member, need),
        })
        .collect::<Vec<_>>();
    let decision = select_team_assignee_explained(&snapshots, need)?;
    // The decision's member came from `snapshots`, which was built from `team`
    // in order — the position exists. `NoRoutableMembers` is the honest error
    // for the unreachable miss; routing to index 0 would hand work to the
    // wrong person.
    team.iter()
        .position(|member| member.member_id == decision.member_id.into_uuid())
        .ok_or(TeamRoutingRefusal::NoRoutableMembers)
}

pub(super) fn parse_team_skill(value: &str) -> Option<TeamSkill> {
    TeamSkill::parse(value)
}

fn bounded_u16(value: i64) -> u16 {
    u16::try_from(value.clamp(0, i64::from(u16::MAX))).unwrap_or(u16::MAX)
}
