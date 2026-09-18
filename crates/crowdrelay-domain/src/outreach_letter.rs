//! The letter an outreach target actually receives.
//!
//! Same rule as `booking_letter` and `gig_letter`: the words are composed
//! here, at the moment the action is written, so the approval shows exactly
//! what the curator, editor or station reads and the executor sends
//! `draft.body` verbatim. Before this existed the event carried
//! `template_key` and `evidence` and left the writing to whatever ran
//! downstream — which meant the operator approved "outreach contact: X" and
//! a stranger received a pitch nobody had read, with the pitch itself
//! resolved from environment variables outside any approval.
//!
//! The pitch is the tenant's own catalogue: the release plan that carries a
//! listen link. A workspace with no listenable release gets no letter — the
//! composer refuses rather than pitching "our latest release" at nothing.
//! Agents and labels are not composed here at all: §4h-12 routes them
//! through the approach path, where the published listing is the pitch.

use serde::{Deserialize, Serialize};

use crate::gig_letter::SenderIdentity;
use crate::outreach::{OutreachPhase, OutreachTargetKind};

/// A finished letter — the wire shape every letter executor reads.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutreachLetter {
    pub subject: String,
    pub body: String,
}

/// Everything the letter is composed from.
#[derive(Clone, Debug)]
pub struct OutreachLetterInput<'a> {
    pub sender: &'a SenderIdentity,
    /// The target's display name — a playlist, a station, an editor.
    pub target_name: &'a str,
    /// What the target is being asked for — playlist consideration, airplay,
    /// coverage. Shapes the ask line.
    pub target_kind: OutreachTargetKind,
    /// The release being pitched — a title from the tenant's release plans.
    pub pitch_title: &'a str,
    /// Where the target listens to it.
    pub pitch_url: &'a str,
    pub phase: OutreachPhase,
}

/// Why a letter could not be composed — each a missing fact, shown to the
/// operator rather than translated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachLetterRefusal {
    NoTarget,
    NoSenderName,
    /// No release plan carries a listen link — there is nothing to pitch.
    NoPitch,
    /// Agents and labels are approached through the listing, not pitched.
    RepresentationTarget,
}

impl OutreachLetterRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoTarget => "the letter has nobody to address",
            Self::NoSenderName => "this workspace has no name to sign with",
            Self::NoPitch => "no active release plan carries a listen link",
            Self::RepresentationTarget => {
                "agents and labels are approached through the listing, not pitched"
            }
        }
    }
}

/// Subjects stay well under the executor's 220 cap — a subject a mail client
/// truncates is a subject nobody read.
const MAX_SUBJECT: usize = 160;

/// Composes the letter, or refuses with the fact that is missing.
///
/// # Errors
///
/// Returns the missing fact. Every refusal is recoverable by the operator —
/// which is why none of them is a silent fallback.
pub fn compose_outreach_letter(
    input: &OutreachLetterInput<'_>,
) -> Result<OutreachLetter, OutreachLetterRefusal> {
    let target = input.target_name.trim();
    let act = input.sender.act_name.trim();
    let title = input.pitch_title.trim();
    let url = input.pitch_url.trim();
    if target.is_empty() {
        return Err(OutreachLetterRefusal::NoTarget);
    }
    if act.is_empty() {
        return Err(OutreachLetterRefusal::NoSenderName);
    }
    let Some(ask) = purpose(input.target_kind) else {
        return Err(OutreachLetterRefusal::RepresentationTarget);
    };
    if title.is_empty() || url.is_empty() {
        return Err(OutreachLetterRefusal::NoPitch);
    }
    Ok(match input.phase {
        OutreachPhase::Initial => initial(input, target, act, title, url, ask),
        OutreachPhase::FollowUp => follow_up(input, target, act, title, url, ask),
    })
}

/// What the target is being asked for, in the words the letter uses. `None`
/// for the kinds no pitch letter may be written to — the compose checks the
/// refusal, so a new kind answers for itself rather than pitching under a
/// wrong purpose.
fn purpose(kind: OutreachTargetKind) -> Option<&'static str> {
    Some(match kind {
        OutreachTargetKind::Playlist => "playlist consideration",
        OutreachTargetKind::Radio => "airplay",
        OutreachTargetKind::Press => "coverage",
        OutreachTargetKind::Creator => "a feature or collaboration",
        OutreachTargetKind::SupportSlot => "a support slot",
        OutreachTargetKind::Endorsement => "an endorsement",
        OutreachTargetKind::MediaPatronage => "media patronage",
        OutreachTargetKind::Agent | OutreachTargetKind::Label => return None,
    })
}

fn initial(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
    title: &str,
    url: &str,
    ask: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        format!(
            "{} and we would love to submit {title} for {ask}.",
            introduction(input.sender, act),
        ),
        String::new(),
        format!("Listen: {url}"),
        String::new(),
        "If it is not a fit, no worries — one follow-up is the maximum and any reply \
         stops automation."
            .to_owned(),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("{act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

fn follow_up(
    input: &OutreachLetterInput<'_>,
    target: &str,
    act: &str,
    title: &str,
    url: &str,
    ask: &str,
) -> OutreachLetter {
    let mut lines = vec![
        format!("Hi {target},"),
        String::new(),
        format!(
            "A quick follow-up on the note below — {title} is still with us for {ask}. \
             If it is not a fit, a short \"no\" is just as useful as a yes."
        ),
        String::new(),
        format!("Listen: {url}"),
    ];
    lines.extend(sign_off(input.sender, act));
    OutreachLetter {
        subject: truncate(format!("Follow-up: {act} — {title}"), MAX_SUBJECT),
        body: lines.join("\n"),
    }
}

/// "I am writing from {act}{, a {style} act}{ from {home_city}}" — each part
/// present only when the tenant's own records say it.
fn introduction(sender: &SenderIdentity, act: &str) -> String {
    let style = sender
        .style
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let home = sender
        .home_city
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (style, home) {
        (Some(style), Some(home)) => {
            format!("I am writing from {act}, a {style} act from {home},")
        }
        (Some(style), None) => format!("I am writing from {act}, a {style} act,"),
        (None, Some(home)) => format!("I am writing from {act}, from {home},"),
        (None, None) => format!("I am writing from {act},"),
    }
}

/// The closing block — the act's name and, where the tenant declared one,
/// its own site. No link line when there is no link to give.
fn sign_off(sender: &SenderIdentity, act: &str) -> Vec<String> {
    let mut lines = vec![String::new(), "Best,".to_owned(), act.to_owned()];
    if let Some(url) = sender
        .site_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        lines.push(url.to_owned());
    }
    lines
}

fn truncate(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> SenderIdentity {
        SenderIdentity {
            act_name: "VIRYA".to_owned(),
            style: Some("modern metal".to_owned()),
            home_city: Some("Wrocław".to_owned()),
            site_url: Some("https://virya.music".to_owned()),
        }
    }

    fn input<'a>(
        sender: &'a SenderIdentity,
        kind: OutreachTargetKind,
        phase: OutreachPhase,
    ) -> OutreachLetterInput<'a> {
        OutreachLetterInput {
            sender,
            target_name: "Metal Playlists Weekly",
            target_kind: kind,
            pitch_title: "our new single \"Rytuał\"",
            pitch_url: "https://virya.music/f/rytual",
            phase,
        }
    }

    #[test]
    fn an_initial_pitch_names_the_target_the_release_and_the_ask() {
        let sender = sender();
        let letter = compose_outreach_letter(&input(
            &sender,
            OutreachTargetKind::Playlist,
            OutreachPhase::Initial,
        ))
        .expect("a complete input composes");
        assert!(letter.body.contains("Hi Metal Playlists Weekly,"));
        assert!(
            letter
                .body
                .contains("VIRYA, a modern metal act from Wrocław")
        );
        assert!(
            letter
                .body
                .contains("\"Rytuał\" for playlist consideration")
        );
        assert!(letter.body.contains("Listen: https://virya.music/f/rytual"));
        assert!(letter.body.contains("one follow-up is the maximum"));
        assert!(letter.body.ends_with("https://virya.music"));
        assert_eq!(letter.subject, "VIRYA — our new single \"Rytuał\"");
    }

    #[test]
    fn a_followup_re_asks_without_rewriting_the_pitch() {
        let sender = sender();
        let letter = compose_outreach_letter(&input(
            &sender,
            OutreachTargetKind::Press,
            OutreachPhase::FollowUp,
        ))
        .expect("a follow-up composes");
        assert!(letter.body.contains("quick follow-up"));
        assert!(letter.body.contains("for coverage"));
        assert!(letter.subject.starts_with("Follow-up:"));
    }

    #[test]
    fn missing_facts_refuse_instead_of_filling_in() {
        let sender = sender();
        let no_target = OutreachLetterInput {
            target_name: " ",
            ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
        };
        assert_eq!(
            compose_outreach_letter(&no_target),
            Err(OutreachLetterRefusal::NoTarget)
        );
        let no_pitch = OutreachLetterInput {
            pitch_url: "",
            ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
        };
        assert_eq!(
            compose_outreach_letter(&no_pitch),
            Err(OutreachLetterRefusal::NoPitch)
        );
        let no_name = SenderIdentity {
            act_name: String::new(),
            ..sender.clone()
        };
        let no_sender = input(&no_name, OutreachTargetKind::Press, OutreachPhase::Initial);
        assert_eq!(
            compose_outreach_letter(&no_sender),
            Err(OutreachLetterRefusal::NoSenderName)
        );
    }

    #[test]
    fn representation_targets_are_never_pitched() {
        let sender = sender();
        for kind in [OutreachTargetKind::Agent, OutreachTargetKind::Label] {
            assert_eq!(
                compose_outreach_letter(&input(&sender, kind, OutreachPhase::Initial)),
                Err(OutreachLetterRefusal::RepresentationTarget),
                "{kind:?} must refuse — the listing carries that approach"
            );
        }
    }
}
