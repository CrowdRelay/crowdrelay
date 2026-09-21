//! The suggestion engine — which two or three beats are worth the band's
//! time this cycle.
//!
//! This is a pure ranker: everything measurable arrives as inputs, and the
//! output is a short ordered list of `ScoredSuggestion`s. The repository
//! layer gathers the inputs; the worker persists the result. No LLM is
//! involved — a suggestion is a deterministic claim, and its evidence is
//! the rows that produced it.
//!
//! The pipeline, in the order §4b specifies:
//!
//! 1. **Capability filter.** `capability_gap` decides what the band can
//!    execute at all — a `video` format with no filmmaker never ranks,
//!    whatever the trend says.
//! 2. **Trend lift.** A `format`-dimension trend matching the entry's key
//!    multiplies its expected fans — peers proving a format works is the
//!    strongest evidence available before the band has outcomes of its own.
//! 3. **EFE plus band-hour cost.** `efe.rs` scores value and learning; the
//!    cost term is denominated in the band's own hours, so a near-free beat
//!    under a scheduled production day outranks a whole extra day unless
//!    the second is dramatically better (§4b-3: marginal, not absolute).
//! 4. **Arc lift and refusal.** A beat the band already approved into an
//!    active arc's spine outranks an equivalent orphan — the arc is the
//!    shape the band chose; suggestions fill it, they do not redraw it.
//!    While an arc runs, a format outside its spine is refused outright
//!    unless a production deadline (`suggested_before`) makes it urgent
//!    and time-boxed — §4b-4's "no suggestion without a current arc".
//! 5. **Rank, then cut (§4b-4).** The engine emits the top `limit` (2–3),
//!    not everything above a threshold. The tail is named in the caller's
//!    summary, not offered.
//! 6. **Distribution promise.** Every survivor carries its promise
//!    assembled from live data — admitted communities by name, press
//!    contacts and consented fans by count. A promise with no real clause
//!    is a suggestion not worth raising: the engine declines it.

use std::collections::{BTreeMap, BTreeSet};

use crowdrelay_domain::content_engine::{
    CapabilityProfile, ContentFormatEntry, ContentTrend, Effort, FormatPurpose,
    ProductionEventKind, TrendDimension, TrendStatus, distribution_promise_is_empty,
};
use serde_json::{Value as JsonValue, json};
use time::Date;
use uuid::Uuid;

use crate::efe::{EfeWeights, GrowthOpportunity, information_gain};

/// Band-hours each effort level costs. The unit is the band's evening,
/// not compute — `low` is a post someone writes on the couch, `high` is a
/// day with gear and other people.
fn effort_hours(effort: Effort) -> f64 {
    match effort {
        Effort::Low => 2.0,
        Effort::Medium => 6.0,
        Effort::High => 12.0,
    }
}

/// How much a band-hour costs the score. Chosen so a standalone `high`
/// day (12h → +1.8 EFE) can still beat a marginal `low` (2h → +0.3) when
/// its expected fans are roughly triple — dramatic, not routine.
pub const COST_WEIGHT: f64 = 0.15;

/// An arc-approved beat lifts expected fans by this factor — the band
/// already chose this shape, so the engine treats its formats as the
/// plan, not the alternatives.
pub const ARC_LIFT: f64 = 1.4;
/// Confirmed corroboration lifts expected fans by half; an emerging
/// pattern by an eighth. Supply-plus-demand agreement is the strongest
/// signal a format has short of the band's own outcomes.
pub const CONFIRMED_TREND_LIFT: f64 = 1.5;
pub const EMERGING_TREND_LIFT: f64 = 1.125;

/// 5.6 — shared learning. A format the roster's same-style sibling acts
/// actually produced at least this many times is the label's experience
/// arguing for it: a modest lift on expected fans, weaker than the band's
/// own approved arc and much weaker than a confirmed trend. One production
/// is an anecdote, not a pattern — the term stays inert below two.
pub const SIBLING_PROOF_MIN: u32 = 2;
pub const SIBLING_PROOF_LIFT: f64 = 1.15;

/// One outcome's report of what a production earned — `new_fans` from the
/// suggestion's `results` payload — decays at this rate per older report.
/// The most recent measured outcome is half the answer; the one before it
/// a quarter. Two or three reports are all a format usually has, so the
/// weighting stays shallow enough that a single viral outlier cannot pin
/// the yield.
pub const YIELD_EMA_ALPHA: f64 = 0.5;
/// Measured yield shrinks toward the purpose prior rather than replacing
/// it: a format's first report moves expected fans by a third, and the
/// measured mean takes over only as reports accumulate.
pub const YIELD_PRIOR_WEIGHT: f64 = 2.0;
/// Bounds on the learned multiplier — one bad week cannot zero a format's
/// rank, and one good one cannot make it the only thing suggested.
pub const YIELD_MIN: f64 = 0.25;
pub const YIELD_MAX: f64 = 4.0;

/// What the band's own resolved outcomes measured for one format: the
/// exponentially-weighted mean of reported `new_fans` (recent reports
/// weigh more), and how many outcomes carried a real measurement.
/// `measured` counts only outcomes that reported — `done` without a
/// `new_fans` figure teaches the stale rule, not the yield.
#[derive(Clone, Copy, Debug)]
pub struct FormatYield {
    pub measured_fans_ema: f64,
    pub measured: u32,
}

/// Pareto says two or three, not everything over a threshold.
pub const DEFAULT_LIMIT: usize = 3;

/// A concept that has been offered this many times without ever being
/// produced has had its chances — the tail names it, the queue stops
/// offering it. Six is the §4b-4 bound: enough attempts that "the band
/// never got to it" stops being the plausible story.
pub const STALE_ATTEMPT_LIMIT: u32 = 6;

/// The §4b-4 stale rule, one spelling for every caller: offered at least
/// [`STALE_ATTEMPT_LIMIT`] times and never once produced. `done` and
/// `done_differently` count as production; `declined` and `expired` are
/// attempts. Callers count offers and productions, this decides.
pub fn is_stale(offered: u32, produced: u32) -> bool {
    offered >= STALE_ATTEMPT_LIMIT && produced == 0
}

/// Expected-fans priors per purpose until this tenant has measured
/// outcomes. Acquisition buys new fans, conversion moves the ones we
/// have, credibility compounds slowly, retention holds. These are
/// guesses in the honest sense — `SuggestionOutcome` rows are what
/// replaces them.
fn base_expected_fans(purpose: FormatPurpose) -> f64 {
    match purpose {
        FormatPurpose::Acquisition => 10.0,
        FormatPurpose::Conversion => 5.0,
        FormatPurpose::Credibility => 4.0,
        FormatPurpose::Retention => 3.0,
    }
}

/// Flat uncertainty prior — there is no per-format variance model yet, so
/// information gain degrades to "formats with fewer outcomes teach more",
/// which is the right shape either way.
const PREDICT_STD_PRIOR: f64 = 1.0;

/// The distribution surfaces a promise can name with real counts behind
/// them. Anything else in a format's `distribution` text is a channel,
/// not a countable place — it does not become a clause.
#[derive(Clone, Debug, Default)]
pub struct ReachSnapshot {
    /// Names of admitted, active communities (`discovery_places`).
    pub communities: Vec<String>,
    /// Admitted or promoted press/radio/playlist candidates.
    pub press_contacts: u32,
    /// Fans reachable under a marketing consent today.
    pub consented_fans: u32,
    /// Confirmed peers — a collaboration's "other audience" is nameable,
    /// so `peer_cover` and the swap formats can promise it.
    pub peers: Vec<String>,
}

/// A production event covers formats only inside a forward window —
/// a shoot 18 months out does not make today's making-of near-free.
pub const COVERAGE_HORIZON_DAYS: i64 = 45;

/// One production event, reduced to what coverage needs.
#[derive(Clone, Debug)]
pub struct ScheduledProduction {
    /// The persisted id — carried into evidence so the "covered" claim
    /// names the day it rests on.
    pub id: String,
    pub kind: ProductionEventKind,
    pub scheduled_for: Date,
}

/// A ranked, persistable suggestion.
#[derive(Clone, Debug)]
pub struct ScoredSuggestion {
    pub format_key: String,
    /// The concept line the band reads — the catalogue name today; a
    /// bespoke concept is a later sprint's work.
    pub concept: String,
    /// Why this beat, why now — assembled from the clauses that won it
    /// the rank, so the reason is auditable, not vibes.
    pub reason: String,
    /// The ids and rows the claim rests on — trends, the covering event,
    /// the arc, and the score itself, so the rank survives audit.
    pub evidence: JsonValue,
    /// Marginal effort when a production day covers the beat.
    pub effort: Effort,
    /// Whether a scheduled production event covers this format — the
    /// harvest rule's price tag.
    pub covered_by_production: bool,
    /// The arc this beat serves, when the format is in one's spine.
    pub arc_id: Option<Uuid>,
    /// The covering production day — the suggestion dies when it passes.
    pub suggested_before: Option<Date>,
    pub efe_score: f64,
    pub distribution_promise: JsonValue,
    /// This format is a beat in an active arc's spine.
    pub arc_format_key_hit: bool,
}

/// Which production-event kinds harvest which catalogue formats — §4b-3's
/// harvest maps, made explicit so the coverage claim is auditable. A kind
/// not listed covers nothing.
#[must_use]
pub fn covered_by_production(format_key: &str, kind: ProductionEventKind) -> bool {
    match kind {
        // Video shoot day: the video, the making-of, a playthrough cut.
        ProductionEventKind::Shoot => {
            matches!(format_key, "official_video" | "making_of" | "playthrough")
        }
        // Studio session: commentary, a stripped take, the studio diary.
        ProductionEventKind::Studio => {
            matches!(
                format_key,
                "track_by_track" | "stripped_version" | "making_of"
            )
        }
        // Show or festival: everything §4b-3 lists for the day itself.
        ProductionEventKind::Show | ProductionEventKind::Festival => matches!(
            format_key,
            "soundcheck_clip"
                | "aftermovie"
                | "tour_diary"
                | "live_session"
                | "fan_content_feature"
        ),
        // Rehearsal: the clip and the one-take live session.
        ProductionEventKind::Rehearsal => {
            matches!(format_key, "rehearsal_clip" | "live_session")
        }
        // The long drive is the tour diary's home.
        ProductionEventKind::Drive => format_key == "tour_diary",
        // Photoshoot: the artwork stills and gear/table shots.
        ProductionEventKind::Photoshoot => format_key == "behind_artwork",
        ProductionEventKind::Other => false,
    }
}

/// The catalogue keys a detected pattern can lift. The trend lexicon
/// speaks in observation words ("rehearsal", "one take"); the catalogue
/// speaks in format keys ("rehearsal_clip", "live_session"). Without this
/// bridge, corroboration would be unreachable for most of the catalogue —
/// the strongest evidence we have would move nothing.
#[must_use]
pub fn format_keys_for_pattern(pattern: &str) -> &'static [&'static str] {
    match pattern {
        "rehearsal" => &["rehearsal_clip"],
        "interview" => &["interview_podcast"],
        "studio_diary" | "behind_the_scenes" => &["making_of"],
        "cover" => &["peer_cover", "fan_cover_feature"],
        "acoustic" | "stripped" => &["stripped_version"],
        "reaction" => &["old_material_reaction"],
        "one_take" => &["live_session"],
        _ => &[],
    }
}

/// Whether a trend pattern lifts this format key — direct equality plus
/// the alias bridge.
fn pattern_lifts(pattern: &str, format_key: &str) -> bool {
    pattern == format_key || format_keys_for_pattern(pattern).contains(&format_key)
}

/// Assemble the distribution promise from real counts. A clause exists
/// only when the surface it names has something to promise — zero fans is
/// not a promise, it is a fact the band should see elsewhere.
///
/// The mapping from `distribution` text to clauses is deliberately
/// string-matched: the catalogue writes `post → communities, fans, press
/// list`, and the promise names which communities, how many fans, how
/// many contacts. Collaboration formats reach "both audiences" / "the
/// peer's orbit" — the peer is a nameable entity, so `peer_audience`
/// carries confirmed peer names rather than a guessed count. A format
/// whose text names none of the surfaces produces `{}` and the engine
/// declines it.
#[must_use]
pub fn assemble_promise(distribution: &str, reach: &ReachSnapshot) -> JsonValue {
    let text = distribution.to_lowercase();
    let mut promise = serde_json::Map::new();
    if text.contains("communities") && !reach.communities.is_empty() {
        promise.insert(
            "communities".to_owned(),
            json!(
                reach
                    .communities
                    .iter()
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>()
            ),
        );
    }
    if text.contains("press") && reach.press_contacts > 0 {
        promise.insert("press_contacts".to_owned(), json!(reach.press_contacts));
    }
    if text.contains("fans") && reach.consented_fans > 0 {
        promise.insert("consented_fans".to_owned(), json!(reach.consented_fans));
    }
    if (text.contains("peer") || text.contains("audience")) && !reach.peers.is_empty() {
        promise.insert(
            "peer_audience".to_owned(),
            json!(reach.peers.iter().take(10).cloned().collect::<Vec<_>>()),
        );
    }
    JsonValue::Object(promise)
}

/// Inputs to one ranking pass — the repository assembles them; the
/// engine never touches a database.
pub struct RankingInputs<'a> {
    pub formats: &'a [ContentFormatEntry],
    pub profile: &'a CapabilityProfile,
    /// Live trends — `faded` rows are ignored entirely.
    pub trends: &'a [ContentTrend],
    /// Upcoming scheduled production events — only those inside
    /// [`COVERAGE_HORIZON_DAYS`] of `today` can cover a format.
    pub production: &'a [ScheduledProduction],
    /// Format keys with an open suggestion already — never re-raise.
    pub open_format_keys: &'a BTreeSet<String>,
    /// Format keys the band declined inside the taste cooldown — a "not
    /// for us" is a verdict, not a pause, so the concept stays out of the
    /// queue until the window lapses and evidence can argue it back.
    pub declined_format_keys: &'a BTreeSet<String>,
    /// Format keys that ran out of road on their own: suggested at least
    /// [`STALE_ATTEMPT_LIMIT`] times and never once produced — a stale
    /// concept retires itself instead of being offered a seventh time.
    pub retired_format_keys: &'a BTreeSet<String>,
    /// Format key → arc id, written into an active arc's spine.
    pub arc_format_keys: &'a BTreeMap<String, Uuid>,
    /// Outcome count per format key — what the band has already tried.
    pub outcome_counts: &'a BTreeMap<String, u32>,
    /// Measured new-fan yield per format key, learned from the resolved
    /// outcomes' `results` payloads — the answer to "what did this format
    /// actually earn when this band made it". Absent entries mean no
    /// measurement exists and the purpose prior stands unmodified.
    pub format_yield: &'a BTreeMap<String, FormatYield>,
    /// Suggestion count per format key — novelty decays as the engine
    /// repeats itself.
    pub suggestion_counts: &'a BTreeMap<String, u32>,
    /// 5.6 — productions of each format pooled across the roster's
    /// same-style sibling acts. A shared prior, not a verdict: it can only
    /// lift a format that already cleared every gate of this act's own —
    /// a declined or capability-gated concept never reaches this code. The
    /// act's own taste stays absolute; the label's experience only argues.
    pub sibling_produced: &'a BTreeMap<String, u32>,
    pub reach: &'a ReachSnapshot,
    pub weights: EfeWeights,
    /// The cycle's date — coverage only counts events ahead of it.
    pub today: Date,
}

/// Rank the catalogue into the suggestions worth raising this cycle —
/// the full feasible list, sorted by EFE ascending. The caller applies
/// the Pareto cut and names the tail: "rank, then cut" is a discipline
/// about what reaches the queue, and the discarded tail is part of the
/// answer, not silence.
#[must_use]
pub fn rank_suggestions(inputs: &RankingInputs<'_>) -> Vec<ScoredSuggestion> {
    let horizon_end = inputs.today + time::Duration::days(COVERAGE_HORIZON_DAYS);
    // The band approved a shape for the season — while an arc runs,
    // formats outside its spine are noise unless a deadline makes them
    // urgent. A production-covered beat carries `suggested_before` and is
    // exactly that: time-boxed, or it waits for the next arc.
    let arc_active = !inputs.arc_format_keys.is_empty();
    let mut scored: Vec<ScoredSuggestion> = Vec::new();
    for entry in inputs.formats {
        if !entry.active
            || inputs.open_format_keys.contains(&entry.key)
            || inputs.declined_format_keys.contains(&entry.key)
            || inputs.retired_format_keys.contains(&entry.key)
        {
            continue;
        }
        if entry.capability_gap(inputs.profile).is_some() {
            continue;
        }

        // Coverage needs a real day: the event's kind harvests this
        // format AND its date sits inside the forward horizon. A shoot
        // next year does not make today's beat near-free.
        let covering: Vec<&ScheduledProduction> = inputs
            .production
            .iter()
            .filter(|event| {
                covered_by_production(&entry.key, event.kind)
                    && event.scheduled_for >= inputs.today
                    && event.scheduled_for <= horizon_end
            })
            .collect();
        let covered = !covering.is_empty();
        let effort = entry.effort_for(covered);
        let suggested_before = covering.iter().map(|event| event.scheduled_for).min();

        // Trend lift: the strongest live trend naming this format, via
        // the alias bridge. A platform trend cannot lift a format key —
        // dimension-scoped.
        let mut trend_lift = 1.0_f64;
        let mut lift_evidence: Vec<String> = Vec::new();
        for trend in inputs.trends {
            if trend.dimension != TrendDimension::Format || trend.status == TrendStatus::Faded {
                continue;
            }
            if !pattern_lifts(&trend.pattern, &entry.key) {
                continue;
            }
            let factor = if trend.status == TrendStatus::Confirmed {
                CONFIRMED_TREND_LIFT
            } else {
                EMERGING_TREND_LIFT
            };
            if factor > trend_lift {
                trend_lift = factor;
            }
            lift_evidence.push(trend.id.into_uuid().to_string());
        }

        let arc_id = inputs.arc_format_keys.get(&entry.key).cloned();
        let arc_hit = arc_id.is_some();
        if arc_active && !arc_hit && suggested_before.is_none() {
            continue;
        }
        let mut lift = trend_lift;
        if arc_hit {
            lift *= ARC_LIFT;
        }
        // Shared learning (5.6): the roster's same-style siblings already
        // made this format work — a prior from the label's own ledger.
        // It arrives after every gate, so it reorders the queue but can
        // never put back what this act's own taste or capability removed.
        let sibling_produced = inputs
            .sibling_produced
            .get(&entry.key)
            .copied()
            .unwrap_or(0);
        let sibling_proof = sibling_produced >= SIBLING_PROOF_MIN;
        if sibling_proof {
            lift *= SIBLING_PROOF_LIFT;
        }

        let promise = assemble_promise(&entry.distribution, inputs.reach);
        if distribution_promise_is_empty(&promise) {
            // A suggestion that reaches nobody is a chore, not a plan.
            continue;
        }

        let outcomes = inputs.outcome_counts.get(&entry.key).copied().unwrap_or(0);
        let prior_suggestions = inputs
            .suggestion_counts
            .get(&entry.key)
            .copied()
            .unwrap_or(0);
        // The band's own measured yield argues against the purpose prior:
        // the learned EMA replaces the guess in proportion to how many
        // outcomes reported, clamped so a format can be argued down to a
        // quarter or up to fourfold — never silenced, never crowned.
        let base_fans = base_expected_fans(entry.purpose);
        let yield_multiplier = inputs
            .format_yield
            .get(&entry.key)
            .filter(|yield_| yield_.measured > 0)
            .map_or(1.0, |yield_| {
                let measured = f64::from(yield_.measured);
                let shrunk = (YIELD_PRIOR_WEIGHT + measured * yield_.measured_fans_ema / base_fans)
                    / (YIELD_PRIOR_WEIGHT + measured);
                shrunk.clamp(YIELD_MIN, YIELD_MAX)
            });
        let expected_fans = base_fans * lift * yield_multiplier;
        let gain = information_gain(outcomes, PREDICT_STD_PRIOR);
        // Novelty decays as the same format keeps being offered — the
        // first playthrough suggestion is news, the fifth is nagging.
        let novelty = 1.0 / (1.0 + f64::from(prior_suggestions));

        let efe = GrowthOpportunity::compute_efe(
            expected_fans,
            gain,
            PREDICT_STD_PRIOR,
            novelty,
            inputs.weights,
        ) + COST_WEIGHT * effort_hours(effort);

        scored.push(ScoredSuggestion {
            format_key: entry.key.clone(),
            concept: entry.name.clone(),
            reason: reason_for(
                entry,
                covered,
                trend_lift > 1.0,
                arc_hit,
                sibling_produced,
                &promise,
                inputs.reach.communities.len(),
            ),
            evidence: json!({
                "trend_ids": lift_evidence,
                "covered_by_production": covered,
                "covering_event_ids": covering
                    .iter()
                    .map(|event| event.id.clone())
                    .collect::<Vec<_>>(),
                "arc_format_key_hit": arc_hit,
                "sibling_productions": sibling_produced,
                "efe_score": efe,
                "lift": lift,
                "format_yield": yield_multiplier,
            }),
            effort,
            covered_by_production: covered,
            arc_id,
            suggested_before,
            efe_score: efe,
            distribution_promise: promise,
            arc_format_key_hit: arc_hit,
        });
    }
    scored.sort_by(|a, b| {
        a.efe_score
            .total_cmp(&b.efe_score)
            .then_with(|| a.format_key.cmp(&b.format_key))
    });
    scored
}

/// The auditable "why" — each clause names the input that earned it.
#[allow(clippy::too_many_arguments)]
fn reason_for(
    entry: &ContentFormatEntry,
    covered: bool,
    trend_lifted: bool,
    arc_hit: bool,
    sibling_produced: u32,
    promise: &JsonValue,
    community_total: usize,
) -> String {
    let mut clauses = vec![format!(
        "{} ({} effort{})",
        entry.name,
        entry.effort_for(covered).as_str(),
        if covered {
            ", near-free while a production day already covers it"
        } else {
            ""
        }
    )];
    if trend_lifted {
        clauses.push("the format is trending".to_owned());
    }
    if arc_hit {
        clauses.push("it is a beat in the approved arc".to_owned());
    }
    if sibling_produced >= SIBLING_PROOF_MIN {
        clauses.push(format!(
            "same-style acts on the roster made it {sibling_produced} times"
        ));
    }
    if let Some(map) = promise.as_object() {
        let mut reach = Vec::new();
        if map.contains_key("communities") {
            reach.push(format!("{community_total} communities"));
        }
        if let Some(p) = map.get("press_contacts").and_then(JsonValue::as_u64) {
            reach.push(format!("{p} press contacts"));
        }
        if let Some(f) = map.get("consented_fans").and_then(JsonValue::as_u64) {
            reach.push(format!("{f} consented fans"));
        }
        if let Some(peers) = map.get("peer_audience").and_then(JsonValue::as_array) {
            reach.push(format!("{} peers' audiences", peers.len()));
        }
        if !reach.is_empty() {
            clauses.push(format!("reaches {}", reach.join(", ")));
        }
    }
    clauses.join("; ")
}

#[cfg(test)]
mod tests;
