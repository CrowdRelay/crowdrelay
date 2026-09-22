//! 4G.5 — what an approved proposal actually produced.
//!
//! The reasons a proposal carries are structured precisely so they can be
//! scored: "a room that books our genre", "240 reachable people", "the booker
//! answered last time" are hypotheses, and the honest question is which of
//! them turned out to predict a reply or a show. That tally is computed here
//! rather than written beside the decision — the decision's `input_snapshot`
//! already holds the reasons verbatim, and a second store would be a second
//! truth the first time they disagree.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// A show inside this many days of the approval counts as the proposal's
/// outcome. Unbounded, every city the band ever plays eventually scores every
/// proposal ever made for it, and the reason tally converges on "every reason
/// works" — which is the same as no tally at all.
const SHOW_ATTRIBUTION_WINDOW_DAYS: i64 = 120;

/// One approved proposal and what came of it, newest first.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProposalOutcome {
    pub city: String,
    pub venue: String,
    pub approved_at: OffsetDateTime,
    /// The action's own status — `queued` for a letter parked on a missing
    /// executor, `succeeded` once the outreach actually ran. A score only
    /// exists once the letter left.
    pub action_status: String,
    /// Promoters the letter went to.
    pub recipients: u32,
    /// The catalogue id of `city`, from the decision's subject. The slug in
    /// `city` is for reading; this is the identity a display-name lookup or a
    /// same-slug sibling needs.
    pub city_id: Uuid,
    /// Promoters who answered inside the seven-day window.
    pub replies: u32,
    /// Reply windows still open — a proposal with any of these is in flight
    /// and does not score yet.
    pub unfinished_measurements: u32,
    /// A show in the proposal's city, booked inside
    /// `SHOW_ATTRIBUTION_WINDOW_DAYS` of the approval.
    ///
    /// The window is the whole of the attribution. Unbounded, every city the
    /// band ever plays eventually scores every proposal ever made for it, and
    /// the reason tally converges on "every reason works" — which is the same
    /// as no tally at all.
    pub show_booked: bool,
    /// The stronger version: the show inside the window is at the room the
    /// proposal named. A city booking is evidence the letter helped; the named
    /// room is evidence it worked.
    pub show_booked_at_venue: bool,
    /// The reasons the approved proposal carried, as they were stored.
    pub reasons: Vec<crowdrelay_domain::gig_plan::Reason>,
    /// Reasons in the snapshot this build cannot read, counted rather than
    /// dropped silently.
    ///
    /// A snapshot is history: it was written by whatever the vocabulary was
    /// that day. Renaming or removing a `Reason` variant later must not make
    /// the whole track record unreadable, and it must not quietly shrink an
    /// old proposal's reason list either — a proposal that scored on three
    /// reasons and now reads as two is a silently rewritten record.
    pub unreadable_reasons: u32,
}

impl ProposalOutcome {
    /// Every reply window has closed and the letter genuinely left — the two
    /// conditions under which "nobody answered" is a fact rather than a guess.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.action_status == "succeeded"
            && self.recipients > 0
            && self.unfinished_measurements == 0
    }

    /// The letter was cancelled before it left, so nothing about it is
    /// evidence about the reasons it carried.
    ///
    /// Kept distinct from "settled with no reply", which is a real answer from
    /// a real promoter. A console that shows these as the same thing teaches
    /// the band that their reasons do not work, when the truth is that nothing
    /// was ever sent.
    #[must_use]
    pub fn never_sent(&self) -> bool {
        self.action_status == "cancelled"
    }
}

/// The tally the learning question is asked with: of the settled proposals
/// that carried this reason, how many produced a reply, and how many a show.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ReasonScore {
    /// The `Reason` variant tag — the vocabulary the proposals were made in.
    pub kind: &'static str,
    /// Settled proposals that carried it.
    pub proposals: u32,
    /// Of those, how many got at least one promoter reply.
    pub replies: u32,
    /// Of those, how many produced a show in the city.
    pub shows: u32,
}

/// What approved proposals have produced, and which reasons were on the ones
/// that worked.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GigPlanTrackRecord {
    /// Every approved proposal, newest first — including the in-flight ones,
    /// because a letter parked on a missing executor is a fact too.
    pub proposals: Vec<ProposalOutcome>,
    /// Per reason kind, over settled proposals only.
    pub by_reason: Vec<ReasonScore>,
}

#[derive(Debug, sqlx::FromRow)]
struct ProposalOutcomeRow {
    evaluated_at: OffsetDateTime,
    city_id: Uuid,
    city: Option<String>,
    venue: Option<String>,
    reasons: serde_json::Value,
    action_status: String,
    recipients: i64,
    replies: i64,
    unfinished: i64,
    show_booked: bool,
    show_booked_at_venue: bool,
}

/// Reads the decision → action → measurement → event chain for every
/// band-approved proposal (4G.5).
///
/// A reply is an inbound booking interaction measured by `BookingReply7d`
/// inside seven days; a show is a non-cancelled event in the proposal's city
/// created after the approval. Neither attribution is stronger than that —
/// a reply timed to the outreach is the promoter's answer, and a show that
/// appeared after the band wrote is the outcome the proposal was for.
///
/// # Errors
///
/// Propagates the database error.
pub async fn proposal_track_record(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<GigPlanTrackRecord, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProposalOutcomeRow>(
        r#"
        WITH proposals AS (
            SELECT decision.id AS decision_id,
                   decision.evaluated_at,
                   decision.subject_id AS city_id,
                   decision.input_snapshot ->> 'city' AS city,
                   decision.input_snapshot ->> 'venue' AS venue,
                   decision.input_snapshot -> 'reasons' AS reasons,
                   action.id AS action_id,
                   action.status AS action_status,
                   action.payload AS action_payload
            FROM viryaos_autopilot_decisions AS decision
            JOIN viryaos_autopilot_actions AS action
              ON action.workspace_id = decision.workspace_id
             AND action.decision_id = decision.id
            WHERE decision.workspace_id = $1
              AND decision.decision_kind = 'gig.proposal.approved'
        ), reply_counts AS (
            SELECT outcome.action_id,
                   count(*) FILTER (WHERE outcome.observed_value > 0)::bigint AS replies
            FROM viryaos_autopilot_outcomes AS outcome
            JOIN proposals ON proposals.action_id = outcome.action_id
            WHERE outcome.workspace_id = $1
              AND outcome.metric_key = 'effect.booking_reply_7d'
            GROUP BY outcome.action_id
        ), unfinished AS (
            SELECT measurement.action_id, count(*)::bigint AS n
            FROM viryaos_autopilot_measurements AS measurement
            JOIN proposals ON proposals.action_id = measurement.action_id
            WHERE measurement.workspace_id = $1
              AND measurement.status IN ('pending', 'processing')
            GROUP BY measurement.action_id
        )
        SELECT proposals.evaluated_at,
               proposals.city_id,
               proposals.city,
               proposals.venue,
               proposals.reasons,
               proposals.action_status,
               -- The room the letter addressed, from the action's own payload.
               -- Measurement outcomes would undercount it: a recipient whose
               -- observation failed is still somebody we wrote to.
               COALESCE(
                   jsonb_array_length(proposals.action_payload -> 'recipients'), 0
               )::bigint AS recipients,
               COALESCE(reply_counts.replies, 0) AS replies,
               COALESCE(unfinished.n, 0) AS unfinished,
               EXISTS (
                   SELECT 1 FROM events AS event
                   WHERE event.workspace_id = $1
                     AND event.city_id = proposals.city_id
                     AND event.created_at >= proposals.evaluated_at
                     AND event.created_at < proposals.evaluated_at + $2::interval
                     AND event.status IN ('published', 'completed')
               ) AS show_booked,
               EXISTS (
                   SELECT 1 FROM events AS event
                   WHERE event.workspace_id = $1
                     AND event.city_id = proposals.city_id
                     AND event.created_at >= proposals.evaluated_at
                     AND event.created_at < proposals.evaluated_at + $2::interval
                     AND event.status IN ('published', 'completed')
                     AND proposals.venue IS NOT NULL
                     AND lower(btrim(event.venue)) = lower(btrim(proposals.venue))
               ) AS show_booked_at_venue
        FROM proposals
        LEFT JOIN reply_counts ON reply_counts.action_id = proposals.action_id
        LEFT JOIN unfinished ON unfinished.action_id = proposals.action_id
        ORDER BY proposals.evaluated_at DESC
        "#,
    )
    .bind(workspace_id)
    .bind(time::Duration::days(SHOW_ATTRIBUTION_WINDOW_DAYS))
    .fetch_all(pool)
    .await?;

    let mut proposals = Vec::with_capacity(rows.len());
    for row in rows {
        // A snapshot is history, written in whatever the reason vocabulary was
        // that day. Decoding the list as a whole made one unreadable entry
        // fail the entire read — so renaming or retiring a `Reason` variant
        // would take every past proposal's track record down with it, for
        // good, on a surface whose entire job is to remember. Each entry is
        // decoded on its own; what this build cannot read is counted and
        // reported rather than dropped, because a proposal that quietly loses
        // a reason has had its record rewritten.
        let mut reasons = Vec::new();
        let mut unreadable_reasons = 0u32;
        match row.reasons {
            serde_json::Value::Array(entries) => {
                for entry in entries {
                    match serde_json::from_value::<crowdrelay_domain::gig_plan::Reason>(entry) {
                        Ok(reason) => reasons.push(reason),
                        Err(_) => unreadable_reasons = unreadable_reasons.saturating_add(1),
                    }
                }
            }
            // Not an array at all: the snapshot predates the field or was
            // written by something else. Counted as one unreadable reason so
            // the row still lists, with its record honestly incomplete.
            serde_json::Value::Null => {}
            _ => unreadable_reasons = 1,
        }
        proposals.push(ProposalOutcome {
            city_id: row.city_id,
            city: row.city.unwrap_or_default(),
            venue: row.venue.unwrap_or_default(),
            approved_at: row.evaluated_at,
            action_status: row.action_status,
            recipients: u32::try_from(row.recipients).unwrap_or(u32::MAX),
            replies: u32::try_from(row.replies).unwrap_or(u32::MAX),
            unfinished_measurements: u32::try_from(row.unfinished).unwrap_or(u32::MAX),
            show_booked: row.show_booked,
            show_booked_at_venue: row.show_booked_at_venue,
            reasons,
            unreadable_reasons,
        });
    }

    let mut by_reason_map: std::collections::BTreeMap<&'static str, ReasonScore> =
        std::collections::BTreeMap::new();
    for proposal in proposals.iter().filter(|proposal| proposal.is_settled()) {
        for reason in &proposal.reasons {
            let kind = reason_kind(reason);
            let entry = by_reason_map.entry(kind).or_insert(ReasonScore {
                kind,
                proposals: 0,
                replies: 0,
                shows: 0,
            });
            entry.proposals += 1;
            if proposal.replies > 0 {
                entry.replies += 1;
            }
            if proposal.show_booked {
                entry.shows += 1;
            }
        }
    }
    let mut by_reason: Vec<ReasonScore> = by_reason_map.into_values().collect();
    // The reasons that have worked before come first — the whole point of the
    // tally is that a track record should reorder what the band reads.
    by_reason.sort_by(|left, right| {
        right
            .shows
            .cmp(&left.shows)
            .then_with(|| right.replies.cmp(&left.replies))
            .then_with(|| left.kind.cmp(right.kind))
    });

    Ok(GigPlanTrackRecord {
        proposals,
        by_reason,
    })
}

/// The variant tag, which is the vocabulary the tally speaks in.
fn reason_kind(reason: &crowdrelay_domain::gig_plan::Reason) -> &'static str {
    use crowdrelay_domain::gig_plan::Reason;
    match reason {
        Reason::ComparableActsPlayedHere { .. } => "comparable_acts_played_here",
        Reason::ReachableAudience { .. } => "reachable_audience",
        Reason::RoomDraws { .. } => "room_draws",
        Reason::NeverPlayedButHasFans { .. } => "never_played_but_has_fans",
        Reason::OverdueReturn { .. } => "overdue_return",
        Reason::CoBillAddsAudience { .. } => "co_bill_adds_audience",
        Reason::WarmPromoter { .. } => "warm_promoter",
        Reason::RoomIsActive { .. } => "room_is_active",
        Reason::FansConvertedHere { .. } => "fans_converted_here",
    }
}
