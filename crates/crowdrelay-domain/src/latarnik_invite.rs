//! Asking a promoter, a journalist or a photographer to also hear the dates
//! first (P.1).
//!
//! # The person on the other end
//!
//! These are people in their thirties and forties who book rooms, write about
//! records and shoot shows for a living or close to it. They already get more
//! band mail than they can read, most of it written by software that has never
//! met them. An invitation that reads like a newsletter signup is worse than no
//! invitation: it costs the relationship the band spent a year building, to
//! gain a mailing-list row.
//!
//! So the rules here are mostly refusals, and each refusal is the difference
//! between a professional courtesy and spam:
//!
//! * **Never cold.** Only somebody the band already has a relationship with is
//!   asked. A contact scraped from a directory last week is not invited, ever.
//!   This is the whole line between this feature and a bot.
//! * **Once, ever.** A second ask is nagging. If the first was ignored, that
//!   was the answer.
//! * **Never on top of business.** Somebody asked for a date this month is not
//!   asked for anything else. The contact governor already enforces a week; an
//!   invitation is not urgent and waits [`QUIET_DAYS_AFTER_CONTACT`].
//! * **Never empty-handed.** The invitation names a concrete reason tied to
//!   them — a date in their city, a record they wrote about. Without one there
//!   is nothing to say, and the honest move is to say nothing.
//! * **Not to people who are already in.** A consented fan does not need an
//!   invitation to something they already receive.
//! * **Never back to somebody who left.** An address that once had marketing
//!   consent and withdrew it is not a lapsed lead. It is a person who said no
//!   in the only way the system offers, and asking again through the other role
//!   is the oldest trick in mailing-list software. The working relationship
//!   continues; the invitation does not.
//!
//! # What is actually offered
//!
//! Not "news and updates". Three things a working professional has a use for:
//! dates before they are public, the material they would otherwise have to ask
//! for (photo, one-paragraph bio, the poster), and — after a show they helped
//! with — what came of it. The offer is stated plainly, with the frequency and
//! the way out in the same breath, because a person who knows how to leave is a
//! person who does not have to decide today.

use serde::{Deserialize, Serialize};

use crate::gig_letter::{LetterLanguage, SenderIdentity};

/// Days of silence an invitation waits for after any outward contact.
///
/// The contact governor's own window is a week and binds every channel. This is
/// longer on purpose: a letter asking a promoter for a date and a letter asking
/// them to join a list are the same band arriving twice, and the second one is
/// the one that can wait. Three weeks is long enough that the two do not read
/// as one campaign.
pub const QUIET_DAYS_AFTER_CONTACT: i64 = 21;

/// The relationship score below which nobody is invited.
///
/// The scale is 0–100 and a bare directory entry starts at 50. Sixty means
/// something happened: a reply, a booked night, a piece written. It is
/// deliberately above the default rather than at it — "we have their address"
/// is not a relationship, and treating it as one is exactly how a band burns a
/// scene.
pub const MINIMUM_RELATIONSHIP: i32 = 60;

/// What the band knows about this person, as the rows say.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContactStanding {
    pub display_name: String,
    /// `promoter`, `local_press`, `photographer` — how the band knows them.
    pub role: String,
    /// City they work in, when recorded. Used to pick the reason.
    pub city: Option<String>,
    pub relationship_score: i32,
    /// Has the band ever had an answer from them? A reply outranks a score:
    /// somebody who wrote back is a relationship whatever the number says.
    pub has_replied: bool,
    pub do_not_contact: bool,
    pub accepts_outreach: bool,
    /// Days since the band last contacted them about anything. `None` means
    /// never — which is not the same as long ago, and is why a cold contact
    /// cannot pass the relationship test above.
    pub days_since_last_contact: Option<i64>,
    /// Already asked once. One ask is the whole budget.
    pub already_invited: bool,
    /// Already a consented listener. Nothing to offer.
    pub already_a_fan: bool,
    /// This address held marketing consent at some point and does not now.
    /// They opted out, and the other role is not a way back in.
    pub previously_opted_out: bool,
    /// The double opt-in is already in their inbox — the ask is open, not
    /// declined. Inviting again now would read as a reminder to confirm.
    pub opt_in_pending: bool,
}

/// Why this person is worth writing to *now*, in their own terms.
///
/// An invitation with no reason is a newsletter pitch. Every arm here is a fact
/// the band can point at, and the copy states it in the first sentence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InviteReason {
    /// A date in their city, soon. The strongest reason: it is useful to them
    /// before it is useful to the band.
    UpcomingShowInTheirCity { city: String, when: String },
    /// They were at, wrote about or booked a night that happened.
    SharedPastShow { venue: String, when: String },
    /// A record is out or close, and they cover records.
    RecentRelease { title: String },
}

/// Whether the invitation may go, and if not, why not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InviteDecision {
    Send,
    Hold(InviteHold),
}

/// The named reasons not to ask. Each is shown to the operator as a sentence
/// rather than a greyed-out row: a person who cannot be asked this week is a
/// different thing from a person who must never be asked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InviteHold {
    /// Marked do-not-contact, or the contact does not take outreach.
    Refused,
    /// No relationship yet. The refusal that keeps this from being a bot.
    TooCold,
    /// Contacted recently about something else.
    TooSoon { days_to_wait: i64 },
    /// Asked once already.
    AlreadyAsked,
    /// Already receives the dates.
    AlreadyIn,
    /// Had the dates and left. The strongest hold there is, and the only one
    /// that never expires.
    OptedOut,
    /// Their confirmation email is still open — the ask already landed and
    /// they have not answered it yet.
    OptInPending,
    /// Nothing concrete to say to them right now.
    NothingToOffer,
}

impl InviteHold {
    /// The operator-facing sentence. Says what would change it, where anything
    /// would — a hold that only says no teaches nobody anything.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Refused => {
                "they asked not to be contacted, or the contact is not open to outreach".to_owned()
            }
            Self::TooCold => "no relationship on record yet — a first letter should be about \
                 work, not about a mailing list"
                .to_owned(),
            Self::TooSoon { days_to_wait } => format!(
                "the band wrote to them recently about something else — this can go in \
                 {days_to_wait} days"
            ),
            Self::AlreadyAsked => {
                "they were asked once and did not take it up; a second ask is nagging".to_owned()
            }
            Self::AlreadyIn => "they already get the dates".to_owned(),
            Self::OptedOut => "they were on the list and unsubscribed — the working \
                 relationship stands, the invitation does not go out again"
                .to_owned(),
            Self::OptInPending => "their confirmation is still open — the ask is already \
                 in their inbox, and a second one now reads as pressure"
                .to_owned(),
            Self::NothingToOffer => "nothing concrete to tell them right now — no date in their \
                 city, no shared night, no new record"
                .to_owned(),
        }
    }
}

/// Decides whether this person may be invited now.
#[must_use]
pub fn decide(standing: &ContactStanding, reason: Option<&InviteReason>) -> InviteDecision {
    if standing.do_not_contact || !standing.accepts_outreach {
        return InviteDecision::Hold(InviteHold::Refused);
    }
    if standing.already_a_fan {
        return InviteDecision::Hold(InviteHold::AlreadyIn);
    }
    // Checked before everything except an outright refusal. A withdrawn consent
    // outranks a warm relationship, a good reason and a long silence: those are
    // arguments for writing, and this is the one fact that answers all of them.
    if standing.previously_opted_out {
        return InviteDecision::Hold(InviteHold::OptedOut);
    }
    // A pending double opt-in is the ask already in their inbox — it is not
    // a no, but a second letter now is a reminder to confirm.
    if standing.opt_in_pending {
        return InviteDecision::Hold(InviteHold::OptInPending);
    }
    if standing.already_invited {
        return InviteDecision::Hold(InviteHold::AlreadyAsked);
    }
    // A reply outranks the score. Somebody who wrote back is a relationship
    // whatever a number derived from guesses says about them.
    let known = standing.has_replied || standing.relationship_score >= MINIMUM_RELATIONSHIP;
    // Never contacted at all is the cold case, whatever the score says: a high
    // score with no contact is an estimate, not a history.
    if !known || standing.days_since_last_contact.is_none() {
        return InviteDecision::Hold(InviteHold::TooCold);
    }
    if let Some(days) = standing.days_since_last_contact
        && days < QUIET_DAYS_AFTER_CONTACT
    {
        return InviteDecision::Hold(InviteHold::TooSoon {
            days_to_wait: QUIET_DAYS_AFTER_CONTACT - days,
        });
    }
    if reason.is_none() {
        return InviteDecision::Hold(InviteHold::NothingToOffer);
    }
    InviteDecision::Send
}

/// The invitation itself.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Invite {
    pub subject: String,
    pub body: String,
}

/// Composes the invitation in the recipient's language.
///
/// Deliberately short. A working promoter reads the first two lines and decides;
/// everything after that is either the offer or the way out. No adjectives about
/// the band, no exclamation marks, no hashtags — the register is a colleague
/// writing, because that is what the relationship is.
#[must_use]
pub fn compose(
    sender: &SenderIdentity,
    standing: &ContactStanding,
    reason: &InviteReason,
    member_area_url: &str,
    language: LetterLanguage,
) -> Invite {
    let act = sender.act_name.trim();
    let name = standing.display_name.trim();
    match language {
        LetterLanguage::Polish => polish(act, name, reason, member_area_url),
        LetterLanguage::English => english(act, name, reason, member_area_url),
    }
}

fn polish(act: &str, name: &str, reason: &InviteReason, url: &str) -> Invite {
    let opening = match reason {
        InviteReason::UpcomingShowInTheirCity { city, when } => format!(
            "Gramy w {city} {when}. Zanim to wyjdzie oficjalnie, chcieliśmy dać znać Tobie."
        ),
        InviteReason::SharedPastShow { venue, when } => format!(
            "Graliśmy razem w {venue} ({when}) — dzięki jeszcze raz. Piszemy w jednej sprawie."
        ),
        InviteReason::RecentRelease { title } => {
            format!("Wyszła nasza nowa rzecz — {title}. Piszemy w jednej sprawie.")
        }
    };
    let body = format!(
        "Cześć {name},\n\
         \n\
         {opening}\n\
         \n\
         Prowadzimy małą listę dla ludzi z branży, z którymi już pracowaliśmy. \
         Trafiają tam trzy rzeczy i nic poza tym:\n\
         \n\
         - terminy, zanim ogłosimy je publicznie,\n\
         - materiały gotowe do użycia: zdjęcia w druku, nota biograficzna na akapit, plakat,\n\
         - po koncercie krótkie podsumowanie, co z niego wyszło.\n\
         \n\
         Kilka wiadomości na kwartał, nie więcej. Wypisanie się to jedno kliknięcie \
         w każdej z nich.\n\
         \n\
         Jeśli chcesz być na liście: {url}\n\
         Jeśli nie — nie wracamy z tym drugi raz.\n\
         \n\
         Pozdrawiamy,\n\
         {act}"
    );
    Invite {
        subject: format!("Terminy zanim wyjdą oficjalnie — {act}"),
        body,
    }
}

fn english(act: &str, name: &str, reason: &InviteReason, url: &str) -> Invite {
    let opening = match reason {
        InviteReason::UpcomingShowInTheirCity { city, when } => format!(
            "We are playing {city} on {when}. Before it goes public, we wanted you to know."
        ),
        InviteReason::SharedPastShow { venue, when } => format!(
            "We played {venue} together ({when}) — thanks again for that. One thing we wanted \
             to ask."
        ),
        InviteReason::RecentRelease { title } => {
            format!("Our new record is out — {title}. One thing we wanted to ask.")
        }
    };
    let body = format!(
        "Hi {name},\n\
         \n\
         {opening}\n\
         \n\
         We keep a small list for the people in the industry we have already worked with. \
         Three things go on it and nothing else:\n\
         \n\
         - dates, before we announce them publicly,\n\
         - material ready to use: print photos, a one-paragraph bio, the poster,\n\
         - after a show, a short note on what came of it.\n\
         \n\
         A few messages a quarter, no more. Leaving is one click in any of them.\n\
         \n\
         If you want to be on it: {url}\n\
         If not, we will not come back to this a second time.\n\
         \n\
         Best,\n\
         {act}"
    );
    Invite {
        subject: format!("Dates before they go public — {act}"),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known_promoter() -> ContactStanding {
        ContactStanding {
            display_name: "Anna".to_owned(),
            role: "promoter".to_owned(),
            city: Some("Wrocław".to_owned()),
            relationship_score: 72,
            has_replied: true,
            do_not_contact: false,
            accepts_outreach: true,
            days_since_last_contact: Some(40),
            already_invited: false,
            already_a_fan: false,
            previously_opted_out: false,
            opt_in_pending: false,
        }
    }

    fn reason() -> InviteReason {
        InviteReason::UpcomingShowInTheirCity {
            city: "Wrocław".to_owned(),
            when: "12 października".to_owned(),
        }
    }

    /// The line between this feature and a bot: a contact with no history is
    /// never asked, however good the estimated score looks.
    #[test]
    fn a_cold_contact_is_never_invited() {
        let cold = ContactStanding {
            has_replied: false,
            relationship_score: 95,
            days_since_last_contact: None,
            ..known_promoter()
        };
        assert_eq!(
            decide(&cold, Some(&reason())),
            InviteDecision::Hold(InviteHold::TooCold)
        );
    }

    /// A reply outranks the score. Somebody who wrote back is a relationship
    /// whatever a derived number says.
    #[test]
    fn a_reply_counts_for_more_than_the_score() {
        let replied = ContactStanding {
            relationship_score: 20,
            has_replied: true,
            ..known_promoter()
        };
        assert_eq!(decide(&replied, Some(&reason())), InviteDecision::Send);
    }

    /// Business first. A promoter asked for a date this month is not also asked
    /// to join a list — that is the band arriving twice.
    #[test]
    fn recent_business_contact_makes_the_invitation_wait() {
        let busy = ContactStanding {
            days_since_last_contact: Some(5),
            ..known_promoter()
        };
        match decide(&busy, Some(&reason())) {
            InviteDecision::Hold(InviteHold::TooSoon { days_to_wait }) => {
                assert_eq!(days_to_wait, 16);
            }
            other => panic!("expected a wait, got {other:?}"),
        }
    }

    /// One ask is the whole budget. Silence was the answer.
    #[test]
    fn nobody_is_asked_twice() {
        let asked = ContactStanding {
            already_invited: true,
            ..known_promoter()
        };
        assert_eq!(
            decide(&asked, Some(&reason())),
            InviteDecision::Hold(InviteHold::AlreadyAsked)
        );
    }

    /// Nothing to say means say nothing. An invitation with no reason attached
    /// is a newsletter pitch with a friendly font.
    #[test]
    fn an_invitation_with_no_reason_is_held() {
        assert_eq!(
            decide(&known_promoter(), None),
            InviteDecision::Hold(InviteHold::NothingToOffer)
        );
    }

    /// Already receiving the dates: there is nothing to offer, and the hold
    /// says so rather than treating them as unreachable.
    #[test]
    fn a_consented_fan_is_left_alone() {
        let already = ContactStanding {
            already_a_fan: true,
            ..known_promoter()
        };
        assert_eq!(
            decide(&already, Some(&reason())),
            InviteDecision::Hold(InviteHold::AlreadyIn)
        );
    }

    /// The rule that decides whether this is a courtesy or a trick: somebody
    /// who unsubscribed is not re-approached through their other role.
    #[test]
    fn somebody_who_left_the_list_is_never_asked_back() {
        let left = ContactStanding {
            previously_opted_out: true,
            ..known_promoter()
        };
        assert_eq!(
            decide(&left, Some(&reason())),
            InviteDecision::Hold(InviteHold::OptedOut)
        );
        // And it outranks every argument for writing: a strong relationship, a
        // concrete reason, a long silence.
        let warm_and_quiet = ContactStanding {
            previously_opted_out: true,
            relationship_score: 100,
            has_replied: true,
            days_since_last_contact: Some(365),
            ..known_promoter()
        };
        assert_eq!(
            decide(&warm_and_quiet, Some(&reason())),
            InviteDecision::Hold(InviteHold::OptedOut)
        );
    }

    /// Every hold names what would change it, where anything would.
    #[test]
    fn every_hold_says_something_useful() {
        for hold in [
            InviteHold::Refused,
            InviteHold::TooCold,
            InviteHold::TooSoon { days_to_wait: 3 },
            InviteHold::AlreadyAsked,
            InviteHold::AlreadyIn,
            InviteHold::OptedOut,
            InviteHold::OptInPending,
            InviteHold::NothingToOffer,
        ] {
            assert!(
                hold.message().len() > 20,
                "{hold:?} is a shrug, not a reason"
            );
        }
    }

    /// The copy is the feature. It has to open with their fact, state the offer
    /// as three concrete things, name the frequency, and hand over the way out
    /// in the same breath.
    #[test]
    fn the_polish_invitation_reads_like_a_colleague_not_a_campaign() {
        let sender = SenderIdentity {
            act_name: "Virya".to_owned(),
            style: Some("modern metal".to_owned()),
            home_city: Some("Wrocław".to_owned()),
            site_url: Some("https://virya.music/".to_owned()),
        };
        let invite = compose(
            &sender,
            &known_promoter(),
            &reason(),
            "https://virya.music/pl/latarnik",
            LetterLanguage::Polish,
        );
        assert_eq!(invite.subject, "Terminy zanim wyjdą oficjalnie — Virya");
        assert!(invite.body.starts_with("Cześć Anna,"));
        // Their fact first, before anything the band wants.
        assert!(invite.body.contains("Gramy w Wrocław 12 października"));
        // The offer, the frequency and the exit are all present.
        assert!(invite.body.contains("zanim ogłosimy je publicznie"));
        assert!(invite.body.contains("Kilka wiadomości na kwartał"));
        assert!(invite.body.contains("Wypisanie się to jedno kliknięcie"));
        assert!(invite.body.contains("nie wracamy z tym drugi raz"));
        assert!(invite.body.contains("https://virya.music/pl/latarnik"));
        // Register: no exclamation marks, no hashtags, no emoji-bait.
        assert!(!invite.body.contains('!'), "{}", invite.body);
        assert!(!invite.body.contains('#'), "{}", invite.body);
    }

    /// The English version carries the same promises — the offer cannot be
    /// generous in one language and vague in the other.
    #[test]
    fn the_english_invitation_makes_the_same_promises() {
        let sender = SenderIdentity {
            act_name: "Virya".to_owned(),
            ..SenderIdentity::default()
        };
        let invite = compose(
            &sender,
            &known_promoter(),
            &InviteReason::RecentRelease {
                title: "Nowy Świat".to_owned(),
            },
            "https://virya.music/en/lighthouse",
            LetterLanguage::English,
        );
        assert!(invite.body.contains("Our new record is out — Nowy Świat"));
        assert!(invite.body.contains("before we announce them publicly"));
        assert!(invite.body.contains("A few messages a quarter"));
        assert!(invite.body.contains("Leaving is one click"));
        assert!(!invite.body.contains('!'));
    }
}
