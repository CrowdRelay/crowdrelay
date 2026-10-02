//! FAN SCOUT's next-best-action evaluator for one prospect (`fan_scout` holds the
//! prospect and evidence primitives; this module decides what to do about one).
//!
//! `fan_prospect` says who a person is to the system; this module says what, if
//! anything, the system should do about them next. The rule it serves is the
//! plan's central one: **a public signal is evidence, never permission.** Most
//! prospects get `Observe`. A person who has asked the band a question in
//! public gets an answer in the same thread. A person who has been answered
//! and keeps talking gets one invitation, in the same thread. Nobody is
//! messaged privately, nobody is emailed, and nobody who said no is touched.
//!
//! This is the slice-1 action set only: `Observe`, `EngageInContext`,
//! `InviteToFanbase`, `Hold`, `DoNotContact`. `ActivateFan`, `AskForReferral`,
//! the Latarnik actions and `Reward` belong to people who are already fans and
//! arrive with slice 2.
//!
//! **The decision is deterministic and typed.** Language models extract
//! evidence (the observation rows) and write the words; they never decide who
//! is addressed. A score never triggers a send: the dimensions below are
//! reported beside the action so a person can see why, and the action is a
//! function of facts, not of a number crossing a line.
//!
//! What this module does not do: it does not send, draft, rate-limit or check
//! authority. Those are the execution envelope's job (per-channel caps, the
//! contact governor, the strategic review). `evaluate` answers "what would be
//! right", and the envelope answers "may we, now".

use serde::Serialize;
use time::{Duration, OffsetDateTime};

use crate::fan_prospect::{ObservationKind, ProspectStatus};

/// The slice-1 action set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextFanAction {
    /// Keep watching; do nothing the person can see.
    Observe,
    /// Answer or acknowledge in the thread the person already spoke in.
    EngageInContext,
    /// One invitation to join, in the same thread, with a tracked link.
    InviteToFanbase,
    /// Wanted, but something named in the reason stands in the way.
    Hold,
    /// The person said no (or is suppressed in any role). Terminal.
    DoNotContact,
}

/// Where the action would happen. Slice 1 has exactly one lawful medium for a
/// consumer prospect: the thread they are already in.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Medium {
    /// A reply in the existing public thread on the same platform.
    SameThread,
}

/// Why `Hold`, or the reason `Observe`/`DoNotContact` was chosen. Each is a
/// stable identifier shown to the operator verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The person said no, or is suppressed in some role.
    DeclinedOrSuppressed,
    /// Already a fan: this prospect's work is done.
    AlreadyAFan,
    /// The band spoke to them recently; one voice at a time.
    CooldownActive,
    /// Wants to engage but there is no thread to answer in. A consumer is never
    /// contacted outside the context they spoke in.
    NoThreadToAnswerIn,
    /// A single bare comment is not enough to act on.
    NotEnoughEvidence,
    /// They asked the band something, in public.
    AskedInPublic,
    /// They were answered and have kept talking.
    EngagedAfterAnswer,
    /// One invitation per person: already made.
    InviteAlreadyMade,
}

/// How warm the relationship is, from what has actually happened between the
/// band and this person. `Owned` (a fan) is not a prospect state.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Warmth {
    Cold,
    Observed,
    Engaged,
    Warm,
}

/// One observation as the evaluator sees it.
#[derive(Clone, Copy, Debug)]
pub struct ObservationFact {
    pub kind: ObservationKind,
    pub confidence_basis_points: u16,
    pub observed_at: OffsetDateTime,
}

/// Everything the evaluator reads about one prospect. Built from rows by the
/// loader; contains no free text and no identity — the decision cannot depend
/// on who the person is, only on what they did.
#[derive(Clone, Debug)]
pub struct ProspectSnapshot {
    pub status: ProspectStatus,
    pub observations: Vec<ObservationFact>,
    /// Whether the person is, or has been made, a first-party fan.
    pub is_fan: bool,
    /// Whether a suppression, opt-out or do-not-contact exists for this person
    /// in any other role (fan, contact, beacon). Terminal across roles.
    pub suppressed_in_any_role: bool,
    /// The latest observation carries a checkable thread the band can answer in
    /// on a platform it can reply on.
    pub has_thread_to_answer_in: bool,
    /// When the band last spoke to this person, if ever.
    pub last_engaged_at: Option<OffsetDateTime>,
    /// How many times the band has answered them in the last 30 days.
    pub engagements_30d: u32,
    /// Invitations ever made to this person.
    pub invites_ever: u32,
    /// The person wrote again after the band's last answer.
    pub replied_after_last_engagement: bool,
}

/// The evaluator's policy. Only what decides; caps and authority live in the
/// execution envelope.
#[derive(Clone, Copy, Debug)]
pub struct ScoutPolicy {
    /// Minimum gap between two things the band says to one person.
    pub engagement_cooldown: Duration,
    /// Observations older than this are history, not warmth.
    pub evidence_window: Duration,
}

impl Default for ScoutPolicy {
    fn default() -> Self {
        Self {
            engagement_cooldown: Duration::days(7),
            evidence_window: Duration::days(30),
        }
    }
}

/// The typed dimensions behind a decision. Reported, never thresholded into an
/// action. Only the dimensions computable from slice-1 evidence are here; the
/// plan's locality, activation-readiness, referral-readiness and
/// network-leverage have no evidence source yet and are absent rather than
/// zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Dimensions {
    /// How strongly the person's own words point at this band's music.
    pub affinity_basis_points: u16,
    /// A current, specific ask: a question, a request for the song or the show.
    pub intent_basis_points: u16,
    pub relationship_warmth: Warmth,
    /// How solid the basis is: distinct dated signals, not one repeated.
    pub evidence_quality_basis_points: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Decision {
    pub action: NextFanAction,
    /// `None` unless the action is something the person can see.
    pub medium: Option<Medium>,
    pub reason: Reason,
    pub dimensions: Dimensions,
}

fn is_intent(kind: ObservationKind) -> bool {
    matches!(
        kind,
        ObservationKind::AskedAboutShow | ObservationKind::AskedForMusic
    )
}

fn recent<'a>(
    snapshot: &'a ProspectSnapshot,
    policy: &ScoutPolicy,
    now: OffsetDateTime,
) -> impl Iterator<Item = &'a ObservationFact> {
    let floor = now - policy.evidence_window;
    snapshot
        .observations
        .iter()
        .filter(move |o| o.observed_at >= floor && o.observed_at <= now)
}

fn warmth(snapshot: &ProspectSnapshot, policy: &ScoutPolicy, now: OffsetDateTime) -> Warmth {
    if snapshot.engagements_30d > 0 && snapshot.replied_after_last_engagement {
        return Warmth::Warm;
    }
    let readings: Vec<_> = recent(snapshot, policy, now).collect();
    if readings.is_empty() {
        return Warmth::Cold;
    }
    // Distinct days: five comments in one minute are one signal, not five.
    let mut days: Vec<_> = readings.iter().map(|o| o.observed_at.date()).collect();
    days.sort_unstable();
    days.dedup();
    if days.len() >= 2 || readings.iter().any(|o| is_intent(o.kind)) {
        Warmth::Engaged
    } else {
        Warmth::Observed
    }
}

fn dimensions(
    snapshot: &ProspectSnapshot,
    policy: &ScoutPolicy,
    now: OffsetDateTime,
) -> Dimensions {
    let readings: Vec<_> = recent(snapshot, policy, now).collect();
    let strongest = |pick: fn(ObservationKind) -> bool| {
        readings
            .iter()
            .filter(|o| pick(o.kind))
            .map(|o| o.confidence_basis_points)
            .max()
            .unwrap_or(0)
    };
    let mut days: Vec<_> = readings.iter().map(|o| o.observed_at.date()).collect();
    days.sort_unstable();
    days.dedup();
    Dimensions {
        affinity_basis_points: strongest(|_| true),
        intent_basis_points: strongest(is_intent),
        relationship_warmth: warmth(snapshot, policy, now),
        // Three distinct days of evidence is solid; each day is a third.
        evidence_quality_basis_points: u16::try_from((days.len().min(3) * 10_000) / 3)
            .unwrap_or(u16::MAX),
    }
}

/// Decides the next action for one prospect. Order matters and is the
/// priority of the rules: a no outranks everything, a cooldown outranks wanting
/// to speak, and wanting to speak requires a thread to speak in.
#[must_use]
pub fn evaluate(
    snapshot: &ProspectSnapshot,
    policy: &ScoutPolicy,
    now: OffsetDateTime,
) -> Decision {
    let dims = dimensions(snapshot, policy, now);
    let decide = |action, medium, reason| Decision {
        action,
        medium,
        reason,
        dimensions: dims,
    };

    if snapshot.status.forbids_contact() || snapshot.suppressed_in_any_role {
        return decide(
            NextFanAction::DoNotContact,
            None,
            Reason::DeclinedOrSuppressed,
        );
    }
    if snapshot.is_fan || snapshot.status == ProspectStatus::Converted {
        return decide(NextFanAction::Hold, None, Reason::AlreadyAFan);
    }
    let wants = dims.intent_basis_points > 0
        || matches!(dims.relationship_warmth, Warmth::Engaged | Warmth::Warm);
    if !wants {
        return decide(NextFanAction::Observe, None, Reason::NotEnoughEvidence);
    }
    if snapshot
        .last_engaged_at
        .is_some_and(|at| now - at < policy.engagement_cooldown)
    {
        return decide(NextFanAction::Hold, None, Reason::CooldownActive);
    }
    if !snapshot.has_thread_to_answer_in {
        return decide(NextFanAction::Hold, None, Reason::NoThreadToAnswerIn);
    }
    // They have been answered and have written again: the one invitation, once.
    if snapshot.engagements_30d > 0 && snapshot.replied_after_last_engagement {
        return if snapshot.invites_ever == 0 {
            decide(
                NextFanAction::InviteToFanbase,
                Some(Medium::SameThread),
                Reason::EngagedAfterAnswer,
            )
        } else {
            decide(NextFanAction::Hold, None, Reason::InviteAlreadyMade)
        };
    }
    // Asked, in public, and not yet answered.
    if dims.intent_basis_points > 0 && snapshot.engagements_30d == 0 {
        return decide(
            NextFanAction::EngageInContext,
            Some(Medium::SameThread),
            Reason::AskedInPublic,
        );
    }
    decide(NextFanAction::Observe, None, Reason::NotEnoughEvidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

    fn fact(kind: ObservationKind, days_ago: i64) -> ObservationFact {
        ObservationFact {
            kind,
            confidence_basis_points: 3_000,
            observed_at: NOW - Duration::days(days_ago),
        }
    }

    fn snapshot(observations: Vec<ObservationFact>) -> ProspectSnapshot {
        ProspectSnapshot {
            status: ProspectStatus::Observed,
            observations,
            is_fan: false,
            suppressed_in_any_role: false,
            has_thread_to_answer_in: true,
            last_engaged_at: None,
            engagements_30d: 0,
            invites_ever: 0,
            replied_after_last_engagement: false,
        }
    }

    fn act(s: &ProspectSnapshot) -> (NextFanAction, Reason) {
        let d = evaluate(s, &ScoutPolicy::default(), NOW);
        (d.action, d.reason)
    }

    #[test]
    fn a_single_bare_comment_is_observed_not_acted_on() {
        let s = snapshot(vec![fact(ObservationKind::ActiveUnderOurPost, 1)]);
        assert_eq!(act(&s), (NextFanAction::Observe, Reason::NotEnoughEvidence));
        assert_eq!(evaluate(&s, &ScoutPolicy::default(), NOW).medium, None);
        assert_eq!(
            evaluate(&s, &ScoutPolicy::default(), NOW)
                .dimensions
                .relationship_warmth,
            Warmth::Observed
        );
    }

    #[test]
    fn a_question_about_a_show_is_answered_in_the_thread_it_was_asked_in() {
        let s = snapshot(vec![fact(ObservationKind::AskedAboutShow, 0)]);
        let d = evaluate(&s, &ScoutPolicy::default(), NOW);
        assert_eq!(
            (d.action, d.medium, d.reason),
            (
                NextFanAction::EngageInContext,
                Some(Medium::SameThread),
                Reason::AskedInPublic
            )
        );
        assert!(d.dimensions.intent_basis_points > 0);
    }

    #[test]
    fn nothing_is_said_without_a_thread_to_say_it_in() {
        let mut s = snapshot(vec![fact(ObservationKind::AskedAboutShow, 0)]);
        s.has_thread_to_answer_in = false;
        assert_eq!(act(&s), (NextFanAction::Hold, Reason::NoThreadToAnswerIn));
    }

    #[test]
    fn one_voice_at_a_time() {
        let mut s = snapshot(vec![fact(ObservationKind::AskedAboutShow, 0)]);
        s.last_engaged_at = Some(NOW - Duration::days(2));
        assert_eq!(act(&s), (NextFanAction::Hold, Reason::CooldownActive));
        s.last_engaged_at = Some(NOW - Duration::days(8));
        s.engagements_30d = 1;
        // Past the cooldown, but they have not written since: nothing new to answer.
        assert_eq!(act(&s), (NextFanAction::Observe, Reason::NotEnoughEvidence));
    }

    #[test]
    fn a_person_who_was_answered_and_wrote_again_is_invited_once() {
        let mut s = snapshot(vec![
            fact(ObservationKind::AskedAboutShow, 9),
            fact(ObservationKind::Replied, 1),
        ]);
        s.last_engaged_at = Some(NOW - Duration::days(8));
        s.engagements_30d = 1;
        s.replied_after_last_engagement = true;
        let d = evaluate(&s, &ScoutPolicy::default(), NOW);
        assert_eq!(
            (
                d.action,
                d.medium,
                d.reason,
                d.dimensions.relationship_warmth
            ),
            (
                NextFanAction::InviteToFanbase,
                Some(Medium::SameThread),
                Reason::EngagedAfterAnswer,
                Warmth::Warm
            )
        );
        s.invites_ever = 1;
        assert_eq!(act(&s), (NextFanAction::Hold, Reason::InviteAlreadyMade));
    }

    #[test]
    fn a_no_outranks_everything_even_a_direct_question() {
        for status in [ProspectStatus::Refused, ProspectStatus::Suppressed] {
            let mut s = snapshot(vec![fact(ObservationKind::AskedAboutShow, 0)]);
            s.status = status;
            assert_eq!(
                act(&s),
                (NextFanAction::DoNotContact, Reason::DeclinedOrSuppressed)
            );
        }
        let mut s = snapshot(vec![fact(ObservationKind::AskedAboutShow, 0)]);
        s.suppressed_in_any_role = true;
        assert_eq!(
            act(&s),
            (NextFanAction::DoNotContact, Reason::DeclinedOrSuppressed)
        );
    }

    #[test]
    fn a_fan_is_not_a_prospect_to_work() {
        let mut s = snapshot(vec![fact(ObservationKind::AskedAboutShow, 0)]);
        s.is_fan = true;
        assert_eq!(act(&s), (NextFanAction::Hold, Reason::AlreadyAFan));
        let mut s = snapshot(vec![]);
        s.status = ProspectStatus::Converted;
        assert_eq!(act(&s), (NextFanAction::Hold, Reason::AlreadyAFan));
    }

    #[test]
    fn five_comments_in_one_minute_are_one_signal() {
        let burst: Vec<_> = (0..5)
            .map(|_| fact(ObservationKind::ActiveUnderOurPost, 0))
            .collect();
        let d = evaluate(&snapshot(burst), &ScoutPolicy::default(), NOW);
        assert_eq!(d.dimensions.relationship_warmth, Warmth::Observed);
        assert_eq!(d.dimensions.evidence_quality_basis_points, 3_333);
        assert_eq!(d.action, NextFanAction::Observe);
        // Two different days is the start of a relationship.
        let spread = vec![
            fact(ObservationKind::ActiveUnderOurPost, 0),
            fact(ObservationKind::ActiveUnderOurPost, 3),
        ];
        let d = evaluate(&snapshot(spread), &ScoutPolicy::default(), NOW);
        assert_eq!(d.dimensions.relationship_warmth, Warmth::Engaged);
        assert_eq!(d.dimensions.evidence_quality_basis_points, 6_666);
    }

    #[test]
    fn evidence_outside_the_window_is_history_not_warmth() {
        let s = snapshot(vec![fact(ObservationKind::AskedAboutShow, 45)]);
        let d = evaluate(&s, &ScoutPolicy::default(), NOW);
        assert_eq!(d.dimensions.relationship_warmth, Warmth::Cold);
        assert_eq!(d.dimensions.intent_basis_points, 0);
        assert_eq!(d.action, NextFanAction::Observe);
    }

    #[test]
    fn the_decision_depends_on_what_a_person_did_never_on_who_they_are() {
        // The snapshot has no identity field to vary; this pins that by
        // construction — two snapshots built from the same facts decide alike.
        let a = snapshot(vec![fact(ObservationKind::AskedForMusic, 0)]);
        let b = snapshot(vec![fact(ObservationKind::AskedForMusic, 0)]);
        assert_eq!(
            evaluate(&a, &ScoutPolicy::default(), NOW),
            evaluate(&b, &ScoutPolicy::default(), NOW)
        );
    }
}
