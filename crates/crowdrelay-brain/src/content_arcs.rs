//! Deterministic arc proposals — the campaign shape the band approves once.
//!
//! An arc is the unit that turns a stream of suggestions into a plan: a
//! horizon, a spine of dated beats, and the evidence that argues for it.
//! The engine proposes; the band approves the arc, not every step in it.
//! Proposal is deliberately conservative — an arc exists to point at real
//! material, so a band with nothing upcoming gets no campaign invented for
//! it, and a proposal never names a format the roster cannot do.

use crowdrelay_domain::content_engine::{ContentFormatEntry, FormatPurpose, FormatRequirement};
use serde_json::{Value as JsonValue, json};
use time::Date;

/// The soonest an arc may start — a campaign needs lead time; a beat due
/// tomorrow is a suggestion's job, not an arc's.
const MIN_LEAD_DAYS: i64 = 7;
/// The furthest an anchor may sit — past twelve weeks a campaign is a
/// horizon, not a plan.
const MAX_ANCHOR_DAYS: i64 = 84;
/// Beats per spine — enough to tell the story, few enough to read in a
/// glance.
const MAX_BEATS: usize = 5;
/// Post-anchor tail: the aftermovie and the "we did it" beat live in the
/// week after the thing they cover.
const TAIL_DAYS: i64 = 7;

/// What an arc points at — the material the campaign exists for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArcAnchorKind {
    Release,
    Show,
    /// A scheduled shoot or production day — material that feeds many beats.
    ProductionEvent,
}

impl ArcAnchorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Show => "show",
            Self::ProductionEvent => "production_event",
        }
    }
}

/// A dated thing worth building a campaign around.
#[derive(Clone, Debug)]
pub struct ArcAnchor {
    pub kind: ArcAnchorKind,
    /// The persisted row id — evidence names what it rests on.
    pub id: String,
    pub name: String,
    pub date: Date,
}

/// One trend's support for the arc: which catalogue keys it lifts and the
/// trend row ids that carry the claim.
#[derive(Clone, Debug)]
pub struct ArcTrendSupport {
    /// Catalogue format keys this trend lifts (the alias bridge the ranker
    /// already applies — the caller passes bridged keys).
    pub format_keys: Vec<String>,
    /// `content_trends.id`s — the arc's evidence names them so the
    /// band can check the claim instead of trusting it.
    pub trend_ids: Vec<String>,
}

/// Everything a proposal decision is allowed to see.
pub struct ArcInputs<'a> {
    /// Candidate anchors, any order — filtered to [MIN_LEAD, MAX] days out.
    pub anchors: &'a [ArcAnchor],
    /// Catalogue entries the band can actually do — the caller has already
    /// applied the capability gate; a proposal never re-litigates it.
    pub feasible_formats: &'a [ContentFormatEntry],
    /// Corroborated trends, pre-bridged to catalogue keys.
    pub trend_support: &'a [ArcTrendSupport],
    /// Anchor ids a recently-retired arc already covered — a "no" the band
    /// gave inside the cooldown is still a no.
    pub declined_anchor_ids: &'a [String],
    pub today: Date,
}

/// The arc the engine would put in front of the band.
#[derive(Clone, Debug)]
pub struct ProposedArc {
    pub title: String,
    pub summary: String,
    pub horizon_start: Date,
    pub horizon_end: Date,
    /// `[{"at": "YYYY-MM-DD", "beat": "<concept>", "format_key": "<key>"}]` —
    /// dates are intentions the spine documents, not commitments.
    pub spine: JsonValue,
    /// `{anchor: {...}, trend_ids: [...]}` — the band approves because of
    /// this; it must survive audit without a join.
    pub evidence: JsonValue,
}

/// The purpose order a spine tells: reach first, then the ask, then the
/// proof, then keeping what was earned. A campaign that opens with
/// conversion has nobody to convert.
const SPINE_PURPOSE_ORDER: [FormatPurpose; 4] = [
    FormatPurpose::Acquisition,
    FormatPurpose::Conversion,
    FormatPurpose::Credibility,
    FormatPurpose::Retention,
];

fn purpose_rank(purpose: FormatPurpose) -> usize {
    SPINE_PURPOSE_ORDER
        .iter()
        .position(|candidate| *candidate == purpose)
        .unwrap_or(SPINE_PURPOSE_ORDER.len())
}

/// Whether a format can anchor around this kind of material. A format that
/// `requires` a release only exists inside a release arc; `Nothing` formats
/// are cadence beats and fit anywhere.
fn fits_anchor(entry: &ContentFormatEntry, anchor: ArcAnchorKind) -> bool {
    match (entry.requires, anchor) {
        (FormatRequirement::Nothing, _) => true,
        (FormatRequirement::Release, ArcAnchorKind::Release) => true,
        (FormatRequirement::Show, ArcAnchorKind::Show) => true,
        // A production day is material, not a story: only cadence beats hang
        // off it — the release or show the shoot feeds is the real anchor.
        (_, ArcAnchorKind::ProductionEvent) => false,
        _ => false,
    }
}

/// Propose the arc the evidence supports, or nothing when it supports none.
///
/// The spine is built from the anchor outward: the purpose order gives the
/// story its shape, corroborated trends get first claim on the slots (a
/// beat the world is already asking for belongs in the plan), and dates
/// spread across the horizon ending just after the anchor. Fewer than two
/// feasible formats is a playlist, not an arc — refused.
pub fn propose_arc(inputs: &ArcInputs<'_>) -> Option<ProposedArc> {
    let today = inputs.today;
    let anchor = inputs
        .anchors
        .iter()
        .filter(|candidate| {
            let lead = (candidate.date - today).whole_days();
            (MIN_LEAD_DAYS..=MAX_ANCHOR_DAYS).contains(&lead)
                && !inputs
                    .declined_anchor_ids
                    .iter()
                    .any(|declined| declined == &candidate.id)
        })
        .min_by_key(|candidate| candidate.date)?;

    // Corroboration first: a format the peers and the fans are already
    // proving belongs ahead of one that is merely feasible.
    let mut lifted: Vec<&ContentFormatEntry> = Vec::new();
    for support in inputs.trend_support {
        for key in &support.format_keys {
            if lifted.iter().any(|entry| entry.key == *key) {
                continue;
            }
            if let Some(entry) = inputs
                .feasible_formats
                .iter()
                .find(|entry| &entry.key == key && fits_anchor(entry, anchor.kind))
            {
                lifted.push(entry);
            }
        }
    }
    let mut rest: Vec<&ContentFormatEntry> = inputs
        .feasible_formats
        .iter()
        .filter(|entry| {
            fits_anchor(entry, anchor.kind) && !lifted.iter().any(|chosen| chosen.key == entry.key)
        })
        .collect();
    rest.sort_by_key(|entry| (purpose_rank(entry.purpose), entry.key.clone()));

    let beats: Vec<&ContentFormatEntry> = lifted.into_iter().chain(rest).take(MAX_BEATS).collect();
    if beats.len() < 2 {
        return None;
    }

    let horizon_start = today + time::Duration::days(1);
    let horizon_end = anchor.date + time::Duration::days(TAIL_DAYS);
    let span = (horizon_end - horizon_start).whole_days().max(1);
    let spine: Vec<JsonValue> = beats
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            // Beats land at even fractions of the horizon — the first beat
            // inside the first fifth, the last inside the last. Dates are
            // intentions; the week view turns them into deadlines later.
            let at = horizon_start
                + time::Duration::days(span * (index as i64 + 1) / (beats.len() as i64 + 1));
            json!({
                "at": at.to_string(),
                "beat": entry.name,
                "format_key": entry.key,
            })
        })
        .collect();

    // Evidence claims only what the spine uses: a trend lifts the arc when
    // one of its formats made a beat. Naming every live trend would corro-
    // borate formats the plan does not contain — the evidence has to survive
    // the audit it invites.
    let spine_keys: Vec<&str> = beats.iter().map(|entry| entry.key.as_str()).collect();
    let trend_ids: Vec<String> = inputs
        .trend_support
        .iter()
        .filter(|support| {
            support
                .format_keys
                .iter()
                .any(|key| spine_keys.contains(&key.as_str()))
        })
        .flat_map(|support| support.trend_ids.iter().cloned())
        .collect();
    Some(ProposedArc {
        title: format!("{}: {}", anchor.name, anchor.kind.as_str()),
        summary: format!(
            "A {}-beat run at {} — {} to {}.",
            beats.len(),
            anchor.name,
            horizon_start,
            horizon_end
        ),
        horizon_start,
        horizon_end,
        spine: JsonValue::Array(spine),
        evidence: json!({
            "anchor": {
                "kind": anchor.kind.as_str(),
                "id": anchor.id,
                "name": anchor.name,
                "date": anchor.date.to_string(),
            },
            "trend_ids": trend_ids,
            "format_keys": beats.iter().map(|entry| entry.key.clone()).collect::<Vec<_>>(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::content_engine::{Effort, FormatCadence, FormatCategory};

    fn day(year: i32, month: u8, day: u8) -> Date {
        Date::from_calendar_date(year, month.try_into().unwrap(), day).unwrap()
    }

    fn entry(key: &str, purpose: FormatPurpose, requires: FormatRequirement) -> ContentFormatEntry {
        ContentFormatEntry {
            key: key.to_owned(),
            name: key.to_owned(),
            category: FormatCategory::Release,
            purpose,
            effort_standalone: Effort::Medium,
            effort_marginal: Effort::Low,
            skill: crowdrelay_domain::team_operations::TeamSkill::Video,
            requires,
            distribution: "communities".to_owned(),
            cadence: FormatCadence::OneOff,
            genre_fit: Vec::new(),
            notes: String::new(),
            active: true,
        }
    }

    fn release_anchor(days_out: i64, today: Date) -> ArcAnchor {
        ArcAnchor {
            kind: ArcAnchorKind::Release,
            id: "rel-1".to_owned(),
            name: "Single".to_owned(),
            date: today + time::Duration::days(days_out),
        }
    }

    #[test]
    fn no_anchor_means_no_arc() {
        let today = day(2026, 10, 1);
        let formats = vec![
            entry(
                "playthrough",
                FormatPurpose::Acquisition,
                FormatRequirement::Release,
            ),
            entry(
                "teaser",
                FormatPurpose::Acquisition,
                FormatRequirement::Nothing,
            ),
        ];
        let inputs = ArcInputs {
            anchors: &[],
            feasible_formats: &formats,
            trend_support: &[],
            declined_anchor_ids: &[],
            today,
        };
        assert!(
            propose_arc(&inputs).is_none(),
            "a band with nothing upcoming gets no campaign invented for it"
        );
    }

    #[test]
    fn a_release_anchor_builds_a_dated_spine_from_capable_formats() {
        let today = day(2026, 10, 1);
        let formats = vec![
            entry(
                "teaser",
                FormatPurpose::Acquisition,
                FormatRequirement::Nothing,
            ),
            entry(
                "playthrough",
                FormatPurpose::Acquisition,
                FormatRequirement::Release,
            ),
            entry(
                "aftermovie",
                FormatPurpose::Retention,
                FormatRequirement::Show,
            ),
            entry(
                "making_of",
                FormatPurpose::Credibility,
                FormatRequirement::Nothing,
            ),
        ];
        let anchor = release_anchor(30, today);
        let inputs = ArcInputs {
            anchors: std::slice::from_ref(&anchor),
            feasible_formats: &formats,
            trend_support: &[ArcTrendSupport {
                format_keys: vec!["making_of".to_owned()],
                trend_ids: vec!["trend-9".to_owned()],
            }],
            declined_anchor_ids: &[],
            today,
        };
        let arc = propose_arc(&inputs).expect("an arc");
        let spine = arc.spine.as_array().expect("spine");
        assert_eq!(spine.len(), 3, "aftermovie is a show format — off this arc");
        // The corroborated format leads; every beat sits inside the horizon.
        assert_eq!(spine[0]["format_key"], "making_of");
        for beat in spine {
            let at = Date::parse(
                beat["at"].as_str().unwrap(),
                &time::format_description::well_known::Iso8601::DEFAULT,
            )
            .unwrap();
            assert!(at >= arc.horizon_start && at <= arc.horizon_end);
        }
        assert_eq!(arc.evidence["anchor"]["id"], "rel-1");
        assert_eq!(arc.evidence["trend_ids"][0], "trend-9");
        assert_eq!(arc.horizon_end, anchor.date + time::Duration::days(7));
    }

    #[test]
    fn one_format_is_a_playlist_not_an_arc() {
        let today = day(2026, 10, 1);
        let formats = vec![entry(
            "teaser",
            FormatPurpose::Acquisition,
            FormatRequirement::Nothing,
        )];
        let anchor = release_anchor(30, today);
        let inputs = ArcInputs {
            anchors: &[anchor],
            feasible_formats: &formats,
            trend_support: &[],
            declined_anchor_ids: &[],
            today,
        };
        assert!(propose_arc(&inputs).is_none());
    }

    #[test]
    fn a_declined_anchor_stays_declined_inside_the_cooldown() {
        let today = day(2026, 10, 1);
        let formats = vec![
            entry(
                "teaser",
                FormatPurpose::Acquisition,
                FormatRequirement::Nothing,
            ),
            entry(
                "playthrough",
                FormatPurpose::Acquisition,
                FormatRequirement::Release,
            ),
        ];
        let anchor = release_anchor(30, today);
        let inputs = ArcInputs {
            anchors: &[anchor],
            feasible_formats: &formats,
            trend_support: &[],
            declined_anchor_ids: &["rel-1".to_owned()],
            today,
        };
        assert!(
            propose_arc(&inputs).is_none(),
            "a 'no' the band gave inside the cooldown is still a no"
        );
    }

    #[test]
    fn anchors_too_soon_or_too_far_are_not_arc_material() {
        let today = day(2026, 10, 1);
        let formats = vec![
            entry(
                "teaser",
                FormatPurpose::Acquisition,
                FormatRequirement::Nothing,
            ),
            entry(
                "playthrough",
                FormatPurpose::Acquisition,
                FormatRequirement::Release,
            ),
        ];
        for days in [3_i64, 100_i64] {
            let anchor = release_anchor(days, today);
            let inputs = ArcInputs {
                anchors: &[anchor],
                feasible_formats: &formats,
                trend_support: &[],
                declined_anchor_ids: &[],
                today,
            };
            assert!(
                propose_arc(&inputs).is_none(),
                "{days}d out is a suggestion's job or a horizon, not an arc"
            );
        }
    }
}
