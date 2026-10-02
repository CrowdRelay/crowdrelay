//! One-tap Latarnik missions: what to ask an active Latarnik, if anything.
//!
//! A mission is one question about one real thing and one text the person may
//! send to one friend. It is *offered*, never pushed: it sits in the Latarnik's
//! own signed-in Signal session until they act or it expires. The decision here
//! is deterministic — facts in, at most one plan out — and the wording is
//! assembled from those facts, not generated: a show's title, city and date and
//! the person's own referral link. There is no personality in it to drift from
//! the band, and nothing the band has not already published.
//!
//! What this module refuses to do is as important as what it does:
//!
//! - **One at a time.** A Latarnik with an open mission is offered none. Cadence
//!   follows measured human yield: proven carriers may see a new *different*
//!   fact a little sooner; repeated ignored or clickless asks back off hard.
//! - **Only what is real.** A show mission needs a published show in the future,
//!   in the Latarnik's own city, inside [`SHOW_HORIZON`]; a release mission needs
//!   something published recently. No fact, no mission.
//! - **No reward lives here.** A tap, a share or a sent text is never rewarded;
//!   what a mission earns is decided by the existing referral rules when someone
//!   it brought actually arrives.

use serde::Serialize;
use time::{Date, Duration, OffsetDateTime};

/// Offered missions expire after this long: a stale ask is not an ask.
pub const MISSION_LIFETIME: Duration = Duration::days(10);
/// Baseline gap between two missions offered to the same person.
pub const COOLDOWN: Duration = Duration::days(14);
/// A proven carrier may see a new, different fact this soon. Success does not
/// waive the one-person/one-fact rules; it only avoids needless dead time.
pub const PROVEN_COOLDOWN: Duration = Duration::days(10);
/// Repeated taps with no real person following the link are a low-yield signal.
pub const CLICKLESS_COOLDOWN: Duration = Duration::days(30);
/// Two offers with no tap mean "leave this person alone", not "ask harder".
pub const QUIET_COOLDOWN: Duration = Duration::days(45);
/// A show is mission-worthy this far ahead and no further.
pub const SHOW_HORIZON: Duration = Duration::days(21);
/// A release is mission-worthy this long after it came out.
pub const RELEASE_WINDOW: Duration = Duration::days(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionKind {
    ShowOnePerson,
    ReleaseOnePerson,
}

impl MissionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ShowOnePerson => "show_one_person",
            Self::ReleaseOnePerson => "release_one_person",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    Pl,
    En,
}

impl Language {
    /// Anything that is not Polish is English: the band's two languages.
    #[must_use]
    pub fn from_locale(locale: Option<&str>) -> Self {
        match locale.map(|l| l.trim().to_ascii_lowercase()) {
            Some(l) if l.starts_with("pl") => Self::Pl,
            _ => Self::En,
        }
    }
}

/// A published show, as the mission needs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShowFact {
    pub event_id: uuid::Uuid,
    pub slug: String,
    pub title: String,
    pub city: Option<String>,
    pub starts_on: Date,
    pub starts_at: OffsetDateTime,
    /// The show is in a city the Latarnik has said they care about.
    pub in_their_city: bool,
}

/// Something the band published recently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseFact {
    pub content_source_id: uuid::Uuid,
    pub title: String,
    pub published_at: OffsetDateTime,
}

/// Recent observed advocacy outcomes. These are behaviour of the mission loop,
/// not personality scores. They only tune how often CrowdRelay asks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdvocacyYield {
    pub offered_90d: u32,
    pub tapped_90d: u32,
    pub human_clickers_90d: u32,
    pub completed_90d: u32,
}

/// What the evaluator may read: role state, real published facts, which facts
/// were already asked about, and bounded recent mission yield.
#[derive(Clone, Debug)]
pub struct MissionContext {
    /// Role is `active` and carries the referral capability.
    pub may_carry: bool,
    /// An offered or tapped mission is already open.
    pub has_open_mission: bool,
    pub last_offered_at: Option<OffsetDateTime>,
    pub advocacy_yield: AdvocacyYield,
    /// A person is never asked about the same show/release twice.
    pub seen_event_ids: Vec<uuid::Uuid>,
    pub seen_content_source_ids: Vec<uuid::Uuid>,
    pub language: Language,
    /// The person's own referral link, absolute. Without it there is nothing to
    /// send on, so there is no mission.
    pub referral_url: Option<String>,
    pub shows: Vec<ShowFact>,
    pub releases: Vec<ReleaseFact>,
}

/// The one mission to offer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissionPlan {
    pub kind: MissionKind,
    pub event_id: Option<uuid::Uuid>,
    pub content_source_id: Option<uuid::Uuid>,
    pub prompt: String,
    pub share_text: String,
}

fn date_label(language: Language, date: Date) -> String {
    let (day, month) = (date.day(), u8::from(date.month()));
    match language {
        Language::Pl | Language::En => format!("{day}.{month:02}"),
    }
}

fn show_plan(language: Language, show: &ShowFact, link: &str) -> MissionPlan {
    let when = date_label(language, show.starts_on);
    let place = show.city.as_deref().unwrap_or_default();
    let lang = match language {
        Language::Pl => "pl",
        Language::En => "en",
    };
    // Preserve why the recipient clicked all the way through the referral
    // resolver. The resolver still validates the event against this tenant's
    // published event cache before it will use the contextual destination.
    let link = format!("{link}?event={}&lang={lang}", show.slug);
    let (prompt, share) = match language {
        Language::Pl => (
            format!(
                "Znasz jedną osobę, którą zabrałbyś na {} ({when})?",
                show.title
            ),
            if place.is_empty() {
                format!("{} — {when}. Szczegóły: {link}", show.title)
            } else {
                format!("{} — {place}, {when}. Szczegóły: {link}", show.title)
            },
        ),
        Language::En => (
            format!("Is there one person you'd take to {} ({when})?", show.title),
            if place.is_empty() {
                format!("{} — {when}. Details: {link}", show.title)
            } else {
                format!("{} — {place}, {when}. Details: {link}", show.title)
            },
        ),
    };
    MissionPlan {
        kind: MissionKind::ShowOnePerson,
        event_id: Some(show.event_id),
        content_source_id: None,
        prompt,
        share_text: share,
    }
}

fn release_plan(language: Language, release: &ReleaseFact, link: &str) -> MissionPlan {
    let lang = match language {
        Language::Pl => "pl",
        Language::En => "en",
    };
    let link = format!(
        "{link}?release={}&lang={lang}",
        release.content_source_id
    );
    let (prompt, share) = match language {
        Language::Pl => (
            format!("Komu jednej osobie wysłałbyś „{}”?", release.title),
            format!("{} — posłuchaj: {link}", release.title),
        ),
        Language::En => (
            format!("Who is the one person you'd send \"{}\" to?", release.title),
            format!("{} — have a listen: {link}", release.title),
        ),
    };
    MissionPlan {
        kind: MissionKind::ReleaseOnePerson,
        event_id: None,
        content_source_id: Some(release.content_source_id),
        prompt,
        share_text: share,
    }
}

fn cooldown_for(yield_: AdvocacyYield) -> Duration {
    if yield_.completed_90d > 0 || yield_.human_clickers_90d >= 2 {
        return PROVEN_COOLDOWN;
    }
    if yield_.offered_90d >= 2 && yield_.tapped_90d == 0 {
        return QUIET_COOLDOWN;
    }
    if yield_.tapped_90d >= 2 && yield_.human_clickers_90d == 0 {
        return CLICKLESS_COOLDOWN;
    }
    COOLDOWN
}

/// Chooses at most one mission. A show in the person's own city outranks a
/// release (it is closer, more specific and has a date); among shows the
/// soonest unseen one wins; among releases the newest unseen one.
///
/// Cadence reacts only after repeated evidence. One quiet mission does not label
/// a person as low-yield, and even proven carriers never see the same fact twice.
#[must_use]
pub fn choose_mission(context: &MissionContext, now: OffsetDateTime) -> Option<MissionPlan> {
    if !context.may_carry || context.has_open_mission {
        return None;
    }
    let cooldown = cooldown_for(context.advocacy_yield);
    if context
        .last_offered_at
        .is_some_and(|at| now - at < cooldown)
    {
        return None;
    }
    let link = context.referral_url.as_deref()?;
    if let Some(show) = context
        .shows
        .iter()
        .filter(|s| {
            s.in_their_city
                && s.starts_at > now
                && s.starts_at - now <= SHOW_HORIZON
                && !context.seen_event_ids.contains(&s.event_id)
        })
        .min_by_key(|s| s.starts_at)
    {
        return Some(show_plan(context.language, show, link));
    }
    context
        .releases
        .iter()
        .filter(|r| {
            r.published_at <= now
                && now - r.published_at <= RELEASE_WINDOW
                && !context
                    .seen_content_source_ids
                    .contains(&r.content_source_id)
        })
        .max_by_key(|r| r.published_at)
        .map(|r| release_plan(context.language, r, link))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;
    use uuid::Uuid;

    const NOW: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

    fn show(days_ahead: i64, in_city: bool) -> ShowFact {
        let at = NOW + Duration::days(days_ahead);
        ShowFact {
            event_id: Uuid::now_v7(),
            slug: "virya-furydate-impala".into(),
            title: "Virya × Furydate × Impala".into(),
            city: Some("Gorzów Wielkopolski".into()),
            starts_on: at.date(),
            starts_at: at,
            in_their_city: in_city,
        }
    }

    fn release(days_ago: i64) -> ReleaseFact {
        ReleaseFact {
            content_source_id: Uuid::now_v7(),
            title: "Technophobia".into(),
            published_at: NOW - Duration::days(days_ago),
        }
    }

    fn context() -> MissionContext {
        MissionContext {
            may_carry: true,
            has_open_mission: false,
            last_offered_at: None,
            advocacy_yield: AdvocacyYield::default(),
            seen_event_ids: vec![],
            seen_content_source_ids: vec![],
            language: Language::Pl,
            referral_url: Some("https://virya.music/r/abc123".into()),
            shows: vec![show(15, true)],
            releases: vec![release(5)],
        }
    }

    #[test]
    fn a_show_in_their_own_city_is_one_question_and_one_text_to_send_on() {
        let plan = choose_mission(&context(), NOW).expect("a mission");
        assert_eq!(plan.kind, MissionKind::ShowOnePerson);
        assert!(plan.prompt.contains("jedną osobę"), "{}", plan.prompt);
        assert!(plan.prompt.ends_with("(17.10)?"), "{}", plan.prompt);
        assert!(plan.share_text.contains("Gorzów Wielkopolski"));
        assert!(plan.share_text.ends_with(
            "https://virya.music/r/abc123?event=virya-furydate-impala&lang=pl"
        ));
        assert!(plan.event_id.is_some() && plan.content_source_id.is_none());
    }

    #[test]
    fn the_wording_is_only_facts_no_hype_no_exclamation() {
        for language in [Language::Pl, Language::En] {
            let ctx = MissionContext {
                language,
                ..context()
            };
            let plan = choose_mission(&ctx, NOW).expect("a mission");
            assert!(!plan.prompt.contains('!') && !plan.share_text.contains('!'));
            assert!(!plan.share_text.contains('#'), "no hashtags");
            assert_eq!(
                plan.share_text.matches("http").count(),
                1,
                "exactly one link"
            );
        }
    }

    #[test]
    fn a_release_is_offered_when_no_show_is_close_and_real() {
        for shows in [
            vec![],
            vec![show(15, false)],
            vec![show(40, true)],
            vec![show(-2, true)],
        ] {
            let plan =
                choose_mission(&MissionContext { shows, ..context() }, NOW).expect("release");
            assert_eq!(plan.kind, MissionKind::ReleaseOnePerson);
            assert!(plan.share_text.contains("Technophobia"));
            assert!(plan.share_text.contains("?release="), "{}", plan.share_text);
            assert!(plan.share_text.ends_with("&lang=pl"), "{}", plan.share_text);
        }
    }

    #[test]
    fn no_fact_no_mission() {
        let nothing = MissionContext {
            shows: vec![],
            releases: vec![release(45)],
            ..context()
        };
        assert_eq!(choose_mission(&nothing, NOW), None);
        let no_link = MissionContext {
            referral_url: None,
            ..context()
        };
        assert_eq!(choose_mission(&no_link, NOW), None, "nothing to send on");
    }

    #[test]
    fn one_at_a_time_and_not_again_inside_the_cooldown() {
        let open = MissionContext {
            has_open_mission: true,
            ..context()
        };
        assert_eq!(choose_mission(&open, NOW), None);
        let recent = MissionContext {
            last_offered_at: Some(NOW - Duration::days(13)),
            ..context()
        };
        assert_eq!(choose_mission(&recent, NOW), None);
        let rested = MissionContext {
            last_offered_at: Some(NOW - Duration::days(15)),
            ..context()
        };
        assert!(choose_mission(&rested, NOW).is_some());
    }

    #[test]
    fn the_same_fact_is_never_asked_twice() {
        let show_ctx = context();
        let seen_show = MissionContext {
            seen_event_ids: vec![show_ctx.shows[0].event_id],
            seen_content_source_ids: vec![show_ctx.releases[0].content_source_id],
            ..show_ctx
        };
        assert_eq!(choose_mission(&seen_show, NOW), None);

        let release_ctx = MissionContext {
            shows: vec![],
            ..context()
        };
        let seen_release = MissionContext {
            seen_content_source_ids: vec![release_ctx.releases[0].content_source_id],
            ..release_ctx
        };
        assert_eq!(choose_mission(&seen_release, NOW), None);
    }

    #[test]
    fn measured_yield_changes_cadence_without_overreacting_to_one_attempt() {
        let one_quiet = MissionContext {
            last_offered_at: Some(NOW - Duration::days(15)),
            advocacy_yield: AdvocacyYield {
                offered_90d: 1,
                ..AdvocacyYield::default()
            },
            ..context()
        };
        assert!(choose_mission(&one_quiet, NOW).is_some(), "one miss keeps baseline cadence");

        let ignored_twice = MissionContext {
            last_offered_at: Some(NOW - Duration::days(30)),
            advocacy_yield: AdvocacyYield {
                offered_90d: 2,
                ..AdvocacyYield::default()
            },
            ..context()
        };
        assert_eq!(
            choose_mission(&ignored_twice, NOW),
            None,
            "two ignored asks back off to 45 days"
        );

        let clickless_twice = MissionContext {
            last_offered_at: Some(NOW - Duration::days(20)),
            advocacy_yield: AdvocacyYield {
                offered_90d: 2,
                tapped_90d: 2,
                ..AdvocacyYield::default()
            },
            ..context()
        };
        assert_eq!(
            choose_mission(&clickless_twice, NOW),
            None,
            "two taps with no human click back off to 30 days"
        );

        let proven = MissionContext {
            last_offered_at: Some(NOW - Duration::days(11)),
            advocacy_yield: AdvocacyYield {
                offered_90d: 2,
                tapped_90d: 2,
                human_clickers_90d: 1,
                completed_90d: 1,
            },
            ..context()
        };
        assert!(
            choose_mission(&proven, NOW).is_some(),
            "a proven carrier may get a different fact after 10 days"
        );
    }

    #[test]
    fn only_an_active_latarnik_who_may_carry_a_link_is_offered_anything() {
        let paused = MissionContext {
            may_carry: false,
            ..context()
        };
        assert_eq!(choose_mission(&paused, NOW), None);
    }

    #[test]
    fn the_soonest_show_and_the_newest_release_win() {
        let ctx = MissionContext {
            shows: vec![show(18, true), show(6, true), show(12, true)],
            ..context()
        };
        let plan = choose_mission(&ctx, NOW).expect("a mission");
        assert_eq!(plan.event_id, Some(ctx.shows[1].event_id));
        let ctx = MissionContext {
            shows: vec![],
            releases: vec![release(20), release(2), release(9)],
            ..context()
        };
        let plan = choose_mission(&ctx, NOW).expect("a mission");
        assert_eq!(
            plan.content_source_id,
            Some(ctx.releases[1].content_source_id)
        );
    }

    #[test]
    fn language_follows_the_fans_locale() {
        assert_eq!(Language::from_locale(Some("pl-PL")), Language::Pl);
        assert_eq!(Language::from_locale(Some("PL")), Language::Pl);
        assert_eq!(Language::from_locale(Some("en-GB")), Language::En);
        assert_eq!(Language::from_locale(None), Language::En);
    }
}
