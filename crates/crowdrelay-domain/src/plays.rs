//! The agent's unit of work: a multi-step campaign that survives a restart.
//!
//! Every other context here answers one question per cycle and forgets. That is
//! the right shape for a detector and the wrong shape for a campaign, because
//! the useful growth work is not one message — it is a message now, a second one
//! after the show, and a check on whether either moved anything. Without durable
//! state, step two either never happens or happens every cycle.
//!
//! Four properties decide whether this is safe to leave running.
//!
//! 1. **A step carries its own [`ActionClass`].** The class ceiling already
//!    downgrades an outward step to `require_approval` on its own, so a play
//!    needs no separate permission system and cannot smuggle a curator email
//!    through an owned-audience play.
//! 2. **A gated step never blocks the play.** A third-party step waiting on a
//!    human does not stop the owned-audience step behind it. A campaign that
//!    stalls entirely because nobody clicked approve is a campaign that teaches
//!    an operator to ignore it.
//! 3. **A missed step is skipped and recorded, never silently dropped.** Each
//!    step has a window measured from the anchor. Past it, the step is written
//!    as skipped with its reason. A post-show ask sent three weeks late is worse
//!    than one not sent, and an unrecorded omission is worse than both.
//! 4. **Steps are due, not scheduled.** The due time is derived from the
//!    anchor, so the existing autopilot cycle finds them by asking "what is due
//!    now". No new scheduler, no timer to lose across a deploy.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{
    PlayId, action_class::ActionClass, autonomy::Confidence, learning::LearningPolicy,
    play_measurement::PlayMeasurementPolicy,
};

/// What a play's schedule hangs off.
///
/// Every play until the follow-ask ladder was anchored on a show, and the
/// anchor was therefore implicit. It cannot stay implicit: a ladder climbed by
/// one fan over months has no show in it, and a play whose anchor kind is
/// assumed is a play whose audience query reads the wrong table.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayAnchorKind {
    /// A published show. Its date is in the future, so steps may precede it.
    Event,
    /// One fan. The anchor moment is when they qualified, so every step of such
    /// a play follows it.
    Fan,
    /// A release plan. The anchor moment is `release_at`; steps may precede it
    /// (pre-save, announce, curator wave) or follow it (release-day push,
    /// sustain ask).
    Release,
}

impl PlayAnchorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Fan => "fan",
            Self::Release => "release",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "event" => Some(Self::Event),
            "fan" => Some(Self::Fan),
            "release" => Some(Self::Release),
            _ => None,
        }
    }
}

/// A campaign the agent knows how to run.
///
/// Deliberately a closed set. A play is a piece of judgement about how this band
/// grows, and one assembled at runtime from operator-supplied parts is a rule
/// nobody reviewed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayKind {
    /// Ask the fans around a show to follow the band where new dates are
    /// announced, timed to the announce and to the day after the show.
    ///
    /// The most under-used free lever the band has: a fan who tracks the artist
    /// is told about every future date without the band paying for reach, and
    /// the two moments a fan is most willing to press that button are when a
    /// date near them is announced and the morning after they enjoyed one.
    TrackUsAsk,
    /// Make sure every published upcoming show has a complete free listing
    /// before anybody is asked to look at it.
    ///
    /// The cheapest work in the whole system and the least glamorous: a show
    /// that is not listed, or listed without a ticket link, cannot be found by
    /// the people already looking for it. Nothing here contacts anyone.
    ListingCompletenessSweep,
    /// Ask one engaged fan, three times across four months, to follow the band
    /// where future dates are announced.
    ///
    /// The track-us ask reaches the fans of one show at the moment that show is
    /// the thing on their mind. It therefore never reaches the fan who bought a
    /// ticket last spring and has had no date near them since — which is most
    /// of the list, most of the time. This is the ladder for exactly those
    /// people: one ask, then a nudge, then a last one, each with a single call
    /// to action and months between them.
    FollowAskLadder,
    /// Write once more to a fan who was here and stopped being here.
    ///
    /// A quiet fan is not a lost one, and the list already holds the people who
    /// bought a ticket two years ago and have heard nothing since. This is the
    /// cheapest audience the band has left, and the only one it currently does
    /// nothing with. Two messages, then the agent stops.
    DormantRevival,
    /// The release runway: pre-save surface, owned-audience announce, curator
    /// wave, release-day push, and a sustain ask. A release is the biggest
    /// organic growth moment the band gets, and the runway is the sequence
    /// that makes it compound rather than spike and fade.
    ReleaseRunway,
}

impl PlayKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TrackUsAsk => "track_us_ask",
            Self::ListingCompletenessSweep => "listing_completeness_sweep",
            Self::FollowAskLadder => "follow_ask_ladder",
            Self::DormantRevival => "dormant_revival",
            Self::ReleaseRunway => "release_runway",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "track_us_ask" => Some(Self::TrackUsAsk),
            "listing_completeness_sweep" => Some(Self::ListingCompletenessSweep),
            "follow_ask_ladder" => Some(Self::FollowAskLadder),
            "dormant_revival" => Some(Self::DormantRevival),
            "release_runway" => Some(Self::ReleaseRunway),
            _ => None,
        }
    }

    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::TrackUsAsk,
            Self::ListingCompletenessSweep,
            Self::FollowAskLadder,
            Self::DormantRevival,
            Self::ReleaseRunway,
        ]
    }

    /// What this play's schedule hangs off.
    #[must_use]
    pub const fn anchor_kind(self) -> PlayAnchorKind {
        match self {
            Self::TrackUsAsk | Self::ListingCompletenessSweep => PlayAnchorKind::Event,
            Self::FollowAskLadder | Self::DormantRevival => PlayAnchorKind::Fan,
            Self::ReleaseRunway => PlayAnchorKind::Release,
        }
    }

    /// What the play claims will happen, recorded when it starts so the claim
    /// can be read back against the measurement rather than reconstructed.
    #[must_use]
    pub const fn hypothesis(self) -> &'static str {
        match self {
            Self::TrackUsAsk => {
                "fans reached around a show will follow the band where future dates are announced, \
                 raising the tracker count that future announcements reach for free"
            }
            Self::ListingCompletenessSweep => {
                "a show that is listed completely, with a working ticket link, is found by people \
                 already searching for it, so the free listing surfaces reach more of them"
            }
            Self::FollowAskLadder => {
                "an engaged fan asked once, and twice more over the months that follow, will \
                 follow the band where dates are announced, so every future date reaches them \
                 without the band paying for it"
            }
            Self::DormantRevival => {
                "a fan who came once and went quiet will come back when there is a date near them \
                 and somebody remembers to tell them, so the count of fans active in a month rises"
            }
            Self::ReleaseRunway => {
                "a release runway — pre-save, announce, curator wave, release-day push and sustain \
                 — compounds the biggest organic growth moment the band gets, raising followers and \
                 saves beyond what a single release-day post achieves"
            }
        }
    }

    /// The series this play is judged against. Correlational, never claimed as
    /// attribution: the measurement layer decides what may be inferred.
    #[must_use]
    pub const fn success_metric(self) -> (&'static str, &'static str) {
        match self {
            Self::TrackUsAsk => ("bandsintown", "trackers"),
            // The listing is first-party work whose reach is not first-party
            // measurable: nothing tells us how many people found a complete
            // listing. The tracker series is the number a complete listing is
            // supposed to move, and the measurement layer reports it as
            // correlational — which is exactly what it is.
            Self::ListingCompletenessSweep => ("bandsintown", "trackers"),
            // The same series, and the same one every rung asks for. A ladder
            // whose rungs pointed at different platforms would be measured
            // against a number two of its three messages never mentioned, and
            // the platform the agent can actually read is this one.
            Self::FollowAskLadder => ("bandsintown", "trackers"),
            // First-party and exactly the thing being attempted: a revived fan
            // is a fan who became active again. Unlike a tracker count, this is
            // a number the workspace owns and observes directly.
            Self::DormantRevival => ("signal", "activated_fans_30d"),
            // Spotify followers are the number a release runway is supposed to
            // move, and the feed adapter (Phase 11) makes them observable. The
            // measurement layer reports this as correlational, which is what it
            // is — a release week and a show announcement land in the same
            // fortnight.
            Self::ReleaseRunway => ("spotify", "followers"),
        }
    }

    /// The play's steps, in order.
    #[must_use]
    pub const fn steps(self) -> &'static [PlayStepSpec] {
        match self {
            Self::TrackUsAsk => &TRACK_US_ASK_STEPS,
            Self::ListingCompletenessSweep => &LISTING_SWEEP_STEPS,
            Self::FollowAskLadder => &FOLLOW_ASK_LADDER_STEPS,
            Self::DormantRevival => &DORMANT_REVIVAL_STEPS,
            Self::ReleaseRunway => &RELEASE_RUNWAY_STEPS,
        }
    }
}

/// One step of a play, defined relative to the anchor rather than to a clock.
///
/// `offset_hours` may be negative: an announce-time step happens before the show
/// it is anchored to.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlayStepSpec {
    pub index: u16,
    pub kind: PlayStepKind,
    pub class: ActionClass,
    /// When the step becomes due, measured from the anchor.
    pub offset_hours: i32,
    /// How long it stays sendable. Past this the step is skipped, because a
    /// step delivered far outside its moment is a different, worse message.
    pub window_hours: u32,
}

const TRACK_US_ASK_STEPS: [PlayStepSpec; 2] = [
    // Fourteen days out: late enough that the date is real and near enough that
    // a fan reading it is thinking about this show rather than a hypothetical.
    PlayStepSpec {
        index: 0,
        kind: PlayStepKind::AnnounceAsk,
        class: PlayStepKind::AnnounceAsk.action_class(),
        offset_hours: -14 * 24,
        window_hours: 7 * 24,
    },
    // The morning after. The single highest-intent moment the band gets, and
    // the one it currently does nothing with.
    PlayStepSpec {
        index: 1,
        kind: PlayStepKind::PostShowAsk,
        class: PlayStepKind::PostShowAsk.action_class(),
        offset_hours: 18,
        window_hours: 3 * 24,
    },
];

// Three weeks out: far enough that a missing listing can still be fixed before
// anyone looks for the show, and near enough that the date is real.
const LISTING_SWEEP_STEPS: [PlayStepSpec; 1] = [PlayStepSpec {
    index: 0,
    kind: PlayStepKind::ListingSweep,
    class: PlayStepKind::ListingSweep.action_class(),
    offset_hours: -21 * 24,
    window_hours: 14 * 24,
}];

// Every rung follows the anchor, because the anchor is the moment the fan
// qualified rather than a date in the future. The spacing is the point: the
// same ask three times in a month is nagging, and the same ask three times
// across four months is a campaign. Fourteen days of window each, so a rung
// that cannot be sent this week is still the right message next week and a
// recorded skip after that.
const FOLLOW_ASK_LADDER_STEPS: [PlayStepSpec; 3] = [
    PlayStepSpec {
        index: 0,
        kind: PlayStepKind::FollowAskFirst,
        class: PlayStepKind::FollowAskFirst.action_class(),
        offset_hours: 0,
        window_hours: 14 * 24,
    },
    PlayStepSpec {
        index: 1,
        kind: PlayStepKind::FollowAskSecond,
        class: PlayStepKind::FollowAskSecond.action_class(),
        offset_hours: 45 * 24,
        window_hours: 14 * 24,
    },
    PlayStepSpec {
        index: 2,
        kind: PlayStepKind::FollowAskFinal,
        class: PlayStepKind::FollowAskFinal.action_class(),
        offset_hours: 120 * 24,
        window_hours: 14 * 24,
    },
];

// Two messages and then the agent stops. Somebody who has ignored the band for
// a year and then ignored two reminders has answered, and a third would be the
// campaign talking to itself.
const DORMANT_REVIVAL_STEPS: [PlayStepSpec; 2] = [
    PlayStepSpec {
        index: 0,
        kind: PlayStepKind::DormantRevivalFirst,
        class: PlayStepKind::DormantRevivalFirst.action_class(),
        offset_hours: 0,
        window_hours: 14 * 24,
    },
    PlayStepSpec {
        index: 1,
        kind: PlayStepKind::DormantRevivalFinal,
        class: PlayStepKind::DormantRevivalFinal.action_class(),
        offset_hours: 45 * 24,
        window_hours: 14 * 24,
    },
];

// The release runway. Five steps, anchored on `release_at`:
//
//   T-28d  pre-save surface live     (first-party, reaches nobody)
//   T-14d  owned-audience announce   (free, to consented fans)
//   T-7d   curator wave queued        (third-party, approval-gated)
//   T+0    release-day push           (free, to consented fans)
//   T+14d  sustain ask                (free, to consented fans)
//
// The curator wave is the only third-party step; everything else is owned
// audience or first-party work, so the runway runs autonomously except for
// the one step that contacts outsiders.
const RELEASE_RUNWAY_STEPS: [PlayStepSpec; 5] = [
    PlayStepSpec {
        index: 0,
        kind: PlayStepKind::ReleasePresaveLive,
        class: PlayStepKind::ReleasePresaveLive.action_class(),
        offset_hours: -28 * 24,
        window_hours: 7 * 24,
    },
    PlayStepSpec {
        index: 1,
        kind: PlayStepKind::ReleaseAudienceAnnounce,
        class: PlayStepKind::ReleaseAudienceAnnounce.action_class(),
        offset_hours: -14 * 24,
        window_hours: 7 * 24,
    },
    PlayStepSpec {
        index: 2,
        kind: PlayStepKind::ReleaseCuratorWave,
        class: PlayStepKind::ReleaseCuratorWave.action_class(),
        offset_hours: -7 * 24,
        window_hours: 4 * 24,
    },
    PlayStepSpec {
        index: 3,
        kind: PlayStepKind::ReleaseDayPush,
        class: PlayStepKind::ReleaseDayPush.action_class(),
        offset_hours: 0,
        window_hours: 3 * 24,
    },
    PlayStepSpec {
        index: 4,
        kind: PlayStepKind::ReleaseSustainAsk,
        class: PlayStepKind::ReleaseSustainAsk.action_class(),
        offset_hours: 14 * 24,
        window_hours: 7 * 24,
    },
];

/// Who a step is sent to.
///
/// Not every step has recipients. A listing sweep is work on the band's own
/// surfaces, and running it through an audience check would settle it as
/// `no_eligible_recipients` — a campaign refusing to do the one thing that
/// needs nobody's consent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepAudience {
    /// Consented fans, one action each.
    Fans,
    /// Nobody. The step runs once for its anchor.
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayStepKind {
    AnnounceAsk,
    PostShowAsk,
    ListingSweep,
    FollowAskFirst,
    FollowAskSecond,
    FollowAskFinal,
    DormantRevivalFirst,
    DormantRevivalFinal,
    ReleasePresaveLive,
    ReleaseAudienceAnnounce,
    ReleaseCuratorWave,
    ReleaseDayPush,
    ReleaseSustainAsk,
}

impl PlayStepKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnnounceAsk => "announce_ask",
            Self::PostShowAsk => "post_show_ask",
            Self::ListingSweep => "listing_sweep",
            Self::FollowAskFirst => "follow_ask_first",
            Self::FollowAskSecond => "follow_ask_second",
            Self::FollowAskFinal => "follow_ask_final",
            Self::DormantRevivalFirst => "dormant_revival_first",
            Self::DormantRevivalFinal => "dormant_revival_final",
            Self::ReleasePresaveLive => "release_presave_live",
            Self::ReleaseAudienceAnnounce => "release_audience_announce",
            Self::ReleaseCuratorWave => "release_curator_wave",
            Self::ReleaseDayPush => "release_day_push",
            Self::ReleaseSustainAsk => "release_sustain_ask",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "announce_ask" => Some(Self::AnnounceAsk),
            "post_show_ask" => Some(Self::PostShowAsk),
            "listing_sweep" => Some(Self::ListingSweep),
            "follow_ask_first" => Some(Self::FollowAskFirst),
            "follow_ask_second" => Some(Self::FollowAskSecond),
            "follow_ask_final" => Some(Self::FollowAskFinal),
            "dormant_revival_first" => Some(Self::DormantRevivalFirst),
            "dormant_revival_final" => Some(Self::DormantRevivalFinal),
            "release_presave_live" => Some(Self::ReleasePresaveLive),
            "release_audience_announce" => Some(Self::ReleaseAudienceAnnounce),
            "release_curator_wave" => Some(Self::ReleaseCuratorWave),
            "release_day_push" => Some(Self::ReleaseDayPush),
            "release_sustain_ask" => Some(Self::ReleaseSustainAsk),
            _ => None,
        }
    }

    /// The message template the executor renders. One per step, because a step
    /// that reuses another's copy is a step whose result cannot be read.
    #[must_use]
    pub const fn template_key(self) -> &'static str {
        match self {
            Self::AnnounceAsk => "play.track_us.announce",
            Self::PostShowAsk => "play.track_us.post_show",
            Self::ListingSweep => "play.listing.sweep",
            // One template per rung, because three sends of the same copy are
            // three sends whose separate results cannot be read — and because
            // the third message to somebody who ignored two has to say
            // something the first did not.
            Self::FollowAskFirst => "play.follow_ask.first",
            Self::FollowAskSecond => "play.follow_ask.second",
            Self::FollowAskFinal => "play.follow_ask.final",
            Self::DormantRevivalFirst => "play.dormant_revival.first",
            Self::DormantRevivalFinal => "play.dormant_revival.final",
            Self::ReleasePresaveLive => "play.release_runway.presave_live",
            Self::ReleaseAudienceAnnounce => "play.release_runway.audience_announce",
            Self::ReleaseCuratorWave => "play.release_runway.curator_wave",
            Self::ReleaseDayPush => "play.release_runway.release_day_push",
            Self::ReleaseSustainAsk => "play.release_runway.sustain_ask",
        }
    }

    /// Why this step is worth sending, recorded on the decision so an operator
    /// reading the queue sees the argument rather than the mechanism.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::AnnounceAsk => {
                "a date near this fan is announced, which is one of the two moments they are most \
                 willing to follow the band where future dates appear"
            }
            Self::PostShowAsk => {
                "the fan was at the show yesterday, the highest-intent moment the band gets and the \
                 one it currently does nothing with"
            }
            Self::ListingSweep => {
                "a published show three weeks out is worth checking is listed completely, because \
                 an incomplete listing cannot be found by the people already looking"
            }
            Self::FollowAskFirst => {
                "this fan has come to a show or said they wanted to, and has never been asked to \
                 follow the band where the next date is announced"
            }
            Self::FollowAskSecond => {
                "six weeks after the first ask, and a single message that goes unread is not an \
                 answer — one nudge is the difference between asking and not"
            }
            Self::FollowAskFinal => {
                "the last rung. Four months after the first ask, and after this the ladder is \
                 finished for this fan whatever they did with it"
            }
            Self::DormantRevivalFirst => {
                "this fan came to a show or wanted to, has done nothing for a year, and there is a \
                 date coming that is a reason to write to them"
            }
            Self::DormantRevivalFinal => {
                "six weeks on and no sign of life. This is the last message, and after it the \
                 agent leaves them alone"
            }
            Self::ReleasePresaveLive => {
                "the pre-save surface must be live before anybody is asked to use it, and a \
                 release four weeks out is the moment to check"
            }
            Self::ReleaseAudienceAnnounce => {
                "the band's own audience is the first place a release should be heard, and two \
                 weeks out is early enough to act on it and late enough that it is real"
            }
            Self::ReleaseCuratorWave => {
                "independent curators need lead time to listen, so a wave queued a week before \
                 release is the latest that still gives them one"
            }
            Self::ReleaseDayPush => {
                "the release is out today, and the one message that says so is the single \
                 highest-reach moment in the runway"
            }
            Self::ReleaseSustainAsk => {
                "two weeks after release, one more ask to the audience that did not act on day \
                 one — the last rung of the runway and the one that catches the people who meant \
                 to come back"
            }
        }
    }

    /// How far this step reaches, and therefore what authority it needs.
    ///
    /// The step kind decides, not the step table: a spec is data a play author
    /// writes, and a play author who could pick the class could route a curator
    /// email through a step the operator only ever approved for their own fans.
    /// Exhaustive, so a new step kind cannot compile until somebody has said
    /// what it costs.
    #[must_use]
    pub const fn action_class(self) -> ActionClass {
        match self {
            // Both asks go to consented fans of this workspace. Free, and
            // irreversible only in the sense every sent message is.
            Self::AnnounceAsk | Self::PostShowAsk => ActionClass::OwnedAudience,
            // Our own listings. Free, reaches nobody outside the workspace, and
            // undone by doing the opposite.
            Self::ListingSweep => ActionClass::FirstPartyReversible,
            // Every rung is a message to a consented fan of this workspace, so
            // the ladder spends the same budget and obeys the same cooldown as
            // the show-timed asks.
            Self::FollowAskFirst | Self::FollowAskSecond | Self::FollowAskFinal => {
                ActionClass::OwnedAudience
            }
            // Quiet is not withdrawn consent. A fan who stopped turning up is
            // still a fan of this workspace who agreed to hear from it.
            Self::DormantRevivalFirst | Self::DormantRevivalFinal => ActionClass::OwnedAudience,
            // Pre-save and listing work are first-party: our own surfaces, our
            // own links, reaching nobody outside the workspace.
            Self::ReleasePresaveLive => ActionClass::FirstPartyReversible,
            // The audience announce and release-day push go to consented fans.
            Self::ReleaseAudienceAnnounce | Self::ReleaseDayPush
                | Self::ReleaseSustainAsk => ActionClass::OwnedAudience,
            // A curator wave is third-party outreach: free, but it contacts
            // people outside the workspace and is therefore approval-gated at
            // the current posture.
            Self::ReleaseCuratorWave => ActionClass::ThirdParty,
        }
    }

    /// Who this step is sent to.
    #[must_use]
    pub const fn audience(self) -> StepAudience {
        match self {
            Self::AnnounceAsk
            | Self::PostShowAsk
            | Self::FollowAskFirst
            | Self::FollowAskSecond
            | Self::FollowAskFinal
            | Self::DormantRevivalFirst
            | Self::DormantRevivalFinal
            | Self::ReleaseAudienceAnnounce
            | Self::ReleaseDayPush
            | Self::ReleaseSustainAsk => StepAudience::Fans,
            // Steps that run once for their anchor: listing sweep is work on
            // the band's own listing, pre-save live is work on the band's own
            // surface, and the curator wave is a trigger that queues a separate
            // outreach wave rather than contacting curators directly from the
            // play step.
            Self::ListingSweep
            | Self::ReleasePresaveLive
            | Self::ReleaseCuratorWave => StepAudience::None,
        }
    }
}

/// Why a step will never be sent. Recorded on the row, so an omission is a fact
/// rather than an absence somebody has to notice.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepSkipReason {
    /// The window closed. For a gated step that means nobody approved it in
    /// time; for any step it means the moment it was written for has passed.
    WindowClosed,
    /// Nobody was left to send it to. Not a failure: a step with an empty
    /// audience has done all it can.
    NoEligibleRecipients,
    /// The play's anchor stopped being a reason to act — the show was cancelled
    /// or unpublished.
    AnchorWithdrawn,
}

impl StepSkipReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WindowClosed => "window_closed",
            Self::NoEligibleRecipients => "no_eligible_recipients",
            Self::AnchorWithdrawn => "anchor_withdrawn",
        }
    }
}

/// One step as the database holds it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlayStepState {
    pub index: u16,
    pub kind: PlayStepKind,
    pub class: ActionClass,
    pub due_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    /// True once the step reached a terminal state — fully delivered, or
    /// skipped. Terminal steps are never revisited.
    pub settled: bool,
    /// Recipients this step has already been emitted for. Bounds the send and
    /// makes a resumed play pick up where it stopped rather than start again.
    pub recipients_emitted: u32,
}

impl PlayStepState {
    #[must_use]
    pub const fn is_open(&self, now: OffsetDateTime) -> bool {
        !self.settled
            && now.unix_timestamp() >= self.due_at.unix_timestamp()
            && now.unix_timestamp() < self.expires_at.unix_timestamp()
    }
}

/// One running play, with the outside facts its next decision needs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlaySnapshot {
    pub play_id: PlayId,
    pub kind: PlayKind,
    pub anchor_at: OffsetDateTime,
    /// False once the show is cancelled or unpublished. Every remaining step is
    /// then skipped rather than sent: the reason to act has gone.
    pub anchor_active: bool,
    pub steps: Vec<PlayStepState>,
    /// Fans eligible for the play's next send: consented, in scope, and not
    /// already reached by this step.
    pub eligible_recipients: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PlayPolicy {
    /// Most recipients one step may reach in total. The growth envelope caps a
    /// single cycle's blast radius; this caps the step's whole audience, so a
    /// wrong segment costs a bounded number of messages rather than a bounded
    /// number per cycle for ever.
    pub max_recipients_per_step: u32,
    /// A play is only worth starting for a show far enough out that its first
    /// step still has a window. Starting one for tomorrow's show creates a play
    /// whose every step is already expired.
    pub minimum_lead_hours: u32,
    /// How the play's effect is read once its last step closes. Nested here
    /// rather than given its own context because it is the same operator
    /// setting: a play and the reading of that play are one decision.
    pub measurement: PlayMeasurementPolicy,
    /// What the measured record is allowed to change about how often and how
    /// widely this kind of play runs.
    pub learning: LearningPolicy,
}

impl Default for PlayPolicy {
    fn default() -> Self {
        Self {
            // Deliberately small. The first thing an operator does with a play
            // that works is raise this, and that is a row update.
            max_recipients_per_step: 150,
            // The announce step is due fourteen days out with a seven-day
            // window, so a show under seven days away can never use it.
            minimum_lead_hours: 7 * 24,
            measurement: PlayMeasurementPolicy::default(),
            learning: LearningPolicy::default(),
        }
    }
}

/// What the play wants to do next.
///
/// One step at a time on purpose: the caller turns this into a single durable
/// action, and a decision that fanned out would be a decision no idempotency key
/// could describe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PlayDecision {
    /// Send this step to one more recipient.
    RunStep {
        index: u16,
        kind: PlayStepKind,
        class: ActionClass,
        confidence: Confidence,
    },
    /// Settle this step without sending it, for a stated reason.
    SkipStep {
        index: u16,
        kind: PlayStepKind,
        reason: StepSkipReason,
    },
    /// Every step is settled. Nothing further happens under this play.
    Complete,
    /// Nothing due. The play stays running.
    Hold(PlayHold),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayHold {
    /// No step is inside its window yet.
    NoStepDue,
    /// The open step has reached its recipient ceiling and is waiting for its
    /// window to close before it settles.
    StepSaturated,
}

/// Decides the play's next move.
///
/// Order is the whole rule. Withdrawal beats everything, because a cancelled
/// show must not be promoted for any reason. Expiry beats sending, so a step
/// cannot be delivered outside the moment it was written for. And a saturated
/// step holds rather than skipping, because "we sent all we were allowed" is not
/// the same fact as "we sent none".
#[must_use]
pub fn evaluate_play(
    snapshot: &PlaySnapshot,
    policy: PlayPolicy,
    now: OffsetDateTime,
) -> PlayDecision {
    let Some(step) = snapshot.steps.iter().find(|step| !step.settled) else {
        return PlayDecision::Complete;
    };

    if !snapshot.anchor_active {
        return PlayDecision::SkipStep {
            index: step.index,
            kind: step.kind,
            reason: StepSkipReason::AnchorWithdrawn,
        };
    }

    // Past its window the step is settled as skipped whatever else is true. A
    // gated step nobody approved in time lands here, which is exactly the
    // intent: it is recorded as skipped, not left pending for ever and not sent
    // three weeks late.
    if now.unix_timestamp() >= step.expires_at.unix_timestamp() {
        return PlayDecision::SkipStep {
            index: step.index,
            kind: step.kind,
            reason: StepSkipReason::WindowClosed,
        };
    }

    if now.unix_timestamp() < step.due_at.unix_timestamp() {
        return PlayDecision::Hold(PlayHold::NoStepDue);
    }

    // A step with no audience runs once for its anchor. Measuring it against
    // the recipient ceiling, or against an audience it does not have, would
    // settle the one kind of work that needs nobody's consent as
    // `no_eligible_recipients`.
    let ceiling = match step.kind.audience() {
        StepAudience::Fans => policy.max_recipients_per_step,
        StepAudience::None => 1,
    };
    if step.recipients_emitted >= ceiling {
        return PlayDecision::Hold(PlayHold::StepSaturated);
    }

    // An audience of nobody settles the step now rather than at expiry. Holding
    // a step open against an empty segment reads as work in progress for as long
    // as the window lasts, and it is not.
    if matches!(step.kind.audience(), StepAudience::Fans) && snapshot.eligible_recipients == 0 {
        return PlayDecision::SkipStep {
            index: step.index,
            kind: step.kind,
            reason: StepSkipReason::NoEligibleRecipients,
        };
    }

    PlayDecision::RunStep {
        index: step.index,
        kind: step.kind,
        class: step.class,
        confidence: step_confidence(step, snapshot.eligible_recipients, now),
    }
}

/// Whether a play is worth starting for this anchor at all.
///
/// Separate from [`evaluate_play`] because there is no play yet: this reads an
/// anchor, not a state machine.
#[must_use]
pub fn play_is_worth_starting(
    kind: PlayKind,
    anchor_active: bool,
    hours_until_anchor: i64,
    policy: PlayPolicy,
) -> bool {
    if !anchor_active {
        return false;
    }
    // The operator's minimum lead exists to stop a step that runs *before* its
    // anchor being scheduled into a moment that has already gone. A play whose
    // every step follows its anchor has no such moment to miss, and holding one
    // to the same floor would refuse every play anchored on something that has
    // already happened — which is every fan-anchored play there will ever be.
    let anticipates_anchor = kind.steps().iter().any(|spec| spec.offset_hours < 0);
    if anticipates_anchor && hours_until_anchor < policy.minimum_lead_hours as i64 {
        return false;
    }
    // And at least one of this play's own steps must still have a window. The
    // operator's minimum lead is a floor for every play; each play then has its
    // own earliest useful moment, and starting one whose every window has
    // already closed produces a campaign that exists only to record its own
    // skips. The track-us ask asks fourteen days out and the listing sweep
    // twenty-one; one number for both would be wrong for one of them.
    kind.steps().iter().any(|spec| {
        let closes_at_hours = i64::from(spec.offset_hours) + i64::from(spec.window_hours);
        hours_until_anchor + closes_at_hours > 0
    })
}

/// Derives one step's schedule from the anchor. The only place offsets are
/// applied, so a step's due time and its expiry can never disagree.
#[must_use]
pub fn step_schedule(
    spec: PlayStepSpec,
    anchor_at: OffsetDateTime,
) -> (OffsetDateTime, OffsetDateTime) {
    let due_at = anchor_at + Duration::hours(i64::from(spec.offset_hours));
    let expires_at = due_at + Duration::hours(i64::from(spec.window_hours));
    (due_at, expires_at)
}

/// How sure the play is that this send is the right one now.
///
/// Highest in the middle of the window with an audience to reach, and falling as
/// the window runs out — a step sent in its last hour is a step that nearly
/// became a skip, and the record should say so.
fn step_confidence(step: &PlayStepState, eligible: u32, now: OffsetDateTime) -> Confidence {
    let window = step.expires_at.unix_timestamp() - step.due_at.unix_timestamp();
    if window <= 0 {
        return Confidence::MIN;
    }
    let elapsed = (now.unix_timestamp() - step.due_at.unix_timestamp()).max(0);
    // Linear from full at the start of the window to a third at its close. Never
    // zero: a step still inside its window is still the right thing to do.
    let remaining = (window - elapsed).max(0);
    let freshness = 3_400 + (remaining as u64 * 6_600 / window as u64);
    // A step with one recipient left is a weaker case than one with a hundred,
    // but it is not a wrong one, so audience size only trims the top.
    let reach = if eligible >= 25 { 10_000 } else { 8_000 };
    let basis_points = freshness * reach / 10_000;
    Confidence::saturating_from_basis_points(u16::try_from(basis_points).unwrap_or(10_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn step(index: u16, spec: PlayStepSpec) -> PlayStepState {
        let (due_at, expires_at) = step_schedule(spec, anchor());
        PlayStepState {
            index,
            kind: spec.kind,
            class: spec.class,
            due_at,
            expires_at,
            settled: false,
            recipients_emitted: 0,
        }
    }

    fn running() -> PlaySnapshot {
        PlaySnapshot {
            play_id: PlayId::new(),
            kind: PlayKind::TrackUsAsk,
            anchor_at: anchor(),
            anchor_active: true,
            steps: PlayKind::TrackUsAsk
                .steps()
                .iter()
                .enumerate()
                .map(|(index, spec)| step(u16::try_from(index).expect("small index"), *spec))
                .collect(),
            eligible_recipients: 60,
        }
    }

    fn skip(decision: PlayDecision) -> StepSkipReason {
        match decision {
            PlayDecision::SkipStep { reason, .. } => reason,
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn an_open_step_with_an_audience_sends() {
        let snapshot = running();
        let due = snapshot.steps[0].due_at + Duration::hours(1);
        assert!(matches!(
            evaluate_play(&snapshot, PlayPolicy::default(), due),
            PlayDecision::RunStep {
                index: 0,
                kind: PlayStepKind::AnnounceAsk,
                ..
            }
        ));
    }

    #[test]
    fn a_step_before_its_window_holds_rather_than_sending_early() {
        let snapshot = running();
        let early = snapshot.steps[0].due_at - Duration::hours(1);
        assert_eq!(
            evaluate_play(&snapshot, PlayPolicy::default(), early),
            PlayDecision::Hold(PlayHold::NoStepDue)
        );
    }

    #[test]
    fn a_step_past_its_window_is_skipped_and_never_sent_late() {
        // The property the whole design turns on: an unapproved or unreached
        // step becomes a recorded skip, not a message three weeks after the
        // moment it was written for.
        let snapshot = running();
        let late = snapshot.steps[0].expires_at + Duration::hours(1);
        assert_eq!(
            skip(evaluate_play(&snapshot, PlayPolicy::default(), late)),
            StepSkipReason::WindowClosed
        );
    }

    #[test]
    fn a_skipped_step_does_not_block_the_one_behind_it() {
        // A gated step nobody approved must not stall the play. Settling step
        // zero leaves step one to run on its own schedule.
        let mut snapshot = running();
        snapshot.steps[0].settled = true;
        let due = snapshot.steps[1].due_at + Duration::hours(1);
        assert!(matches!(
            evaluate_play(&snapshot, PlayPolicy::default(), due),
            PlayDecision::RunStep {
                index: 1,
                kind: PlayStepKind::PostShowAsk,
                ..
            }
        ));
    }

    #[test]
    fn a_withdrawn_anchor_skips_every_remaining_step() {
        // A cancelled show must not be promoted, and the reason recorded has to
        // be the withdrawal rather than a window that had not even closed.
        let mut snapshot = running();
        snapshot.anchor_active = false;
        let due = snapshot.steps[0].due_at + Duration::hours(1);
        assert_eq!(
            skip(evaluate_play(&snapshot, PlayPolicy::default(), due)),
            StepSkipReason::AnchorWithdrawn
        );
    }

    #[test]
    fn an_empty_audience_settles_the_step_instead_of_holding_it_open() {
        let mut snapshot = running();
        snapshot.eligible_recipients = 0;
        let due = snapshot.steps[0].due_at + Duration::hours(1);
        assert_eq!(
            skip(evaluate_play(&snapshot, PlayPolicy::default(), due)),
            StepSkipReason::NoEligibleRecipients
        );
    }

    #[test]
    fn a_saturated_step_holds_rather_than_reporting_it_reached_nobody() {
        // "We sent all we were allowed" and "we sent none" are different facts
        // and must not collapse into the same recorded outcome.
        let mut snapshot = running();
        snapshot.steps[0].recipients_emitted = PlayPolicy::default().max_recipients_per_step;
        let due = snapshot.steps[0].due_at + Duration::hours(1);
        assert_eq!(
            evaluate_play(&snapshot, PlayPolicy::default(), due),
            PlayDecision::Hold(PlayHold::StepSaturated)
        );
    }

    #[test]
    fn every_step_settled_completes_the_play() {
        let mut snapshot = running();
        for step in &mut snapshot.steps {
            step.settled = true;
        }
        assert_eq!(
            evaluate_play(&snapshot, PlayPolicy::default(), anchor()),
            PlayDecision::Complete
        );
    }

    #[test]
    fn a_show_too_close_to_run_the_first_step_is_never_started() {
        let policy = PlayPolicy::default();
        let kind = PlayKind::TrackUsAsk;
        assert!(!play_is_worth_starting(kind, true, 24, policy));
        assert!(play_is_worth_starting(kind, true, 30 * 24, policy));
        // And a cancelled show is never a reason to start one.
        assert!(!play_is_worth_starting(kind, false, 30 * 24, policy));
    }

    #[test]
    fn a_play_whose_every_window_has_closed_is_never_started() {
        // The sweep asks twenty-one days out with a fortnight's window, so it
        // closes seven days before the show. An operator who lowers the floor
        // to two days would otherwise start a sweep for a show three days away:
        // a campaign that exists only to record its own skip.
        let policy = PlayPolicy {
            minimum_lead_hours: 48,
            ..PlayPolicy::default()
        };
        assert!(!play_is_worth_starting(
            PlayKind::ListingCompletenessSweep,
            true,
            3 * 24,
            policy
        ));
        assert!(play_is_worth_starting(
            PlayKind::ListingCompletenessSweep,
            true,
            10 * 24,
            policy
        ));
        // The track-us ask still has its post-show step at that distance, which
        // is why one lead time for every play would be wrong for one of them.
        assert!(play_is_worth_starting(
            PlayKind::TrackUsAsk,
            true,
            3 * 24,
            policy
        ));
    }

    #[test]
    fn confidence_falls_as_the_window_runs_out_but_never_to_nothing() {
        let snapshot = running();
        let early = evaluate_play(
            &snapshot,
            PlayPolicy::default(),
            snapshot.steps[0].due_at + Duration::hours(1),
        );
        let late = evaluate_play(
            &snapshot,
            PlayPolicy::default(),
            snapshot.steps[0].expires_at - Duration::hours(1),
        );
        let (
            PlayDecision::RunStep { confidence: a, .. },
            PlayDecision::RunStep { confidence: b, .. },
        ) = (early, late)
        else {
            panic!("both are inside the window");
        };
        assert!(a > b, "a step nearly out of time is a weaker case");
        assert!(
            b > Confidence::MIN,
            "a step still inside its window is still worth doing"
        );
    }

    #[test]
    fn a_step_with_no_audience_runs_once_and_is_never_skipped_for_want_of_fans() {
        // The listing sweep is work on the band's own surfaces. Running it
        // through an audience check would settle the one kind of work that
        // needs nobody's consent as "no eligible recipients".
        let spec = PlayKind::ListingCompletenessSweep
            .steps()
            .first()
            .copied()
            .expect("the sweep has a step");
        let (due_at, expires_at) = step_schedule(spec, anchor());
        let mut snapshot = PlaySnapshot {
            play_id: PlayId::new(),
            kind: PlayKind::ListingCompletenessSweep,
            anchor_at: anchor(),
            anchor_active: true,
            steps: vec![PlayStepState {
                index: 0,
                kind: spec.kind,
                class: spec.class,
                due_at,
                expires_at,
                settled: false,
                recipients_emitted: 0,
            }],
            eligible_recipients: 0,
        };
        let due = due_at + Duration::hours(1);
        assert!(matches!(
            evaluate_play(&snapshot, PlayPolicy::default(), due),
            PlayDecision::RunStep {
                kind: PlayStepKind::ListingSweep,
                class: ActionClass::FirstPartyReversible,
                ..
            }
        ));
        // And once is enough: a second sweep of the same listing is noise.
        if let Some(step) = snapshot.steps.first_mut() {
            step.recipients_emitted = 1;
        }
        assert_eq!(
            evaluate_play(&snapshot, PlayPolicy::default(), due),
            PlayDecision::Hold(PlayHold::StepSaturated)
        );
    }

    #[test]
    fn a_revival_stops_after_two_and_never_precedes_its_anchor() {
        // Somebody who ignored the band for a year and then ignored two
        // reminders has answered. A third would be the campaign talking to
        // itself.
        let steps = PlayKind::DormantRevival.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(PlayKind::DormantRevival.anchor_kind(), PlayAnchorKind::Fan);
        for spec in steps {
            assert!(spec.offset_hours >= 0);
            assert_eq!(spec.class, ActionClass::OwnedAudience);
            assert_eq!(spec.kind.audience(), StepAudience::Fans);
        }
        assert!(play_is_worth_starting(
            PlayKind::DormantRevival,
            true,
            0,
            PlayPolicy::default()
        ));
    }

    #[test]
    fn the_two_fan_anchored_plays_are_judged_by_different_numbers() {
        // A revival is measured on fans becoming active again, which is the
        // thing it attempts. Borrowing the ladder's tracker series would read a
        // number none of its messages mentions.
        assert_eq!(
            PlayKind::DormantRevival.success_metric(),
            ("signal", "activated_fans_30d")
        );
        assert_ne!(
            PlayKind::DormantRevival.success_metric(),
            PlayKind::FollowAskLadder.success_metric()
        );
    }

    #[test]
    fn every_play_names_a_hypothesis_a_metric_and_at_least_one_step() {
        for kind in PlayKind::all() {
            assert!(!kind.hypothesis().is_empty());
            assert!(!kind.steps().is_empty());
            let (platform, metric_key) = kind.success_metric();
            assert!(!platform.is_empty() && !metric_key.is_empty());
            assert_eq!(PlayKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                PlayAnchorKind::parse(kind.anchor_kind().as_str()),
                Some(kind.anchor_kind())
            );
        }
    }

    #[test]
    fn every_step_kind_names_its_own_template_and_survives_a_round_trip() {
        // A rung that reused another's template would be a message whose result
        // cannot be told apart from the message before it.
        let mut templates = Vec::new();
        for kind in PlayKind::all() {
            for spec in kind.steps() {
                assert_eq!(PlayStepKind::parse(spec.kind.as_str()), Some(spec.kind));
                assert_eq!(spec.class, spec.kind.action_class());
                templates.push(spec.kind.template_key());
            }
        }
        let unique: std::collections::BTreeSet<_> = templates.iter().collect();
        assert_eq!(unique.len(), templates.len(), "one template per step kind");
    }

    #[test]
    fn a_fan_anchored_play_starts_even_though_its_anchor_is_now() {
        // The whole ladder follows its anchor, so there is no lead time to
        // wait for. Applying the show-shaped minimum here would refuse every
        // fan-anchored play the system will ever have.
        let policy = PlayPolicy::default();
        assert_eq!(PlayKind::FollowAskLadder.anchor_kind(), PlayAnchorKind::Fan);
        assert!(play_is_worth_starting(
            PlayKind::FollowAskLadder,
            true,
            0,
            policy
        ));
        // And a fan who is no longer a reason to act is still never started.
        assert!(!play_is_worth_starting(
            PlayKind::FollowAskLadder,
            false,
            0,
            policy
        ));
    }

    #[test]
    fn the_ladder_spaces_its_rungs_by_months_and_never_overlaps_them() {
        // Three sends of one ask in a month is nagging. The gaps are the play.
        let steps = PlayKind::FollowAskLadder.steps();
        assert_eq!(steps.len(), 3);
        let mut previous_close = i64::MIN;
        for spec in steps {
            assert!(
                spec.offset_hours >= 0,
                "a rung before the anchor would be scheduled into the past"
            );
            let (due, expires) = step_schedule(*spec, anchor());
            assert!(
                due.unix_timestamp() > previous_close,
                "a rung must not open before the one above it has closed"
            );
            previous_close = expires.unix_timestamp();
            assert_eq!(spec.class, ActionClass::OwnedAudience);
            assert_eq!(spec.kind.audience(), StepAudience::Fans);
        }
    }

    #[test]
    fn a_ladder_rung_reaches_exactly_one_fan_and_is_never_re_sent() {
        let spec = PlayKind::FollowAskLadder.steps()[0];
        let (due_at, expires_at) = step_schedule(spec, anchor());
        let mut snapshot = PlaySnapshot {
            play_id: PlayId::new(),
            kind: PlayKind::FollowAskLadder,
            anchor_at: anchor(),
            anchor_active: true,
            steps: vec![PlayStepState {
                index: 0,
                kind: spec.kind,
                class: spec.class,
                due_at,
                expires_at,
                settled: false,
                recipients_emitted: 0,
            }],
            eligible_recipients: 1,
        };
        let due = due_at + Duration::hours(1);
        assert!(matches!(
            evaluate_play(&snapshot, PlayPolicy::default(), due),
            PlayDecision::RunStep {
                kind: PlayStepKind::FollowAskFirst,
                class: ActionClass::OwnedAudience,
                ..
            }
        ));
        // The anchor fan is the only recipient, so once they are committed to
        // the rung has nobody left and settles rather than holding open.
        snapshot.eligible_recipients = 0;
        if let Some(step) = snapshot.steps.first_mut() {
            step.recipients_emitted = 1;
        }
        assert_eq!(
            skip(evaluate_play(&snapshot, PlayPolicy::default(), due)),
            StepSkipReason::NoEligibleRecipients
        );
    }

    #[test]
    fn the_announce_step_lands_before_the_show_and_the_thanks_after_it() {
        let steps = PlayKind::TrackUsAsk.steps();
        let (announce_due, announce_expiry) = step_schedule(steps[0], anchor());
        let (post_due, _) = step_schedule(steps[1], anchor());
        assert!(
            announce_due < anchor(),
            "the announce ask precedes the show"
        );
        assert!(
            announce_expiry < anchor(),
            "and closes before it, so it is never sent as a reminder about a show already played"
        );
        assert!(post_due > anchor(), "the thanks follows the show");
    }
}
