//! The reply letter — the answer to somebody who wrote back.
//!
//! A pitch letter (`outreach_letter`) opens a conversation; this one
//! continues it. The difference that shapes the design: the imported reply
//! cohort carries the sheet's *verdict* but not the reply's text — the
//! words live in the band's mailbox. So the draft is a scaffold shaped by
//! the verdict, not an answer to content nobody here has read. The
//! approver edits it against the real thread; the send goes out verbatim
//! as always, so what the card shows is what the recipient gets.
//!
//! Three shapes:
//!
//! - **Positive** — they said yes; the reply thanks them and moves the
//!   conversation to the next concrete step.
//! - **Negotiation** — a booking-side reply where terms are in play; the
//!   reply keeps the thread warm without conceding a number nobody quoted.
//! - **Holding** — the verdict records that they wrote, not what they said
//!   (`GMAIL_REPLY`, unknown codes). A short acknowledgement that keeps
//!   the thread alive while the operator reads the actual reply — the one
//!   shape whose whole job is to not be the last unanswered message.
//!
//! The letter is plain and stays plain: no invented claims, no hype, no
//! genre adjectives. The tenant's register per the working rules — when no
//! samples exist, write plainly and invent no personality.

use crate::gig_letter::SenderIdentity;
use crate::outreach::OutreachTargetKind;
use crate::outreach_letter::OutreachLetter;

/// What the reply's disposition told us, decided before composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyShape {
    /// The answer was affirmative — move it forward.
    Positive,
    /// Terms are in play — acknowledge without committing numbers.
    Negotiation,
    /// They wrote; what they said is not recorded here.
    Holding,
}

/// Everything the reply letter is composed from.
#[derive(Clone, Debug)]
pub struct ReplyLetterInput<'a> {
    pub sender: &'a SenderIdentity,
    /// The counterparty's display name.
    pub target_name: &'a str,
    /// Shapes the next-step ask — a playlist gets the track again, a venue
    /// gets availability. `None` (a kind the vocabulary has not met yet)
    /// falls back to the generic offer rather than refusing the reply.
    pub target_kind: Option<OutreachTargetKind>,
    pub shape: ReplyShape,
    /// The pitched release — the reply threads onto it.
    pub pitch_title: &'a str,
    pub pitch_url: &'a str,
}

/// Why a reply letter could not be composed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyLetterRefusal {
    NoTarget,
    NoSenderName,
}

impl ReplyLetterRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoTarget => "the reply has nobody to address",
            Self::NoSenderName => "this workspace has no name to sign with",
        }
    }
}

const MAX_SUBJECT: usize = 160;

/// Composes the reply, or refuses with the fact that is missing.
///
/// # Errors
/// Returns the missing fact. There is deliberately no `NoPitch` refusal:
/// a reply owes the thread its answer — the pitch line shortens away when
/// no active release plan carries a link, rather than refusing the whole
/// reply for want of a URL.
pub fn compose_reply_letter(
    input: &ReplyLetterInput<'_>,
) -> Result<OutreachLetter, ReplyLetterRefusal> {
    let target = crate::outreach_letter::salutation_name(input.target_name);
    let act = input.sender.act_name.trim();
    if target.is_empty() {
        return Err(ReplyLetterRefusal::NoTarget);
    }
    if act.is_empty() {
        return Err(ReplyLetterRefusal::NoSenderName);
    }
    let subject = if input.pitch_title.trim().is_empty() {
        format!("Re: {act}")
    } else {
        format!("Re: {act} — {}", input.pitch_title.trim())
    };
    let subject = if subject.chars().count() > MAX_SUBJECT {
        format!("Re: {act}")
    } else {
        subject
    };

    let listen = (!input.pitch_url.trim().is_empty()).then(|| {
        format!(
            "Here it is again in case it helps: {}",
            input.pitch_url.trim()
        )
    });
    let signoff = format!("Best,\n{act}");

    let body = match input.shape {
        ReplyShape::Positive => {
            let next_step = match input.target_kind {
                Some(OutreachTargetKind::Playlist) => {
                    "very happy to send the full record or anything else that makes the placement easier"
                }
                Some(OutreachTargetKind::Radio) => {
                    "happy to send the full track, a clean edit, or a short intro — whatever fits the show"
                }
                Some(OutreachTargetKind::Press) | Some(OutreachTargetKind::MediaPatronage) => {
                    "happy to send the full record, a CD, or answer anything for the piece"
                }
                Some(OutreachTargetKind::Creator) => {
                    "happy to share stems, the full record, or talk about what a feature could look like"
                }
                Some(OutreachTargetKind::SupportSlot) => {
                    "we can send over our live videos and current availability"
                }
                Some(OutreachTargetKind::Endorsement) => {
                    "happy to talk through what a collaboration could look like"
                }
                // Representation replies ride their own approach lane, and a
                // kind the vocabulary has not met yet gets the same honest
                // generic offer rather than a refusal.
                Some(OutreachTargetKind::Agent) | Some(OutreachTargetKind::Label) | None => {
                    "happy to share more about the band and what we are planning"
                }
            };
            let mut lines = vec![
                format!("Hi {target},"),
                String::new(),
                format!("Thank you — that's great to hear, and {next_step}.",),
            ];
            if let Some(listen) = listen {
                lines.push(String::new());
                lines.push(listen);
            }
            lines.push(String::new());
            lines.push(signoff);
            lines.join("\n")
        }
        ReplyShape::Negotiation => {
            let mut lines = vec![
                format!("Hi {target},"),
                String::new(),
                "Thank you for getting back to us — we're interested. What did you have in mind on terms?"
                    .to_owned(),
            ];
            if let Some(listen) = listen {
                lines.push(String::new());
                lines.push(listen);
            }
            lines.push(String::new());
            lines.push(signoff);
            lines.join("\n")
        }
        ReplyShape::Holding => {
            let mut lines = vec![
                format!("Hi {target},"),
                String::new(),
                "Thank you for getting back to us — much appreciated. We'll come back to you with the details shortly."
                    .to_owned(),
            ];
            if let Some(listen) = listen {
                lines.push(String::new());
                lines.push(listen);
            }
            lines.push(String::new());
            lines.push(signoff);
            lines.join("\n")
        }
    };
    Ok(OutreachLetter { subject, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gig_letter::SenderIdentity;

    fn sender() -> SenderIdentity {
        SenderIdentity {
            act_name: "VIRYA".to_owned(),
            style: None,
            home_city: Some("Wrocław".to_owned()),
            site_url: None,
        }
    }

    fn input(shape: ReplyShape) -> ReplyLetterInput<'static> {
        ReplyLetterInput {
            sender: Box::leak(Box::new(sender())),
            target_name: "Dobrze Rockują",
            target_kind: Some(OutreachTargetKind::Radio),
            shape,
            pitch_title: "Technophobia",
            pitch_url: "https://virya.example/listen",
        }
    }

    #[test]
    fn positive_reply_thanks_and_offers_the_next_step() {
        let letter = compose_reply_letter(&input(ReplyShape::Positive)).expect("composes");
        assert_eq!(letter.subject, "Re: VIRYA — Technophobia");
        assert!(letter.body.contains("Thank you"));
        assert!(letter.body.contains("https://virya.example/listen"));
        assert!(letter.body.ends_with("VIRYA"));
    }

    #[test]
    fn negotiation_reply_keeps_terms_open_without_quoting_any() {
        let letter = compose_reply_letter(&input(ReplyShape::Negotiation)).expect("composes");
        assert!(letter.body.contains("terms"));
        // The scaffold concedes nothing — no currency figure rides in it.
        for marker in ["PLN", "EUR", "USD", "zł", "€", "$"] {
            assert!(!letter.body.contains(marker), "body carries {marker}");
        }
    }

    #[test]
    fn holding_reply_acknowledges_without_pretending_to_know_more() {
        let letter = compose_reply_letter(&input(ReplyShape::Holding)).expect("composes");
        assert!(letter.body.contains("getting back"));
        assert!(letter.body.contains("details shortly"));
    }

    #[test]
    fn missing_target_or_sender_refuses() {
        let mut bad = input(ReplyShape::Positive);
        bad.target_name = "  ";
        assert_eq!(
            compose_reply_letter(&bad),
            Err(ReplyLetterRefusal::NoTarget)
        );
    }

    #[test]
    fn a_reply_needs_no_pitch_link() {
        let mut no_pitch = input(ReplyShape::Holding);
        no_pitch.pitch_title = "";
        no_pitch.pitch_url = "";
        let letter =
            compose_reply_letter(&no_pitch).expect("reply does not refuse on a missing pitch");
        assert_eq!(letter.subject, "Re: VIRYA");
        assert!(!letter.body.contains("Listen"));
    }
}
