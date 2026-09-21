//! §4b-3's capture plan — one production day, one shot list, a counted
//! harvest.
//!
//! A production event is a day where material physically exists: the
//! shoot, the studio session, the show. The plan is the shot list issued
//! to the member holding the camera before the day, and the harvest is
//! the count afterwards — `content_sources` logged inside the
//! harvest window. The pure logic lives here so the sweep that runs it
//! stays mechanical: build the list, read the verdict, apply it.

use crowdrelay_domain::{
    content_engine::{CapturePlanStatus, ProductionEventKind, ProductionEventStatus},
    team_operations::{TeamAssignmentNeed, TeamSkill},
};
use time::{Date, Duration};

use crate::content_suggestions::covered_by_production;

/// Days after the production day during which logged material still
/// counts as that day's yield — footage gets offloaded and logged late,
/// but not a week late.
pub const HARVEST_WINDOW_DAYS: i64 = 3;
/// The yield a plan is judged against — §4b-3's "one shoot → ≥3
/// sources". Reaching it settles the plan immediately; the window
/// lapsing with less still credits what did land rather than calling a
/// filmed day abandoned.
pub const HARVEST_TARGET_SOURCES: i64 = 3;
/// Long enough to cover the day, short enough to finish on one.
pub const MAX_SHOTS: usize = 5;

/// One line on the shot list — the `items` JSONB the member reads.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureShot {
    /// What to bring back — a catalogue name for targeted shots, a
    /// coverage instruction for the baseline.
    pub item: String,
    /// Which camera — the format's roster skill for targeted shots, the
    /// day's primary camera for the baseline.
    pub skill: TeamSkill,
    /// The catalogue format the shot feeds, when it is targeted.
    pub format_key: Option<String>,
}

/// A format the band currently wants filmed, resolved to what the shot
/// list needs to say it. The caller joins open-suggestion and
/// active-arc format keys against the active catalogue — deduplicated
/// and ordered, so the list is deterministic.
#[derive(Clone, Debug, PartialEq)]
pub struct ShotCandidate {
    pub format_key: String,
    pub name: String,
    pub skill: TeamSkill,
}

/// What the day needs shot, in order.
///
/// Targeted shots come first — every candidate whose format this
/// production kind covers names a shot, so the plan harvests what the
/// engine already told the band it wants. The baseline coverage shot
/// follows when there is room: even a day with no pending need is still
/// a day of material nobody should lose.
#[must_use]
pub fn shot_list(kind: ProductionEventKind, candidates: &[ShotCandidate]) -> Vec<CaptureShot> {
    let mut shots: Vec<CaptureShot> = candidates
        .iter()
        .filter(|candidate| covered_by_production(&candidate.format_key, kind))
        .take(MAX_SHOTS)
        .map(|candidate| CaptureShot {
            item: candidate.name.clone(),
            skill: candidate.skill,
            format_key: Some(candidate.format_key.clone()),
        })
        .collect();
    if shots.len() < MAX_SHOTS
        && let Some(baseline) = baseline_shot(kind)
    {
        shots.push(baseline);
    }
    shots
}

/// The one thing worth filming on a day nothing targeted — the
/// difference between a production day and a day that happened.
fn baseline_shot(kind: ProductionEventKind) -> Option<CaptureShot> {
    let (item, skill) = match kind {
        ProductionEventKind::Shoot => (
            "Wide B-roll of the day — setups, breaks, the room",
            TeamSkill::Video,
        ),
        ProductionEventKind::Studio => {
            ("Takes and talk between songs, room tone", TeamSkill::Video)
        }
        ProductionEventKind::Rehearsal => (
            "One full run of the weakest song, one take",
            TeamSkill::Video,
        ),
        ProductionEventKind::Show => (
            "Soundcheck, doors, crowd, three songs front-of-house",
            TeamSkill::Video,
        ),
        // A festival slot is a stranger-dense room — the biggest warm
        // audience of the year belongs to the bill-mates. The baseline
        // covers them too: their crowd is who the harvest courts.
        ProductionEventKind::Festival => (
            "Our set front-of-house, the crowd between sets, one bill-mate's stage",
            TeamSkill::Video,
        ),
        ProductionEventKind::Drive => (
            "The drive itself — dashboard talk, scenery, arrival",
            TeamSkill::Video,
        ),
        ProductionEventKind::Photoshoot => (
            "The setups, and the candids between frames",
            TeamSkill::Photography,
        ),
        // A day the engine cannot name yields no list to hand anyone.
        ProductionEventKind::Other => return None,
    };
    Some(CaptureShot {
        item: item.to_owned(),
        skill,
        format_key: None,
    })
}

/// Who the plan routes to — a photoshoot wants the stills camera first,
/// every other day wants video first and stills as cover.
#[must_use]
pub fn capture_need(kind: ProductionEventKind) -> TeamAssignmentNeed {
    let (primary_skill, secondary_skill) = if kind == ProductionEventKind::Photoshoot {
        (TeamSkill::Photography, TeamSkill::Video)
    } else {
        (TeamSkill::Video, TeamSkill::Photography)
    };
    TeamAssignmentNeed {
        primary_skill,
        secondary_skill: Some(secondary_skill),
        allow_generalist: true,
    }
}

/// What the sweep should do with an open plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePlanVerdict {
    /// Nothing to settle yet.
    Keep,
    /// The day yielded material — the plan did its job.
    Done,
    /// The day is gone, or cancelled, and nothing came of it.
    Abandon,
}

/// The harvest verdict. `sources_landed` counts captured material —
/// active `video`/`story` content sources whose `occurred_at` sits
/// inside the harvest window. The calendar and discography kinds
/// (`event`, `release`, `show_completed`) are machine projections and
/// the caller never counts them: a show happening is not footage of
/// the show.
#[must_use]
pub fn capture_plan_verdict(
    plan_status: CapturePlanStatus,
    event_status: ProductionEventStatus,
    sources_landed: i64,
    scheduled_for: Date,
    today: Date,
) -> CapturePlanVerdict {
    if event_status == ProductionEventStatus::Cancelled {
        return CapturePlanVerdict::Abandon;
    }
    let window_end = scheduled_for + Duration::days(HARVEST_WINDOW_DAYS);
    match plan_status {
        CapturePlanStatus::Issued => {
            if sources_landed >= HARVEST_TARGET_SOURCES
                || (today > window_end && sources_landed > 0)
            {
                CapturePlanVerdict::Done
            } else if today > window_end {
                CapturePlanVerdict::Abandon
            } else {
                CapturePlanVerdict::Keep
            }
        }
        // A draft that outlived its day was never issued — nothing to
        // harvest and nothing to keep open.
        CapturePlanStatus::Draft => {
            if today > scheduled_for {
                CapturePlanVerdict::Abandon
            } else {
                CapturePlanVerdict::Keep
            }
        }
        _ => CapturePlanVerdict::Keep,
    }
}

#[cfg(test)]
mod tests {
    use time::Month;

    use super::*;

    fn day() -> Date {
        Date::from_calendar_date(2026, Month::October, 2).unwrap()
    }

    fn candidate(key: &str, name: &str) -> ShotCandidate {
        ShotCandidate {
            format_key: key.to_owned(),
            name: name.to_owned(),
            skill: TeamSkill::Video,
        }
    }

    #[test]
    fn needed_formats_become_targeted_shots_in_order() {
        let candidates = vec![
            candidate("aftermovie", "Aftermovie"),
            candidate("playthrough", "Playthrough"), // not show-covered
            candidate("soundcheck_clip", "Soundcheck clip"),
        ];
        let shots = shot_list(ProductionEventKind::Show, &candidates);
        // Playthrough is shoot-covered, not show-covered — it stays out.
        assert_eq!(
            shots
                .iter()
                .map(|s| s.format_key.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("aftermovie"), Some("soundcheck_clip"), None]
        );
        assert_eq!(shots[0].item, "Aftermovie");
        // The baseline fills the remaining room.
        assert!(shots[2].format_key.is_none());
    }

    #[test]
    fn a_day_with_no_needs_still_gets_its_baseline() {
        let shots = shot_list(ProductionEventKind::Rehearsal, &[]);
        assert_eq!(shots.len(), 1);
        assert!(shots[0].item.contains("weakest song"));
    }

    #[test]
    fn a_festival_days_baseline_covers_the_stranger_dense_room() {
        let shots = shot_list(ProductionEventKind::Festival, &[]);
        assert_eq!(shots.len(), 1);
        assert!(shots[0].item.contains("bill-mate"));
        assert_ne!(
            shots[0].item,
            shot_list(ProductionEventKind::Show, &[])[0].item
        );
    }

    #[test]
    fn shot_list_is_capped_and_other_gets_nothing() {
        let candidates: Vec<_> = (0..8)
            .map(|i| candidate("soundcheck_clip", &format!("f{i}")))
            .collect();
        let shots = shot_list(ProductionEventKind::Show, &candidates);
        assert_eq!(shots.len(), MAX_SHOTS);
        // `other` covers nothing and has no baseline — no shots at all.
        assert!(shot_list(ProductionEventKind::Other, &candidates).is_empty());
    }

    #[test]
    fn photoshoot_routes_to_stills_first() {
        let need = capture_need(ProductionEventKind::Photoshoot);
        assert_eq!(need.primary_skill, TeamSkill::Photography);
        assert_eq!(need.secondary_skill, Some(TeamSkill::Video));
        let need = capture_need(ProductionEventKind::Show);
        assert_eq!(need.primary_skill, TeamSkill::Video);
    }

    #[test]
    fn verdict_settles_done_at_target_or_partial_after_window() {
        let day = day();
        let issued = CapturePlanStatus::Issued;
        let sched = ProductionEventStatus::Scheduled;
        // Target met the same day — settle now.
        assert_eq!(
            capture_plan_verdict(issued, sched, 3, day, day),
            CapturePlanVerdict::Done
        );
        // Inside the window, under target — keep waiting.
        assert_eq!(
            capture_plan_verdict(issued, sched, 1, day, day + Duration::days(2)),
            CapturePlanVerdict::Keep
        );
        // Window lapsed with some yield — done, not punished.
        assert_eq!(
            capture_plan_verdict(issued, sched, 1, day, day + Duration::days(4)),
            CapturePlanVerdict::Done
        );
        // Window lapsed with nothing — abandoned.
        assert_eq!(
            capture_plan_verdict(issued, sched, 0, day, day + Duration::days(4)),
            CapturePlanVerdict::Abandon
        );
    }

    #[test]
    fn verdict_abandons_cancelled_days_and_stale_drafts() {
        let day = day();
        assert_eq!(
            capture_plan_verdict(
                CapturePlanStatus::Issued,
                ProductionEventStatus::Cancelled,
                5,
                day,
                day
            ),
            CapturePlanVerdict::Abandon
        );
        assert_eq!(
            capture_plan_verdict(
                CapturePlanStatus::Draft,
                ProductionEventStatus::Scheduled,
                0,
                day,
                day + Duration::days(1)
            ),
            CapturePlanVerdict::Abandon
        );
        // A draft whose day hasn't come yet keeps waiting.
        assert_eq!(
            capture_plan_verdict(
                CapturePlanStatus::Draft,
                ProductionEventStatus::Scheduled,
                0,
                day,
                day
            ),
            CapturePlanVerdict::Keep
        );
    }
}
