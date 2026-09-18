//! The roster view (5.5) — every act's attention, pipeline and gaps on one
//! screen.
//!
//! The weekly brief (5.18) is a digest: what needs a decision this week, what
//! slipped, who is drifting. The overview is the standing page beside it —
//! the state each act is in right now, whether or not anything moved. Three
//! columns, all measured rather than inferred:
//!
//! * **Attention** is the act's position inside the shared fan-attention
//!   budget — what it spent (`viryaos_contact_touches`, trailing 30 days),
//!   who it cannot reach yet (governor rows still in cooldown), and which
//!   doors are closed (`do_not_contact`). The budget is org-wide, so the page
//!   also carries the org's total spend: one act's "3" means something
//!   different against an org spend of 4 than against 400.
//! * **Pipeline** is what is queued and what is coming — approvals waiting on
//!   a human with the same predicate the brief and the attention queue use,
//!   and the published shows on the act's calendar with the nearest date.
//! * **Gaps** are the empty spots a count cannot say — named, not zeroed.
//!   `no_upcoming_show`, `no_briefing`, `no_reachable_fans`. An act missing
//!   one of these is not failing; it is missing the thing, and the name is
//!   what the manager can act on.
//!
//! Order is the argument again: the act missing the most leads, then name and
//! id for a total order — the screen is a scan, and the gaps are the reason
//! to look.

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::WorkspaceId;

/// Gap: the act has no published show ahead of it — the pipeline column's
/// calendar is empty, and nothing is coming unless somebody books it.
pub const GAP_NO_UPCOMING_SHOW: &str = "no_upcoming_show";
/// Gap: the act's workspace has never issued a daily briefing — the brain is
/// running but has not yet reported a day to its operator.
pub const GAP_NO_BRIEFING: &str = "no_briefing";
/// Gap: zero active fans holding a latest `marketing` consent grant — the
/// send path exists but reaches nobody, so growth work has no audience to
/// spend attention on.
pub const GAP_NO_REACHABLE_FANS: &str = "no_reachable_fans";

/// One act's share of the shared attention budget, measured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActAttention {
    /// Outbound touches this act spent in the trailing 30 days — its draw on
    /// the org-wide budget 5.17 enforces.
    pub touches_30d: u32,
    /// Governor rows still cooling down — people this act must not contact
    /// yet. A cooldown is not a closed door; it reopens on its own.
    pub contacts_on_hold: u32,
    /// Governor rows marked `do_not_contact` — doors this act closed, or had
    /// closed for it. This count only goes down by hand.
    pub do_not_contact: u32,
}

/// One act's pipeline: what is waiting on a human and what is coming.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActPipeline {
    /// Approvals waiting on a human — the same `awaiting_approval` predicate
    /// with an open window the brief and the attention queue share, so the
    /// three surfaces can never disagree about the queue's depth.
    pub pending_decisions: u32,
    /// Published shows ahead of `now` — the calendar half of the pipeline.
    pub upcoming_shows: u32,
    /// The nearest of them. `None` means the calendar is empty — the same
    /// fact `no_upcoming_show` names, kept as a timestamp for the screen.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_show_at: Option<OffsetDateTime>,
}

/// One act's row on the roster screen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActOverview {
    pub workspace_id: WorkspaceId,
    /// The workspace's name — the same act name every other roster-plan read
    /// shows, so the screens agree.
    pub name: String,
    pub attention: ActAttention,
    pub pipeline: ActPipeline,
    /// The newest day the act's briefing spoke for. `None` is the honest
    /// "nothing yet" — it is also what `no_briefing` reads.
    pub latest_briefing_date: Option<Date>,
    /// Active fans holding a latest `marketing` consent grant — the send-path
    /// count, the audience the attention budget exists to reach. It sits on
    /// the row rather than under `attention` because it measures the fanbase,
    /// not the spend; it is what `no_reachable_fans` reads.
    pub reachable_fans: u32,
    /// The act's named empty spots, computed by [`compose`] — see the
    /// `GAP_*` constants. Empty means nothing is missing, not that nothing
    /// was checked.
    pub gaps: Vec<String>,
}

/// The page: the org's shared spend plus every member act's row, ordered for
/// a top-down scan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterOverview {
    pub organization_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    /// Every member act's touches summed — the shared budget's current draw.
    /// The per-act numbers mean nothing without it; beside it they are shares.
    pub org_touches_30d: u32,
    /// Ordered by [`compose`] — most gaps first. An organisation with no
    /// members is an empty list, which is the honest page.
    pub acts: Vec<ActOverview>,
}

/// Names an act's empty spots from its measured row, then orders the page.
///
/// The gap names are a closed vocabulary — a screen renders them, and a
/// manager learns them, so they are constants rather than free text. The
/// checks are deliberately simple: each names a measured field being empty,
/// never a judgement about why.
#[must_use]
pub fn compose(
    organization_id: Uuid,
    generated_at: OffsetDateTime,
    org_touches_30d: u32,
    mut acts: Vec<ActOverview>,
) -> RosterOverview {
    for act in &mut acts {
        act.gaps = gaps_for(act);
    }
    acts.sort_by(|a, b| {
        b.gaps
            .len()
            .cmp(&a.gaps.len())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.workspace_id.into_uuid().cmp(&b.workspace_id.into_uuid()))
    });
    RosterOverview {
        organization_id,
        generated_at,
        org_touches_30d,
        acts,
    }
}

/// The gaps a measured row earns, in the order they are listed.
fn gaps_for(act: &ActOverview) -> Vec<String> {
    let mut gaps = Vec::new();
    if act.pipeline.upcoming_shows == 0 {
        gaps.push(GAP_NO_UPCOMING_SHOW.to_owned());
    }
    if act.latest_briefing_date.is_none() {
        gaps.push(GAP_NO_BRIEFING.to_owned());
    }
    if act.reachable_fans == 0 {
        gaps.push(GAP_NO_REACHABLE_FANS.to_owned());
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(name: &str, shows: u32, briefing: Option<Date>, fans: u32) -> ActOverview {
        ActOverview {
            workspace_id: WorkspaceId::new(),
            name: name.to_owned(),
            attention: ActAttention {
                touches_30d: 0,
                contacts_on_hold: 0,
                do_not_contact: 0,
            },
            pipeline: ActPipeline {
                pending_decisions: 0,
                upcoming_shows: shows,
                next_show_at: None,
            },
            latest_briefing_date: briefing,
            reachable_fans: fans,
            gaps: Vec::new(),
        }
    }

    #[test]
    fn a_complete_act_has_no_gaps() {
        let overview = compose(
            Uuid::now_v7(),
            OffsetDateTime::UNIX_EPOCH,
            0,
            vec![act(
                "Busy",
                2,
                Some(Date::from_calendar_date(2026, time::Month::September, 17).unwrap()),
                40,
            )],
        );
        assert!(overview.acts[0].gaps.is_empty());
    }

    #[test]
    fn the_empty_act_names_every_gap() {
        let overview = compose(
            Uuid::now_v7(),
            OffsetDateTime::UNIX_EPOCH,
            0,
            vec![act("Quiet", 0, None, 0)],
        );
        assert_eq!(
            overview.acts[0].gaps,
            vec![
                GAP_NO_UPCOMING_SHOW.to_owned(),
                GAP_NO_BRIEFING.to_owned(),
                GAP_NO_REACHABLE_FANS.to_owned(),
            ]
        );
    }

    #[test]
    fn the_page_leads_with_the_act_missing_the_most() {
        let complete = act(
            "Busy",
            1,
            Some(Date::from_calendar_date(2026, time::Month::September, 17).unwrap()),
            10,
        );
        let empty = act("Quiet", 0, None, 0);
        let overview = compose(
            Uuid::now_v7(),
            OffsetDateTime::UNIX_EPOCH,
            5,
            vec![complete, empty],
        );
        assert_eq!(overview.acts[0].name, "Quiet");
        assert_eq!(overview.acts[1].name, "Busy");
        assert_eq!(overview.org_touches_30d, 5);
    }
}
