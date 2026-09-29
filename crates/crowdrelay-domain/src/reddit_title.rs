//! Rule-aware title composition for Reddit posts.
//!
//! Metal subreddits enforce a `TITLE_REGEX` at submit time — "Artist - Song",
//! "(Year)", "[genre]" or some combination — and a draft that violates it is
//! rejected before a moderator ever sees it. The drafter writes one title per
//! video; this module rewrites it per community from the community's own
//! `rules_summary` (the " || "-joined rule titles the rules sweep stores).
//!
//! Two jobs, both pure:
//!
//! - [`split_draft_title`] pulls the draft apart into the band prefix, the
//!   track and any trailing parenthetical/bracket run, so `title_for` can
//!   rebuild it in the order the rules ask for.
//! - [`title_for`] returns the compliant title, or `None` when the rules
//!   carry no title format at all — a ruleless community keeps the drafted
//!   wording rather than a rebuilt one.
//!
//! What is NOT here: flair selection, link-kind rules (mobile-link bans) and
//! cooldowns — those live in `community_rules.rs` and the executor's claim
//! predicates. This module only composes the string.

/// A draft title decomposed. `descriptor` is the trailing "(Live From FLSS
/// 2026)"-style tail, kept whole so subs that want "Artist - Title" still
/// get the context word and subs that demand a bare "(Year)" can drop it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftParts {
    /// The track name with the band prefix and descriptor tail removed.
    pub track: String,
    /// Everything after the track — parenthetical and bracket groups,
    /// joined and re-bracketed as written. `None` when the draft is bare.
    pub descriptor: Option<String>,
    /// A bare 4-digit year found inside the descriptor, if any — "(2026)"
    /// counts, "(Live From FLSS 2026)" does not.
    pub year: Option<i32>,
}

/// What a community's `rules_summary` asks a title to be. All three flags
/// feed `title_for`; `format` alone decides whether a rewrite happens.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TitleRequirements {
    /// The rules name an "Artist - Song"-style format.
    pub format: bool,
    /// The format includes a "(Year)" slot.
    pub year: bool,
    /// The format includes a "[genre]" slot.
    pub genre: bool,
}

/// Folds typographic dashes (U+2012 figure dash, U+2013 en dash, U+2014 em
/// dash, U+2212 minus sign) to ASCII `-` and trims. Wire-safe and safe for
/// rule matching: sub rules quote their format with ASCII hyphens.
#[must_use]
pub fn ascii_dashes(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Reads the title format a `rules_summary` declares. Matches the phrasings
/// the admitted subs actually use — "Artist - Song", "Artist - Title",
/// "Artist Name - Song Title", "artist and song name in the title" — plus
/// the "(Year)" and "[genre]" slots the stricter ones add. Anything else is
/// no format: a community that never mentions titles is not flagged, since
/// a guessed format would rewrite perfectly legal drafts.
#[must_use]
pub fn title_requirements(rules_summary: &str) -> TitleRequirements {
    let text = ascii_dashes(rules_summary).to_lowercase();
    let format = [
        "artist - song",
        "artist - title",
        "artist - track",
        "artist name - song",
        "artist name - title",
        "band - song",
        "band - title",
        "artist and song",
        "artist and track",
        "artist and title",
        "\"artist\" format",
        "artist/title format",
        "title format",
    ]
    .iter()
    .any(|pattern| text.contains(pattern));
    TitleRequirements {
        format,
        year: text.contains("(year)") || text.contains("(yyyy)"),
        genre: text.contains("[genre]") || text.contains("(genre)"),
    }
}

/// Builds the title a community's rules accept, or `None` when the summary
/// declares no title format — the caller keeps the drafted title then.
///
/// - `track` may carry its descriptor tail ("Technophobia (Live From FLSS
///   2026)"): kept when the rules don't demand a bare "(Year)", dropped to
///   the bare track when they do — "(Live From FLSS 2026) (2026)" fails the
///   regex it was meant to satisfy.
/// - `year`/`genre` fill the slots only when the rules name them; a required
///   slot with no value is omitted rather than invented — a wrong genre tag
///   is worse than a missing one.
#[must_use]
pub fn title_for(
    rules_summary: Option<&str>,
    band: &str,
    track: &str,
    year: Option<i32>,
    genre: Option<&str>,
) -> Option<String> {
    let requirements = title_requirements(rules_summary?);
    if !requirements.format {
        return None;
    }
    let track = if requirements.year {
        strip_trailing_groups(track)
    } else {
        track.to_owned()
    };
    let mut title = format!("{} - {}", ascii_dashes(band), ascii_dashes(&track));
    if requirements.year {
        if let Some(year) = year {
            title.push_str(&format!(" ({year})"));
        }
    }
    if requirements.genre {
        if let Some(genre) = genre.map(str::trim).filter(|g| !g.is_empty()) {
            title.push_str(&format!(" [{genre}]"));
        }
    }
    Some(title)
}

/// Splits a drafted title into the parts `title_for` rebuilds. The draft is
/// expected to be "Band - Track (descriptor) [tag]"-shaped, which is what
/// the engager drafts; a draft without the band prefix or any descriptor
/// still splits cleanly — the whole string is the track.
#[must_use]
pub fn split_draft_title(draft: &str, band: &str) -> DraftParts {
    let folded = ascii_dashes(draft);
    let folded_band = ascii_dashes(band);
    // The band prefix is "Virya - " in whatever case the drafter used;
    // compare case-insensitively so "VIRYA – Rise" splits the same as
    // "Virya - Rise". `.get` keeps a non-boundary band length from slicing
    // mid-character, and the dash is matched on the already-folded string.
    let body = if folded_band.is_empty() {
        folded.clone()
    } else {
        match folded.get(..folded_band.len()) {
            Some(head) if head.to_lowercase() == folded_band.to_lowercase() => folded
                .get(folded_band.len()..)
                .unwrap_or("")
                .trim_start()
                .strip_prefix('-')
                .map(str::trim)
                .map(str::to_owned)
                .unwrap_or_else(|| folded.clone()),
            _ => folded.clone(),
        }
    };
    // Everything from the first '(' or '[' on is the descriptor tail.
    // Titles like "(YouTube) Band - Song" start with a bracket — the split
    // would eat the whole title, so only a tail preceded by a track-sized
    // head counts.
    let tail_at = body.find(['(', '[']);
    let (track, descriptor) = match tail_at {
        Some(at) if !body[..at].trim().is_empty() => (
            body[..at].trim().to_owned(),
            Some(body[at..].trim().to_owned()),
        ),
        _ => (body, None),
    };
    let year = descriptor.as_deref().and_then(|tail| {
        tail.trim_start_matches(['(', '['])
            .trim_end_matches([')', ']'])
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|y| (1900..=2100).contains(y))
    });
    DraftParts {
        track,
        descriptor,
        year,
    }
}

/// Removes every trailing `(...)`/`[...]` group from a track string, so a
/// "(Year)"-enforcing sub gets the bare track plus its own slot.
fn strip_trailing_groups(track: &str) -> String {
    track
        .find(['(', '['])
        .map(|at| track[..at].trim().to_owned())
        .filter(|head| !head.is_empty())
        .unwrap_or_else(|| track.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const METALCORE: &str = "\"Artist - Song\" Format and Informative Titles || \
        Direct image/video link || Self-promotional spam || No Full Album or Spotify Posts";
    const NUMETAL: &str = "Rule 1: \"Artist - Song (Year)\" Format and Informative Titles || \
        Rule 2: Images &amp; Videos / Memes / Low Effort / AI";
    const POSTHARDCORE: &str = "\"Artist - Title\" format || No mobile links || \
        Keep self-promotion to a minimum";
    const THRASHMETAL: &str = "Please post in a \"Artist Name - Song Title\" format || \
        No bands submitted in the last 2 weeks || No obnoxious self-promotion";
    const DJENT: &str = "Post flair is required || ''Artist - Song Title'' is required \
        for finished music || Self-promotion is allowed, but must be marked a such";
    const DEATHCORE: &str = "Please put the artist and song name in the title || \
        No mobile links || Limit your self-promotion";
    const NO_FORMAT: &str = "No hate speech || No racism || No reposts";

    #[test]
    fn no_format_rules_keep_the_draft() {
        assert_eq!(
            title_for(Some(NO_FORMAT), "Virya", "Technophobia", Some(2026), None),
            None
        );
        assert_eq!(title_for(None, "Virya", "Rise", None, None), None);
    }

    #[test]
    fn artist_title_subs_get_band_dash_track_and_keep_the_descriptor() {
        for rules in [METALCORE, POSTHARDCORE, THRASHMETAL, DJENT, DEATHCORE] {
            let title = title_for(
                Some(rules),
                "Virya",
                "Technophobia (Live From FLSS 2026)",
                Some(2026),
                None,
            )
            .unwrap_or_else(|| panic!("{rules} should declare a format"));
            assert_eq!(
                title, "Virya - Technophobia (Live From FLSS 2026)",
                "{rules}"
            );
        }
    }

    #[test]
    fn a_year_slot_drops_the_descriptor_to_its_own_paren() {
        let title = title_for(
            Some(NUMETAL),
            "Virya",
            "Technophobia (Live From FLSS 2026)",
            Some(2026),
            None,
        );
        assert_eq!(title.as_deref(), Some("Virya - Technophobia (2026)"));
    }

    #[test]
    fn a_genre_slot_appends_the_tag_when_known_and_omits_it_when_not() {
        let rules = "\"Artist - Song [Genre]\" format required";
        assert_eq!(
            title_for(Some(rules), "Virya", "Rise", None, Some("death metal")).as_deref(),
            Some("Virya - Rise [death metal]")
        );
        assert_eq!(
            title_for(Some(rules), "Virya", "Rise", None, None).as_deref(),
            Some("Virya - Rise")
        );
    }

    #[test]
    fn every_requirement_stacks_in_order() {
        let rules = "Titles must be \"Artist - Song (Year) [Genre]\"";
        let title = title_for(
            Some(rules),
            "Virya",
            "Technophobia",
            Some(2026),
            Some("metalcore"),
        );
        assert_eq!(
            title.as_deref(),
            Some("Virya - Technophobia (2026) [metalcore]")
        );
    }

    #[test]
    fn typographic_dashes_fold_everywhere() {
        let title = title_for(
            Some(DJENT),
            "Virya",
            "Technophobia \u{2013} live",
            None,
            None,
        );
        assert_eq!(title.as_deref(), Some("Virya - Technophobia - live"));
    }

    #[test]
    fn split_reads_band_prefix_descriptor_and_year() {
        let parts = split_draft_title("Virya \u{2013} Technophobia (Live From FLSS 2026)", "Virya");
        assert_eq!(
            parts,
            DraftParts {
                track: "Technophobia".to_owned(),
                descriptor: Some("(Live From FLSS 2026)".to_owned()),
                year: None,
            }
        );
        let parts = split_draft_title("Virya - Technophobia (2026)", "virya");
        assert_eq!(parts.track, "Technophobia");
        assert_eq!(parts.year, Some(2026));
    }

    #[test]
    fn split_leaves_a_bandless_or_bare_title_as_the_track() {
        let parts = split_draft_title("Technophobia Live From FLSS 2026", "Virya");
        assert_eq!(parts.track, "Technophobia Live From FLSS 2026");
        let parts = split_draft_title(
            "Virya \u{2013} Rise (Live in Namys\u{0142}\u{00f3}w) [metal]",
            "Virya",
        );
        assert_eq!(parts.track, "Rise");
        assert_eq!(
            parts.descriptor.as_deref(),
            Some("(Live in Namys\u{0142}\u{00f3}w) [metal]")
        );
    }
}
