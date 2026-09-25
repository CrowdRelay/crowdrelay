//! Who still owes an answer — the unanswered-reply evaluator.
//!
//! `evaluate_outreach` holds `AlreadyReplied` on purpose: a person who
//! answered must never be re-pitched. What it cannot express is the
//! opposite duty — that a reply nobody answered is work, not a stop. This
//! evaluator is that duty: one snapshot per inbound reply still waiting on
//! an answer, decided the same deterministic way as every other lane.
//!
//! The bar is deliberately narrower than a pitch's. `verified`,
//! `accepts_outreach`, relevance and cooldowns govern whether we may ask a
//! stranger for something; a reply is an answer to somebody who already
//! wrote to us, so the only questions are whether the conversation is still
//! open and whether the last thing they recorded forbids another word.
//! `do_not_contact` holds exactly as it does for a pitch — the line does
//! not move because the email is a reply.
//!
//! What this evaluator does not decide is the letter. The imported cohort
//! carries the sheet's verdict rather than the reply's text, so the action
//! the decision produces is a scaffold for the operator to edit — pinned to
//! approval for exactly that reason by the caller, not by confidence.

use serde::Serialize;
use time::OffsetDateTime;

use crate::{
    OutreachTargetId,
    autonomy::Confidence,
    outreach::{OutreachReplyDisposition, OutreachTargetKind},
};

/// One inbound reply with no outbound answer after it.
///
/// The row exists only because the repository's loader already proved the
/// shape: direction inbound, phase reply, no later outbound touch on the
/// same relationship, the target live and not do-not-contact. The fields
/// here are what the decision — and the audit trail — still needs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnansweredReplySnapshot {
    /// `outreach_interactions.id` of the reply being answered — the pin
    /// that makes a second answer provably a different action.
    pub interaction_id: i64,
    pub target_id: OutreachTargetId,
    pub target_version: i64,
    /// The counterparty's display name, resolved by the loader — a reply
    /// is addressed to a person, not an id.
    pub target_name: String,
    pub target_kind: OutreachTargetKind,
    /// The disposition the interaction carries — `positive` or `received`
    /// are the only values that may reach a request.
    pub reply_disposition: OutreachReplyDisposition,
    /// The sheet's raw verdict when the reply was imported — rides the
    /// action for audit and picks the scaffold's shape; never quoted in
    /// the letter itself.
    pub sheet_verdict: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub replied_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyRescueDecision {
    Hold(ReplyRescueHold),
    /// A reply draft may be raised for the operator.
    Request {
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyRescueHold {
    /// The row is not a reply waiting on us — defensive; the loader does
    /// not produce one, the gate stays so a bad join cannot send.
    NotAReply,
    /// They asked not to be written to. The line does not move because
    /// the email would be an answer rather than a pitch.
    DoNotContact,
    /// The sheet already recorded a no. A courteous close is a human's
    /// letter to write — the autopilot does not reply to a refusal.
    Declined,
    /// The reply arrived in the future relative to the decision clock —
    /// a clock or import error, not work to schedule.
    InvalidSnapshot,
    /// The target row carried no version to pin — a send against a target
    /// that changed mid-approval is how the wrong letter reaches a person.
    Unversioned,
}

#[must_use]
pub fn evaluate_reply_rescue(
    snapshot: &UnansweredReplySnapshot,
    now: OffsetDateTime,
) -> ReplyRescueDecision {
    if snapshot.interaction_id <= 0 || snapshot.target_version <= 0 {
        return ReplyRescueDecision::Hold(ReplyRescueHold::Unversioned);
    }
    if snapshot.replied_at > now {
        return ReplyRescueDecision::Hold(ReplyRescueHold::InvalidSnapshot);
    }
    match snapshot.reply_disposition {
        OutreachReplyDisposition::DoNotContact => {
            ReplyRescueDecision::Hold(ReplyRescueHold::DoNotContact)
        }
        OutreachReplyDisposition::Declined => ReplyRescueDecision::Hold(ReplyRescueHold::Declined),
        OutreachReplyDisposition::None => ReplyRescueDecision::Hold(ReplyRescueHold::NotAReply),
        OutreachReplyDisposition::Received | OutreachReplyDisposition::Positive => {
            // A positive answer and a bare "they wrote" get the same ask —
            // write back — at the same certainty. What differs is the
            // scaffold's shape, which the letter composer reads off the
            // disposition and the sheet verdict, not off this number.
            ReplyRescueDecision::Request {
                confidence: Confidence::saturating_from_basis_points(9_000),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn answered() -> UnansweredReplySnapshot {
        UnansweredReplySnapshot {
            interaction_id: 7,
            target_id: OutreachTargetId::new(),
            target_version: 3,
            target_name: "Radiowa Trójka".to_owned(),
            target_kind: OutreachTargetKind::Radio,
            reply_disposition: OutreachReplyDisposition::Received,
            sheet_verdict: Some("GMAIL_REPLY".to_owned()),
            replied_at: now() - Duration::days(4),
        }
    }

    #[test]
    fn a_received_reply_asks_for_an_answer() {
        assert!(matches!(
            evaluate_reply_rescue(&answered(), now()),
            ReplyRescueDecision::Request { .. }
        ));
    }

    #[test]
    fn a_positive_reply_asks_for_an_answer() {
        let mut snapshot = answered();
        snapshot.reply_disposition = OutreachReplyDisposition::Positive;
        snapshot.sheet_verdict = Some("POSITIVE".to_owned());
        assert!(matches!(
            evaluate_reply_rescue(&snapshot, now()),
            ReplyRescueDecision::Request { .. }
        ));
    }

    #[test]
    fn a_declined_reply_is_not_answered_for_them() {
        let mut snapshot = answered();
        snapshot.reply_disposition = OutreachReplyDisposition::Declined;
        snapshot.sheet_verdict = Some("NEGATIVE".to_owned());
        assert_eq!(
            evaluate_reply_rescue(&snapshot, now()),
            ReplyRescueDecision::Hold(ReplyRescueHold::Declined),
        );
    }

    #[test]
    fn a_do_not_contact_reply_is_never_answered() {
        let mut snapshot = answered();
        snapshot.reply_disposition = OutreachReplyDisposition::DoNotContact;
        assert_eq!(
            evaluate_reply_rescue(&snapshot, now()),
            ReplyRescueDecision::Hold(ReplyRescueHold::DoNotContact),
        );
    }

    #[test]
    fn an_unversioned_target_cannot_be_replied_to() {
        let mut snapshot = answered();
        snapshot.target_version = 0;
        assert_eq!(
            evaluate_reply_rescue(&snapshot, now()),
            ReplyRescueDecision::Hold(ReplyRescueHold::Unversioned),
        );
    }

    #[test]
    fn a_reply_from_the_future_is_not_work() {
        let mut snapshot = answered();
        snapshot.replied_at = now() + Duration::days(1);
        assert_eq!(
            evaluate_reply_rescue(&snapshot, now()),
            ReplyRescueDecision::Hold(ReplyRescueHold::InvalidSnapshot),
        );
    }
}
