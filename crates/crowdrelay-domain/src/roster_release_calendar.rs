//! The roster's release calendar (5.16) — §4i-2's collision rule in the
//! second dimension.
//!
//! Two acts on one roster releasing the same week compete for the same
//! press, the same playlist slots and, where their audiences overlap, the
//! same fans. No band needs this; every label has it. What the read owes a
//! manager is not a report of the clash afterwards but a proposal, stated
//! the way §4i-2 states it: which release moves, and the reason — never a
//! silent reorder.
//!
//! The rule is the plan's own: the release that loses least by moving is
//! the one that moves. A Single outranks a Track outranks a Filler — the
//! bigger moment is the one the week was committed around. Equal tiers
//! break on readiness, because a release whose assets exist is further
//! committed than one still collecting them; equal on both, the earlier
//! announced date keeps the week it claimed. What is never the tiebreak:
//! seniority, or whose workspace id sorts first by accident — name is the
//! last key only so two runs cannot disagree.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::WorkspaceId;
use crate::release_autopilot::ReleaseTier;

/// How far ahead the calendar reads — twelve weeks is two release ladders
/// of lookahead: far enough that a collision is still movable, short enough
/// that a planned-but-unannounced far-future release is not dragged into
/// next quarter's argument.
pub const LOOKAHEAD_WEEKS: u32 = 12;

/// One act's upcoming release as it stands on the calendar.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterRelease {
    pub workspace_id: WorkspaceId,
    pub act_name: String,
    pub release_id: Uuid,
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub release_at: OffsetDateTime,
    pub tier: ReleaseTier,
    /// Whether the release's own asset declaration is complete — the
    /// readiness half of "which one loses less by moving".
    pub assets_ready: bool,
}

/// A proposed move: the release that should yield the week, and the reason
/// stated the way the plan demands — citing the rule that decided, never
/// just the answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseCollision {
    /// The Monday (UTC) the colliding releases share — the frame the
    /// proposal argues about.
    pub week_start: time::Date,
    /// Every release in the collision, in standing order — the act that
    /// keeps the week first, then each mover.
    pub releases: Vec<RosterRelease>,
    /// Who keeps the week.
    pub stays_workspace_id: WorkspaceId,
    pub stays_act: String,
    /// Who is asked to move, and the sentence that says why.
    pub moves_workspace_id: WorkspaceId,
    pub moves_act: String,
    pub reason: String,
    /// How many active fans the two acts share across the organisation —
    /// the count-only overlap between the two workspaces' fanbases. The
    /// overlap query measures every member pair, so `Some(0)` is a counted
    /// zero: a collision between audiences that share nobody is a press
    /// clash only, and the reason says so either way.
    pub shared_fans: Option<u32>,
}

/// The calendar page: the lookahead window's releases by week, and every
/// collision with its proposed move.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterReleaseCalendar {
    pub organization_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub lookahead_weeks: u32,
    /// Releases inside the window, soonest first — the whole calendar, not
    /// only the clashes, because a week with one release is the answer to
    /// "is next month clear" as much as a collision is the answer to "who
    /// moves".
    pub releases: Vec<RosterRelease>,
    pub collisions: Vec<ReleaseCollision>,
}

/// The Monday of the UTC week containing `at`. ISO weeks are the frame —
/// a release "this week" means the same Monday-to-Sunday everywhere the
/// roster reads it, and pinning UTC rather than a tenant zone is what keeps
/// two members of the same org agreeing on which week a date is in.
fn week_start(at: OffsetDateTime) -> time::Date {
    at.date() - time::Duration::days(i64::from(at.weekday().number_from_monday()) - 1)
}

/// How committed a release is — the sort key that decides who keeps the
/// week. Higher keeps: bigger tier, ready assets, the earlier date. Name
/// is the last key so the answer is total.
fn standing_key(release: &RosterRelease) -> impl Ord + use<'_> {
    (
        match release.tier {
            ReleaseTier::Single => 2u8,
            ReleaseTier::Track => 1,
            ReleaseTier::Filler => 0,
        },
        release.assets_ready,
        // Earlier release_at keeps the week: Reverse under a max-picking
        // sort, so the *smaller* timestamp wins.
        std::cmp::Reverse(release.release_at),
        release.act_name.as_str(),
        release.workspace_id.into_uuid(),
    )
}

/// Proposes the move for one colliding week.
///
/// The releases are ranked by standing — highest keeps the week, the next
/// is asked to move. Weeks with three releases name one mover (the weakest
/// standing) rather than a cascade: one proposal a manager can act on, not
/// a bracket. The reason cites the deciding rule verbatim so the answer is
/// a decision, not an ordering.
#[must_use]
pub fn propose_move(
    week_start: time::Date,
    releases: Vec<RosterRelease>,
    shared_fans: Option<u32>,
) -> Option<ReleaseCollision> {
    if releases.len() < 2 {
        return None;
    }
    let mut ranked = releases;
    ranked.sort_by(|left, right| standing_key(right).cmp(&standing_key(left)));
    let mover = ranked.get(1)?.clone();
    let anchor = ranked.first()?.clone();
    let rule = match (anchor.tier, mover.tier) {
        (a, m) if a != m => {
            format!(
                "{m} moves before {a} — the bigger moment keeps the week",
                a = tier_name(anchor.tier),
                m = tier_name(mover.tier),
            )
        }
        _ if anchor.assets_ready != mover.assets_ready => {
            "the release whose assets are still incomplete moves — it can slip for less".to_owned()
        }
        _ => "the later-announced release moves — the earlier one claimed the week".to_owned(),
    };
    let overlap_note = match shared_fans {
        Some(0) => {
            "; the two audiences share nobody measurable — this is a press clash only".to_owned()
        }
        Some(shared) => format!(
            "; the two acts share {shared} active fans — the clash costs fans, not only press"
        ),
        None => String::new(),
    };
    Some(ReleaseCollision {
        week_start,
        releases: ranked,
        stays_workspace_id: anchor.workspace_id,
        stays_act: anchor.act_name,
        moves_workspace_id: mover.workspace_id,
        moves_act: mover.act_name.clone(),
        reason: format!(
            "{mover} releases the same week as {anchor}. {rule}{overlap_note}. Move it a week later.",
            mover = mover.title,
            anchor = anchor.title,
        ),
        shared_fans,
    })
}

fn tier_name(tier: ReleaseTier) -> &'static str {
    match tier {
        ReleaseTier::Single => "a Single",
        ReleaseTier::Track => "a Track",
        ReleaseTier::Filler => "a Filler",
    }
}

/// Composes the calendar: releases already loaded by the caller, grouped by
/// week, each collision carrying its proposal.
#[must_use]
pub fn compose(
    organization_id: Uuid,
    generated_at: OffsetDateTime,
    releases: Vec<RosterRelease>,
    // (workspace_a, workspace_b) → shared active-fan count — the per-pair
    // citation for the reason line. Keyed canonically: the smaller id first.
    shared_by_pair: std::collections::HashMap<(Uuid, Uuid), u32>,
) -> RosterReleaseCalendar {
    let mut by_week: std::collections::BTreeMap<time::Date, Vec<RosterRelease>> =
        std::collections::BTreeMap::new();
    for release in &releases {
        by_week
            .entry(week_start(release.release_at))
            .or_default()
            .push(release.clone());
    }
    let collisions = by_week
        .into_iter()
        .filter_map(|(week, week_releases)| {
            if week_releases.len() < 2 {
                return None;
            }
            // The pair under proposal: the two strongest standings — the
            // anchor and the mover the reason names.
            let mut ranked = week_releases;
            ranked.sort_by(|left, right| standing_key(right).cmp(&standing_key(left)));
            let (a, b) = (
                ranked.first()?.workspace_id.into_uuid(),
                ranked.get(1)?.workspace_id.into_uuid(),
            );
            let pair = if a < b { (a, b) } else { (b, a) };
            // The shared-fan query counts every member pair — a missing
            // row is a measured zero, never an unmeasured one.
            let shared = shared_by_pair.get(&pair).copied().or(Some(0));
            propose_move(week, ranked, shared)
        })
        .collect();
    RosterReleaseCalendar {
        organization_id,
        generated_at,
        lookahead_weeks: LOOKAHEAD_WEEKS,
        releases,
        collisions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn release(
        name: &str,
        title: &str,
        at: time::OffsetDateTime,
        tier: ReleaseTier,
        ready: bool,
    ) -> RosterRelease {
        RosterRelease {
            workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(name.len() as u128 + 7)),
            act_name: name.to_owned(),
            release_id: Uuid::now_v7(),
            title: title.to_owned(),
            release_at: at,
            tier,
            assets_ready: ready,
        }
    }

    /// The bigger moment keeps the week; the reason says so.
    #[test]
    fn a_filler_moves_before_a_single() {
        let collision = propose_move(
            week_start(datetime!(2026-10-12 12:00 UTC)),
            vec![
                release(
                    "act-a",
                    "Loose track",
                    datetime!(2026-10-14 12:00 UTC),
                    ReleaseTier::Filler,
                    true,
                ),
                release(
                    "act-b",
                    "The single",
                    datetime!(2026-10-16 12:00 UTC),
                    ReleaseTier::Single,
                    false,
                ),
            ],
            Some(40),
        )
        .expect("two releases in a week collide");
        assert_eq!(collision.moves_act, "act-a");
        assert_eq!(collision.stays_act, "act-b");
        assert!(collision.reason.contains("a Single"));
        assert!(collision.reason.contains("40"));
    }

    /// Equal tiers break on readiness, then on who announced first — never
    /// on an accident of ordering.
    #[test]
    fn readiness_then_announcement_order_decides() {
        let same_week = datetime!(2026-10-14 12:00 UTC);
        let collision = propose_move(
            week_start(same_week),
            vec![
                release("act-a", "Unready", same_week, ReleaseTier::Track, false),
                release(
                    "act-b",
                    "Ready",
                    same_week + time::Duration::days(1),
                    ReleaseTier::Track,
                    true,
                ),
            ],
            Some(0),
        )
        .expect("collision");
        assert_eq!(
            collision.moves_act, "act-a",
            "the unready release slips for less"
        );
        assert!(collision.reason.contains("assets are still incomplete"));
        assert!(collision.reason.contains("share nobody"));
    }

    /// A week with one release is not a collision, and neither is a week
    /// with none — the calendar does not invent clashes.
    #[test]
    fn one_release_is_not_a_collision() {
        let week = week_start(datetime!(2026-10-14 12:00 UTC));
        assert!(
            propose_move(
                week,
                vec![release(
                    "a",
                    "t",
                    datetime!(2026-10-14 12:00 UTC),
                    ReleaseTier::Track,
                    true
                )],
                None
            )
            .is_none()
        );
    }

    /// Week arithmetic pinned: a Sunday and the Monday after it belong to
    /// different weeks, which is the whole reason the rule exists.
    #[test]
    fn the_week_is_iso_and_utc() {
        assert_eq!(
            week_start(datetime!(2026-10-18 23:59 UTC)),
            time::macros::date!(2026 - 10 - 12)
        );
        assert_eq!(
            week_start(datetime!(2026-10-19 00:01 UTC)),
            time::macros::date!(2026 - 10 - 19)
        );
    }
}
