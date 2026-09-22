//! Content trends — the deterministic detector over observation facts.
//!
//! Two tables feed it: `peer_observations` (what comparable acts
//! publish — the supply side) and `fan_observations` (what the
//! admitted communities' fans engaged with — the demand side). Both are raw
//! dated facts; this module groups them into patterns over the five trend
//! dimensions and scores each pattern by *corroboration*, not volume:
//!
//! - **Sources carry 60% of strength.** A pattern two distinct origins agree
//!   on outranks one a single loud source repeats — the "strongest at ≥2
//!   sources" rule. A peer plus a community is the strongest pairing there
//!   is: supply and demand agreeing.
//! - **Evidence carries 40%.** Count of facts behind the pattern, capped so
//!   a spammy source cannot buy strength by volume alone.
//! - **Status is lifecycle, not a score.** `confirmed` needs ≥2 distinct
//!   sources; a loud single-source pattern is `emerging`; a pattern that
//!   stops appearing is `faded` — fading is a fact the operator should see,
//!   not a row to delete.
//!
//! What this never does: rank the band's own content, read LLM output, or
//! decide what to make — detection stops at "this pattern is real, with
//! these rows proving it". Turning a confirmed trend into a suggestion is
//! the suggestion engine's job (3.5b.5), and it reads this table, not the
//! raw facts.

use std::collections::{BTreeMap, BTreeSet};

use crowdrelay_domain::content_engine::{TrendDimension, TrendStatus};
use time::Date;

/// Facts older than this no longer feed a trend — a stale pattern is a
/// faded one, which is the point of tracking `last_seen`.
pub const WINDOW_DAYS: i64 = 45;
/// A pattern needs at least this many facts before it is a claim at all.
pub const MIN_EVIDENCE: usize = 3;
/// A single-source pattern must be louder — corroboration cannot rescue it.
pub const MIN_SINGLE_SOURCE_EVIDENCE: usize = 5;
/// How many evidence ids per side a trend row carries.
pub const EVIDENCE_CAP: usize = 40;
/// Detector output cap — the tail beyond this is noise, not signal.
pub const MAX_TRENDS: usize = 16;

/// One observation as the detector needs it — the same shape from either
/// table. `source_key` is the origin identity (peer id or place id): two
/// facts from the same peer are one source no matter how loud.
#[derive(Debug, Clone)]
pub struct TrendFact<'a> {
    pub id: i64,
    /// `'peer'` or `'fan'` — which table the fact came from. The evidence
    /// JSONB groups ids under these keys.
    pub side: &'a str,
    /// Origin identity: the peer id or the discovery-place id.
    pub source_key: uuid::Uuid,
    pub observed_at: Date,
    pub platform: &'a str,
    pub fact: &'a str,
}

/// One detected pattern — the row the repository upserts.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedTrend {
    pub dimension: TrendDimension,
    pub pattern: String,
    /// 0..=10000 bp: 60% distinct sources, 40% evidence volume.
    pub strength: i32,
    pub sources: i32,
    /// `{"peer": [ids...], "fan": [ids...]}` — capped at `EVIDENCE_CAP` each.
    pub evidence: serde_json::Value,
    pub status: TrendStatus,
    pub first_seen: Date,
    pub last_seen: Date,
}

/// Lexicon entries: (needle, pattern key). Longest-first inside each
/// dimension so "lyric video" cannot also count as "video".
const FORMAT_LEXICON: &[(&str, &str)] = &[
    ("behind the scenes", "behind_the_scenes"),
    ("behind-the-scenes", "behind_the_scenes"),
    ("playthrough", "playthrough"),
    ("tour diary", "tour_diary"),
    ("studio diary", "studio_diary"),
    ("live session", "live_session"),
    ("lyric video", "lyric_video"),
    ("one take", "one_take"),
    ("one-take", "one_take"),
    ("gear rundown", "gear_rundown"),
    ("livestream", "livestream"),
    ("rehearsal", "rehearsal"),
    ("interview", "interview"),
    ("documentary", "documentary"),
    ("tutorial", "tutorial"),
    ("reaction", "reaction"),
    ("unboxing", "unboxing"),
    ("cover", "cover"),
    ("vlog", "vlog"),
    ("acoustic", "acoustic"),
];

const THEME_LEXICON: &[(&str, &str)] = &[
    ("tour", "tour"),
    ("album", "album"),
    ("single", "single"),
    ("vinyl", "vinyl"),
    ("merch", "merch"),
    ("festival", "festival"),
    ("presale", "presale"),
    ("ticket", "tickets"),
    ("studio", "studio"),
    ("video", "video"),
    ("live", "live"),
];

const STYLING_LEXICON: &[(&str, &str)] = &[
    ("black and white", "black_and_white"),
    ("black-and-white", "black_and_white"),
    ("cinematic", "cinematic"),
    ("stripped", "stripped"),
    ("handheld", "handheld"),
    ("lo-fi", "lo_fi"),
    ("lofi", "lo_fi"),
    ("raw", "raw"),
    ("diy", "diy"),
    ("drone", "drone"),
];

const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// Detects the live trends over the combined fact tail.
///
/// `today` anchors the window — anything older than `WINDOW_DAYS` is
/// history, not a trend. Output is sorted strongest-first and capped at
/// `MAX_TRENDS`; a fact feeding several patterns is fine — the dimensions
/// are views, not partitions.
#[must_use]
pub fn detect_trends(facts: &[TrendFact], today: Date) -> Vec<DetectedTrend> {
    let cutoff = today - time::Duration::days(WINDOW_DAYS);
    struct Group {
        peer_ids: Vec<i64>,
        fan_ids: Vec<i64>,
        sources: BTreeSet<uuid::Uuid>,
        first_seen: Date,
        last_seen: Date,
    }
    let mut groups: BTreeMap<(TrendDimension, String), Group> = BTreeMap::new();
    for fact in facts {
        if fact.observed_at < cutoff || fact.observed_at > today {
            continue;
        }
        for (dimension, pattern) in patterns_for(fact) {
            let group = groups.entry((dimension, pattern)).or_insert_with(|| Group {
                peer_ids: Vec::new(),
                fan_ids: Vec::new(),
                sources: BTreeSet::new(),
                first_seen: fact.observed_at,
                last_seen: fact.observed_at,
            });
            match fact.side {
                "fan" => group.fan_ids.push(fact.id),
                _ => group.peer_ids.push(fact.id),
            }
            group.sources.insert(fact.source_key);
            group.first_seen = group.first_seen.min(fact.observed_at);
            group.last_seen = group.last_seen.max(fact.observed_at);
        }
    }

    let mut trends: Vec<DetectedTrend> = groups
        .into_iter()
        .filter_map(|((dimension, pattern), group)| {
            let count = group.peer_ids.len() + group.fan_ids.len();
            let sources = group.sources.len();
            let status = if sources >= 2 && count >= MIN_EVIDENCE {
                TrendStatus::Confirmed
            } else if count >= MIN_SINGLE_SOURCE_EVIDENCE {
                TrendStatus::Emerging
            } else {
                return None;
            };
            // 60% corroboration (2000bp per source, capped) + 40% evidence
            // volume (400bp per fact, capped). Two sources agreeing on
            // three facts = 5200; a loud solo source tops out at 6000.
            let sources_bp = (2000 * sources as i32).min(6000);
            let evidence_bp = (400 * count as i32).min(4000);
            Some(DetectedTrend {
                dimension,
                pattern,
                strength: (sources_bp + evidence_bp).min(10_000),
                sources: sources as i32,
                evidence: serde_json::json!({
                    "peer": &group.peer_ids[..group.peer_ids.len().min(EVIDENCE_CAP)],
                    "fan": &group.fan_ids[..group.fan_ids.len().min(EVIDENCE_CAP)],
                }),
                status,
                first_seen: group.first_seen,
                last_seen: group.last_seen,
            })
        })
        .collect();
    trends.sort_by(|a, b| {
        b.strength
            .cmp(&a.strength)
            .then_with(|| a.pattern.cmp(&b.pattern))
    });
    trends.truncate(MAX_TRENDS);
    trends
}

/// The (dimension, pattern) keys one fact feeds. A fact can feed several —
/// "released a playthrough video" is format *and* theme evidence.
fn patterns_for(fact: &TrendFact) -> Vec<(TrendDimension, String)> {
    let mut out = Vec::new();
    // Platform is the surface, not the words — always exactly one.
    let platform = fact.platform.trim().to_lowercase();
    if !platform.is_empty() {
        out.push((TrendDimension::Platform, platform));
    }
    // Timing is the weekday the fact happened on — cadence patterns like
    // "friday drops" are real scheduling intelligence.
    let weekday = fact.observed_at.weekday();
    out.push((
        TrendDimension::Timing,
        WEEKDAYS[weekday.number_from_monday() as usize - 1].to_owned(),
    ));
    let mut remaining = fact.fact.to_lowercase();
    for (lexicon, dimension) in [
        (FORMAT_LEXICON, TrendDimension::Format),
        (THEME_LEXICON, TrendDimension::Theme),
        (STYLING_LEXICON, TrendDimension::Styling),
    ] {
        for (needle, key) in lexicon {
            // Positions are collected before mutating; the blanking is
            // length-preserving so earlier matches never shift later ones.
            for (at, _) in remaining.match_indices(needle).collect::<Vec<_>>() {
                let end = at + needle.len();
                let bytes = remaining.as_bytes();
                // Word boundaries, with 's' allowed after — "videos" is
                // still "video", but "deliver" is never "live" and
                // "discover" is never "cover".
                let bounded_left = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
                let bounded_right =
                    end >= bytes.len() || bytes[end] == b's' || !bytes[end].is_ascii_alphanumeric();
                if !bounded_left || !bounded_right {
                    continue;
                }
                // The blanked span is shared across lexicons, so "lyric
                // video" (format) cannot also count as theme "video" —
                // one phrase is one interpretation.
                remaining.replace_range(at..end, &" ".repeat(needle.len()));
                out.push((dimension, (*key).to_owned()));
                break;
            }
        }
    }
    // Alias pairs ("lo-fi"/"lofi" → lo_fi) can push one pattern twice for
    // a single fact — the count is a fact count, not a match count.
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use time::Month;

    /// Each fixture source gets its own id — `from_u128` keeps the
    /// workspace's v7-only convention from needing a feature flag.
    static SOURCE: AtomicU64 = AtomicU64::new(1);

    fn date(day: u8) -> Date {
        Date::from_calendar_date(2026, Month::September, day).expect("date")
    }

    fn fact<'a>(
        id: i64,
        side: &'a str,
        source: uuid::Uuid,
        day: u8,
        text: &'a str,
    ) -> TrendFact<'a> {
        TrendFact {
            id,
            side,
            source_key: source,
            observed_at: date(day),
            platform: "youtube",
            fact: text,
        }
    }

    #[test]
    fn two_sources_agreeing_confirms_the_trend() {
        let peer_a = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128,
        );
        let peer_b = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128,
        );
        let facts = vec![
            fact(1, "peer", peer_a, 1, "new playthrough up"),
            fact(2, "peer", peer_b, 3, "playthrough of the single"),
            fact(3, "peer", peer_a, 5, "another playthrough"),
        ];
        let trends = detect_trends(&facts, date(20));
        let playthrough = trends
            .iter()
            .find(|t| t.dimension == TrendDimension::Format && t.pattern == "playthrough")
            .expect("playthrough detected");
        assert_eq!(playthrough.status, TrendStatus::Confirmed);
        assert_eq!(playthrough.sources, 2);
        assert_eq!(playthrough.evidence["peer"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn one_loud_source_stays_emerging() {
        let peer = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let facts: Vec<_> = (0..6)
            .map(|i| fact(i, "peer", peer, 2, "weekly vlog"))
            .collect();
        let trends = detect_trends(&facts, date(20));
        let vlog = trends
            .iter()
            .find(|t| t.pattern == "vlog")
            .expect("vlog detected");
        assert_eq!(vlog.status, TrendStatus::Emerging);
        assert_eq!(vlog.sources, 1);
    }

    #[test]
    fn thin_evidence_is_no_trend() {
        let a = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let b = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let facts = vec![
            fact(1, "peer", a, 2, "drone footage of the venue"),
            fact(2, "peer", b, 3, "drone shots"),
        ];
        let trends = detect_trends(&facts, date(20));
        assert!(
            !trends.iter().any(|t| t.pattern == "drone"),
            "two facts cannot carry a claim"
        );
    }

    #[test]
    fn stale_facts_fall_out_of_the_window() {
        let peer = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let facts: Vec<_> = (0..6).map(|i| fact(i, "peer", peer, 1, "vlog")).collect();
        // 60 days later the same facts are history.
        let later = date(1) + time::Duration::days(60);
        assert!(detect_trends(&facts, later).is_empty());
    }

    #[test]
    fn longest_lexicon_match_wins() {
        let peer = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let other = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128,
        );
        let facts = vec![
            fact(1, "peer", peer, 2, "our lyric video is out"),
            fact(2, "peer", other, 3, "new lyric video"),
            fact(3, "peer", peer, 4, "lyric video premiere"),
        ];
        let trends = detect_trends(&facts, date(20));
        assert!(trends.iter().any(|t| t.pattern == "lyric_video"));
        assert!(
            !trends
                .iter()
                .any(|t| t.dimension == TrendDimension::Theme && t.pattern == "video"),
            "'lyric video' must not double-count as theme 'video'"
        );
    }

    #[test]
    fn fan_side_evidence_lands_under_its_own_key() {
        let peer = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let place = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128,
        );
        let facts = vec![
            fact(1, "peer", peer, 2, "playthrough drop"),
            fact(2, "fan", place, 3, "that playthrough is insane"),
            fact(3, "peer", peer, 4, "playthrough part two"),
        ];
        let trends = detect_trends(&facts, date(20));
        let t = trends
            .iter()
            .find(|t| t.pattern == "playthrough")
            .expect("detected");
        // Peer + community corroboration — the strongest pairing there is.
        assert_eq!(t.status, TrendStatus::Confirmed);
        assert_eq!(t.evidence["fan"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn weekday_becomes_a_timing_pattern() {
        // 2026-09-04 is a Friday.
        let a = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let b = uuid::Uuid::from_u128(
            SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        );
        let facts = vec![
            fact(1, "peer", a, 4, "out now"),
            fact(2, "peer", b, 4, "premiered today"),
            fact(3, "peer", a, 4, "link in bio"),
        ];
        let trends = detect_trends(&facts, date(20));
        assert!(
            trends
                .iter()
                .any(|t| t.dimension == TrendDimension::Timing && t.pattern == "friday")
        );
    }
}
