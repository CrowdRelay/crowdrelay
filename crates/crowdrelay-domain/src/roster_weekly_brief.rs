//! The roster-level weekly brief — one page a manager reads top-down.
//!
//! Per act, the machinery already exists: the daily briefing
//! (`viryaos_daily_briefings`, one per workspace per tenant-local day), the
//! approval queue (`viryaos_autopilot_actions` rows awaiting a human), and the
//! brain's self-assessment over the sixty-day North Star series. What was
//! missing is the roster's view of all three at once — a manager runs eight
//! acts, and "go check each one's panel" is not an answer to "who needs me
//! this week".
//!
//! Two rules the read keeps:
//!
//! * **Empty is rendered, never fabricated.** An act with nothing pending
//!   reports zero against an empty list; an act whose workspace never issued
//!   a briefing carries `latest_briefing_date: null`, which is itself the
//!   honest "nothing yet" — a missing briefing is information, not a zero
//!   date.
//! * **Order is the argument.** Acts waiting on a human come first, then acts
//!   whose brain reports itself drifting (`stagnant`/`regressing`), then the
//!   quiet ones. The manager reads top-down, so the sort is the message and
//!   it lives here rather than in SQL or the handler — a test can reach it
//!   without a database, and every surface that ever shows this page orders
//!   it identically.

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::WorkspaceId;

/// How far back "what slipped" looks, in days.
///
/// Seven, matching the weekly cadence the page is read on: an ask that died
/// eight days ago was last week's news, and the briefing that covers the same
/// span is the page this sits beside.
pub const WINDOW_DAYS: u32 = 7;

/// The most items of one kind shown per act before the count takes over.
///
/// Four matches the briefing's own pending-asks list: enough to recognize
/// what is waiting, short enough that one act's backlog cannot push the rest
/// of the roster off the page. The totals carry the full count either way.
pub const MAX_ITEMS_SHOWN: i64 = 4;

/// One approval waiting on a human, reduced to what a roster line needs.
///
/// Kind and subject, not the payload — the manager needs to recognize the
/// item and find it, and the act's own queue (`ops/attention`,
/// `PendingAutopilotAction`) is where its full detail already lives. This
/// read names things; it does not reproduce them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingItem {
    pub action_id: Uuid,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: Uuid,
    /// When the window to answer closes. `None` means it does not lapse —
    /// different from a deadline already gone, which is dead work rather
    /// than a decision and is excluded from this list entirely.
    #[serde(with = "time::serde::rfc3339::option")]
    pub approval_expires_at: Option<OffsetDateTime>,
}

/// One ask that died unanswered inside the window.
///
/// `resolution` is the queue's own verdict carried verbatim —
/// `approval_expired` (a window closed with nobody answering) and
/// `insufficient_evidence` (a connector failure wearing a proposal's shape,
/// cleared without being answerable) are different failures and stay
/// distinct. An unknown future kind would pass through rather than be
/// dropped, which is why this is a string and not an enum.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlippedItem {
    pub action_id: Uuid,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: Uuid,
    /// `approval_expired` or `insufficient_evidence`, verbatim.
    pub resolution: String,
    /// When it died — the sweep writes `finished_at` as it cancels.
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
}

/// One act's week, in the shape the manager reads it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActBrief {
    pub workspace_id: WorkspaceId,
    /// The workspace's name — the same act name `roster_opportunity` and the
    /// source-ROI read already show the manager, so the three pages agree.
    pub name: String,
    /// The brain's own verdict over its sixty-day North Star series:
    /// `improving`, `learning`, `stagnant`, `regressing` or `initializing`.
    /// Computed by `crowdrelay_brain::self_assessment::assess` — this page
    /// carries the verdict, it does not re-derive it.
    pub posture: String,
    /// True only for `stagnant` and `regressing` — the brain's own
    /// `needs_attention`, not a second judgement of the same series.
    pub posture_needs_attention: bool,
    /// Distinct days of North Star readings behind the verdict. It is what
    /// makes `initializing` legible: a young act has not been watched long
    /// enough to have an opinion, which is different from one that cannot
    /// decide.
    pub days_observed: u32,
    /// Every approval waiting on a human right now — the count is the queue's
    /// real depth, even when `pending` shows only the first few.
    pub pending_decisions: u32,
    /// The first few pending items, soonest deadline first.
    pub pending: Vec<PendingItem>,
    /// Asks that died unanswered in the last [`WINDOW_DAYS`] days — the real
    /// count, even when `slipped_items` shows only the first few.
    pub slipped: u32,
    /// The most recent deaths, newest first.
    pub slipped_items: Vec<SlippedItem>,
    /// The tenant-local day the act's newest briefing speaks for. `None`
    /// means none has ever issued — stated as null, never as a zero date.
    pub latest_briefing_date: Option<Date>,
}

/// The page: one organisation's acts, each with its week.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterWeeklyBrief {
    pub organization_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub window_days: u32,
    /// Ordered for a top-down read — see [`compose`]. An organisation with
    /// no member workspaces is an empty list, which is the honest page:
    /// there is nobody's week to show.
    pub acts: Vec<ActBrief>,
}

/// Wraps the measured acts into the page, ordered the way it is read.
///
/// Three buckets: anything waiting on a decision first, then acts whose
/// brain reports itself `stagnant` or `regressing`, then the quiet ones.
/// Inside a bucket the deeper queue leads, then the act that slipped most,
/// then name and id for a total order — two reads of unchanged data can
/// never disagree about which act is third.
#[must_use]
pub fn compose(
    organization_id: Uuid,
    generated_at: OffsetDateTime,
    mut acts: Vec<ActBrief>,
) -> RosterWeeklyBrief {
    acts.sort_by(|left, right| order_key(left).cmp(&order_key(right)));
    RosterWeeklyBrief {
        organization_id,
        generated_at,
        window_days: WINDOW_DAYS,
        acts,
    }
}

fn order_key(
    act: &ActBrief,
) -> (
    u8,
    std::cmp::Reverse<u32>,
    std::cmp::Reverse<u32>,
    &str,
    Uuid,
) {
    let bucket = if act.pending_decisions > 0 {
        0
    } else if act.posture_needs_attention {
        1
    } else {
        2
    };
    (
        bucket,
        std::cmp::Reverse(act.pending_decisions),
        std::cmp::Reverse(act.slipped),
        act.name.as_str(),
        act.workspace_id.into_uuid(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn act(name: &str, pending: u32, slipped: u32, attention: bool) -> ActBrief {
        ActBrief {
            workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(name.len() as u128 + 1)),
            name: name.to_owned(),
            posture: if attention { "stagnant" } else { "learning" }.to_owned(),
            posture_needs_attention: attention,
            days_observed: 6,
            pending_decisions: pending,
            pending: Vec::new(),
            slipped,
            slipped_items: Vec::new(),
            latest_briefing_date: None,
        }
    }

    #[test]
    fn decisions_first_then_drifting_then_quiet() {
        let acts = vec![
            act("quiet-act", 0, 0, false),
            act("drifting-act", 0, 2, true),
            act("busy-act", 3, 0, false),
        ];
        let brief = compose(Uuid::now_v7(), datetime!(2026-10-05 09:00 UTC), acts);
        let names: Vec<&str> = brief.acts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["busy-act", "drifting-act", "quiet-act"]);
    }

    #[test]
    fn within_a_bucket_the_deeper_queue_leads() {
        let acts = vec![act("one-ask", 1, 0, false), act("four-asks", 4, 0, false)];
        let brief = compose(Uuid::now_v7(), datetime!(2026-10-05 09:00 UTC), acts);
        let names: Vec<&str> = brief.acts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["four-asks", "one-ask"]);
    }

    /// A drifting act with a deep slip history still waits behind every act
    /// with a live decision — the queue is the only thing a manager can
    /// answer today.
    #[test]
    fn a_backlog_of_dead_asks_does_not_outrank_a_live_one() {
        let acts = vec![
            act("slipped-hard", 0, 9, true),
            act("one-pending", 1, 0, false),
        ];
        let brief = compose(Uuid::now_v7(), datetime!(2026-10-05 09:00 UTC), acts);
        assert_eq!(brief.acts[0].name, "one-pending");
    }

    #[test]
    fn ordering_is_total_and_deterministic() {
        let acts = vec![
            act("beta", 0, 0, false),
            act("alpha", 0, 0, false),
            act("beta", 0, 0, false),
        ];
        let brief = compose(Uuid::now_v7(), datetime!(2026-10-05 09:00 UTC), acts);
        let names: Vec<&str> = brief.acts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "beta"]);
        // Same input twice, same page — the id is the last tiebreak.
        let again = compose(
            Uuid::now_v7(),
            datetime!(2026-10-05 09:00 UTC),
            vec![
                act("beta", 0, 0, false),
                act("alpha", 0, 0, false),
                act("beta", 0, 0, false),
            ],
        );
        let ids: Vec<Uuid> = brief
            .acts
            .iter()
            .map(|a| a.workspace_id.into_uuid())
            .collect();
        let again_ids: Vec<Uuid> = again
            .acts
            .iter()
            .map(|a| a.workspace_id.into_uuid())
            .collect();
        assert_eq!(ids, again_ids);
    }

    /// The honest empty page: an organisation with no members renders as an
    /// empty list, not an error and not invented acts.
    #[test]
    fn an_empty_roster_is_an_empty_page() {
        let brief = compose(Uuid::now_v7(), datetime!(2026-10-05 09:00 UTC), Vec::new());
        assert!(brief.acts.is_empty());
        assert_eq!(brief.window_days, WINDOW_DAYS);
    }
}
