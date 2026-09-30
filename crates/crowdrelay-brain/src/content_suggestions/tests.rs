use std::collections::BTreeMap;

use crowdrelay_domain::content_engine::{FormatCadence, FormatCategory, FormatRequirement};
use crowdrelay_domain::team_operations::TeamSkill;
use uuid::Uuid;

use super::*;

fn entry(key: &str, skill: TeamSkill, requires: FormatRequirement) -> ContentFormatEntry {
    ContentFormatEntry {
        key: key.to_owned(),
        name: key.replace('_', " "),
        category: FormatCategory::Evergreen,
        purpose: FormatPurpose::Acquisition,
        effort_standalone: Effort::Medium,
        effort_marginal: Effort::Low,
        skill,
        requires,
        distribution: "video artifact → YouTube, communities, fans, press".to_owned(),
        cadence: FormatCadence::Recurring,
        genre_fit: vec![],
        notes: String::new(),
        active: true,
    }
}

fn profile() -> CapabilityProfile {
    CapabilityProfile {
        skills: [TeamSkill::Video, TeamSkill::Social].into_iter().collect(),
        has_release_material: true,
        has_show_material: true,
    }
}

fn reach() -> ReachSnapshot {
    ReachSnapshot {
        communities: vec!["r/Metal".to_owned(), "r/listentothis".to_owned()],
        press_contacts: 12,
        consented_fans: 340,
        peers: vec![],
    }
}

fn today() -> Date {
    Date::from_calendar_date(2026, time::Month::October, 1).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn inputs<'a>(
    formats: &'a [ContentFormatEntry],
    profile: &'a CapabilityProfile,
    trends: &'a [ContentTrend],
    production: &'a [ScheduledProduction],
    open: &'a BTreeSet<String>,
    declined: &'a BTreeSet<String>,
    arc: &'a BTreeMap<String, Uuid>,
    outcomes: &'a BTreeMap<String, u32>,
    suggestions: &'a BTreeMap<String, u32>,
    reach: &'a ReachSnapshot,
) -> RankingInputs<'a> {
    RankingInputs {
        formats,
        profile,
        trends,
        production,
        open_format_keys: open,
        declined_format_keys: declined,
        retired_format_keys: &EMPTY_KEYS,
        arc_format_keys: arc,
        outcome_counts: outcomes,
        format_yield: &EMPTY_YIELD,
        suggestion_counts: suggestions,
        sibling_format_yield: &EMPTY_YIELD,
        reach,
        weights: EfeWeights::default(),
        today: today(),
    }
}

#[test]
fn the_video_gap_is_a_hard_filter() {
    let formats = vec![entry(
        "playthrough",
        TeamSkill::Video,
        FormatRequirement::Nothing,
    )];
    let no_film = CapabilityProfile {
        skills: [TeamSkill::Social].into_iter().collect(),
        ..profile()
    };
    let ranked = rank_suggestions(&inputs(
        &formats,
        &no_film,
        &[],
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reach(),
    ));
    assert!(
        ranked.is_empty(),
        "no filmmaker means no playthrough suggestion"
    );
}

#[test]
fn an_empty_promise_declines_the_suggestion() {
    let formats = vec![entry(
        "playthrough",
        TeamSkill::Video,
        FormatRequirement::Nothing,
    )];
    let nobody = ReachSnapshot::default();
    let ranked = rank_suggestions(&inputs(
        &formats,
        &profile(),
        &[],
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &nobody,
    ));
    assert!(
        ranked.is_empty(),
        "a suggestion that reaches nobody is not worth raising"
    );
}

#[test]
fn a_scheduled_shoot_makes_the_video_near_free() {
    let shoot = [ScheduledProduction {
        id: "shoot-1".to_owned(),
        kind: ProductionEventKind::Shoot,
        scheduled_for: today() + time::Duration::days(5),
    }];
    let ranked = rank_suggestions(&inputs(
        &[entry(
            "making_of",
            TeamSkill::Video,
            FormatRequirement::Nothing,
        )],
        &profile(),
        &[],
        &shoot,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reach(),
    ));
    assert_eq!(ranked.len(), 1);
    assert!(ranked[0].covered_by_production);
    assert_eq!(
        ranked[0].effort,
        Effort::Low,
        "the harvest rule prices the day already spent"
    );
    assert_eq!(
        ranked[0].suggested_before,
        Some(today() + time::Duration::days(5)),
        "the suggestion dies when the covering day passes"
    );
    assert!(ranked[0].reason.contains("covers it"));
}

#[test]
fn a_shoot_beyond_the_horizon_does_not_cover() {
    let far = [ScheduledProduction {
        id: "shoot-far".to_owned(),
        kind: ProductionEventKind::Shoot,
        scheduled_for: today() + time::Duration::days(COVERAGE_HORIZON_DAYS + 30),
    }];
    let ranked = rank_suggestions(&inputs(
        &[entry(
            "making_of",
            TeamSkill::Video,
            FormatRequirement::Nothing,
        )],
        &profile(),
        &[],
        &far,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reach(),
    ));
    assert_eq!(ranked.len(), 1);
    assert!(
        !ranked[0].covered_by_production,
        "a production day outside the window is not coverage"
    );
    assert_eq!(ranked[0].effort, Effort::Medium);
}

#[test]
fn measured_yield_moves_expected_fans_toward_the_band_earned_number() {
    let formats = vec![
        entry("playthrough", TeamSkill::Video, FormatRequirement::Nothing),
        entry("tour_diary", TeamSkill::Social, FormatRequirement::Nothing),
    ];
    // `playthrough` has earned 40 fans per production across three
    // reports; `tour_diary` earned 2 on one. The prior is 10 for both
    // (acquisition purpose), so the yields should invert their order.
    let mut yields = BTreeMap::new();
    yields.insert(
        "playthrough".to_owned(),
        FormatYield {
            measured_fans_ema: 40.0,
            measured: 3,
        },
    );
    yields.insert(
        "tour_diary".to_owned(),
        FormatYield {
            measured_fans_ema: 2.0,
            measured: 1,
        },
    );
    let profile = profile();
    let open = BTreeSet::new();
    let declined = BTreeSet::new();
    let arc = BTreeMap::new();
    let outcomes = BTreeMap::new();
    let suggestions = BTreeMap::new();
    let reach = reach();
    let mut inputs = inputs(
        &formats,
        &profile,
        &[],
        &[],
        &open,
        &declined,
        &arc,
        &outcomes,
        &suggestions,
        &reach,
    );
    inputs.format_yield = &yields;
    let ranked = rank_suggestions(&inputs);
    assert_eq!(ranked.len(), 2);
    assert_eq!(
        ranked[0].format_key, "playthrough",
        "the format the band measured at 40 fans outranks the one measured at 2"
    );
    // One report of 2 fans shrinks toward the prior rather than
    // collapsing to it: multiplier = (2 + 1·0.2) / (2 + 1) ≈ 0.73.
    let diary = ranked
        .iter()
        .find(|s| s.format_key == "tour_diary")
        .unwrap();
    let diary_yield = diary.evidence["format_yield"].as_f64().unwrap();
    assert!(
        (diary_yield - 0.7333).abs() < 0.01,
        "a thin measurement argues down toward the prior, not to the raw ratio: {diary_yield}"
    );
}

#[test]
fn measured_yield_is_clamped_against_outliers() {
    let formats = vec![entry(
        "playthrough",
        TeamSkill::Video,
        FormatRequirement::Nothing,
    )];
    let mut yields = BTreeMap::new();
    yields.insert(
        "playthrough".to_owned(),
        FormatYield {
            measured_fans_ema: 10_000.0,
            measured: 1,
        },
    );
    let profile = profile();
    let open = BTreeSet::new();
    let declined = BTreeSet::new();
    let arc = BTreeMap::new();
    let outcomes = BTreeMap::new();
    let suggestions = BTreeMap::new();
    let reach = reach();
    let mut inputs = inputs(
        &formats,
        &profile,
        &[],
        &[],
        &open,
        &declined,
        &arc,
        &outcomes,
        &suggestions,
        &reach,
    );
    inputs.format_yield = &yields;
    let ranked = rank_suggestions(&inputs);
    // YIELD_MAX caps the multiplier — a viral outlier cannot turn one
    // format into the only suggestion the engine ever raises.
    let evidence = &ranked[0].evidence;
    assert_eq!(evidence["format_yield"], serde_json::json!(YIELD_MAX));
}

#[test]
fn a_confirmed_trend_lifts_the_format() {
    let now = time::OffsetDateTime::now_utc();
    let trend = ContentTrend {
        id: crowdrelay_domain::ContentTrendId::from_uuid(Uuid::from_u128(7)),
        workspace_id: crowdrelay_domain::WorkspaceId::from_uuid(Uuid::from_u128(1)),
        dimension: TrendDimension::Format,
        pattern: "playthrough".to_owned(),
        strength: 9000,
        sources: 3,
        evidence: serde_json::json!({}),
        status: TrendStatus::Confirmed,
        first_seen: now.date(),
        last_seen: now.date(),
        created_at: now,
        updated_at: now,
    };
    let formats = vec![entry(
        "playthrough",
        TeamSkill::Video,
        FormatRequirement::Nothing,
    )];
    let lifted = rank_suggestions(&inputs(
        &formats,
        &profile(),
        std::slice::from_ref(&trend),
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reach(),
    ));
    let flat = rank_suggestions(&inputs(
        &formats,
        &profile(),
        &[],
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reach(),
    ));
    assert!(
        lifted[0].efe_score < flat[0].efe_score,
        "corroborated evidence lowers EFE: {} vs {}",
        lifted[0].efe_score,
        flat[0].efe_score
    );
}

static EMPTY_KEYS: BTreeSet<String> = BTreeSet::new();
static EMPTY_COUNTS: BTreeMap<String, u32> = BTreeMap::new();
static EMPTY_YIELD: BTreeMap<String, FormatYield> = BTreeMap::new();

#[test]
fn rank_then_cut_keeps_only_the_vital_few() {
    let formats = vec![
        entry("a", TeamSkill::Video, FormatRequirement::Nothing),
        entry("b", TeamSkill::Video, FormatRequirement::Nothing),
        entry("c", TeamSkill::Video, FormatRequirement::Nothing),
        entry("d", TeamSkill::Video, FormatRequirement::Nothing),
        entry("e", TeamSkill::Video, FormatRequirement::Nothing),
    ];
    let ranked = rank_suggestions(&inputs(
        &formats,
        &profile(),
        &[],
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reach(),
    ));
    // The engine returns the full feasible ordering; the caller cuts
    // to DEFAULT_LIMIT and names the tail.
    assert_eq!(ranked.len(), formats.len());
    let top = &ranked[..DEFAULT_LIMIT];
    assert_eq!(top.len(), DEFAULT_LIMIT, "the caller keeps the vital few");
    assert!(
        ranked.windows(2).all(|w| w[0].efe_score <= w[1].efe_score),
        "the ordering is EFE ascending"
    );
}

#[test]
fn the_promise_carries_only_real_clauses() {
    let promise = assemble_promise(
        "video artifact → YouTube, communities, fans",
        &ReachSnapshot {
            communities: vec!["r/Metal".to_owned()],
            press_contacts: 0,
            consented_fans: 340,
            peers: vec![],
        },
    );
    let map = promise.as_object().unwrap();
    assert!(map.contains_key("communities"));
    assert_eq!(map["consented_fans"], 340);
    assert!(
        !map.contains_key("press_contacts"),
        "the text names press but zero contacts is not a promise"
    );
}

#[test]
fn a_collaboration_promises_the_peer_audience() {
    let promise = assemble_promise(
        "track → both audiences, streaming, press",
        &ReachSnapshot {
            communities: vec![],
            press_contacts: 12,
            consented_fans: 0,
            peers: vec!["Void Congregation".to_owned()],
        },
    );
    let map = promise.as_object().unwrap();
    assert_eq!(map["peer_audience"][0], "Void Congregation");
    assert_eq!(map["press_contacts"], 12);
}

#[test]
fn lexicon_patterns_lift_their_catalogue_keys() {
    // The alias bridge is the whole claim: "rehearsal" facts must move
    // `rehearsal_clip`, or corroboration is unreachable for it.
    assert!(pattern_lifts("rehearsal", "rehearsal_clip"));
    assert!(pattern_lifts("one_take", "live_session"));
    assert!(pattern_lifts("studio_diary", "making_of"));
    assert!(pattern_lifts("cover", "peer_cover"));
    assert!(pattern_lifts("cover", "fan_cover_feature"));
    assert!(!pattern_lifts("rehearsal", "playthrough"));
}

#[test]
fn an_active_arc_refuses_orphans_but_not_deadlines() {
    let arc_id = Uuid::from_u128(42);
    let arc: BTreeMap<String, Uuid> = [("playthrough".to_owned(), arc_id)].into_iter().collect();
    let formats = vec![
        entry("playthrough", TeamSkill::Video, FormatRequirement::Nothing),
        entry(
            "official_video",
            TeamSkill::Video,
            FormatRequirement::Nothing,
        ),
        entry(
            "rehearsal_clip",
            TeamSkill::Video,
            FormatRequirement::Nothing,
        ),
    ];
    let shoot = [ScheduledProduction {
        id: "shoot-1".to_owned(),
        kind: ProductionEventKind::Shoot,
        scheduled_for: today() + time::Duration::days(9),
    }];
    let ranked = rank_suggestions(&inputs(
        &formats,
        &profile(),
        &[],
        &shoot,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &arc,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reach(),
    ));
    let keys: Vec<&str> = ranked.iter().map(|s| s.format_key.as_str()).collect();
    assert!(
        keys.contains(&"playthrough"),
        "a spine beat still ranks: {keys:?}"
    );
    assert!(
        keys.contains(&"official_video"),
        "a shoot-covered orphan is the urgent, time-boxed exception: {keys:?}"
    );
    assert!(
        !keys.contains(&"rehearsal_clip"),
        "an orphan with no deadline waits for the next arc: {keys:?}"
    );
    assert_eq!(
        ranked
            .iter()
            .find(|s| s.format_key == "playthrough")
            .and_then(|s| s.arc_id),
        Some(arc_id),
        "the spine beat carries the arc it serves"
    );
}

/// §4b-4 — a concept that has been offered six times and never
/// produced is retired, not re-offered: the filter is a hard stop on
/// the same footing as an open row or a declined taste signal.
#[test]
fn a_stale_concept_retires_itself() {
    let formats = vec![
        entry("playthrough", TeamSkill::Video, FormatRequirement::Nothing),
        entry(
            "rehearsal_clip",
            TeamSkill::Video,
            FormatRequirement::Nothing,
        ),
    ];
    let empty: BTreeSet<String> = BTreeSet::new();
    let empty_arc: BTreeMap<String, Uuid> = BTreeMap::new();
    let empty_counts: BTreeMap<String, u32> = BTreeMap::new();
    let retired: BTreeSet<String> = ["playthrough".to_owned()].into_iter().collect();
    let profile = profile();
    let reach = reach();
    let base = inputs(
        &formats,
        &profile,
        &[],
        &[],
        &empty,
        &empty,
        &empty_arc,
        &empty_counts,
        &empty_counts,
        &reach,
    );
    let ranked = rank_suggestions(&RankingInputs {
        retired_format_keys: &retired,
        ..base
    });
    let keys: Vec<&str> = ranked.iter().map(|s| s.format_key.as_str()).collect();
    assert!(
        !keys.contains(&"playthrough"),
        "six attempts without a single production retires the concept: {keys:?}"
    );
    assert!(
        keys.contains(&"rehearsal_clip"),
        "the fresh concept still ranks: {keys:?}"
    );
}

#[test]
fn sibling_fan_yield_informs_but_never_overrides_the_band() {
    let formats = vec![
        entry("playthrough", TeamSkill::Video, FormatRequirement::Nothing),
        entry(
            "rehearsal_clip",
            TeamSkill::Video,
            FormatRequirement::Nothing,
        ),
    ];
    let reach = reach();
    let empty = BTreeSet::new();
    let empty_arc = BTreeMap::new();
    let empty_counts = BTreeMap::new();
    let proven = BTreeMap::from([(
        "playthrough".to_owned(),
        FormatYield {
            measured_fans_ema: 40.0,
            measured: 3,
        },
    )]);

    let ranked = rank_suggestions(&RankingInputs {
        sibling_format_yield: &proven,
        ..inputs(
            &formats,
            &profile(),
            &[],
            &[],
            &empty,
            &empty,
            &empty_arc,
            &empty_counts,
            &empty_counts,
            &reach,
        )
    });
    assert_eq!(ranked[0].format_key, "playthrough");
    assert_eq!(
        ranked[0].evidence.get("sibling_measured_pieces"),
        Some(&serde_json::json!(3))
    );
    assert_eq!(
        ranked[0].evidence.get("sibling_yield_multiplier"),
        Some(&serde_json::json!(SIBLING_YIELD_MAX))
    );
    assert!(ranked[0].reason.contains("fans/piece"));

    let anecdote = BTreeMap::from([(
        "rehearsal_clip".to_owned(),
        FormatYield {
            measured_fans_ema: 100.0,
            measured: 1,
        },
    )]);
    let with = rank_suggestions(&RankingInputs {
        sibling_format_yield: &anecdote,
        ..inputs(
            &formats,
            &profile(),
            &[],
            &[],
            &empty,
            &empty,
            &empty_arc,
            &empty_counts,
            &empty_counts,
            &reach,
        )
    });
    let without = rank_suggestions(&inputs(
        &formats,
        &profile(),
        &[],
        &[],
        &empty,
        &empty,
        &empty_arc,
        &empty_counts,
        &empty_counts,
        &reach,
    ));
    assert_eq!(
        with.iter().map(|s| &s.format_key).collect::<Vec<_>>(),
        without.iter().map(|s| &s.format_key).collect::<Vec<_>>(),
        "one measured piece must not reorder"
    );

    let declined = BTreeSet::from(["playthrough".to_owned()]);
    let ranked = rank_suggestions(&RankingInputs {
        sibling_format_yield: &proven,
        ..inputs(
            &formats,
            &profile(),
            &[],
            &[],
            &empty,
            &declined,
            &empty_arc,
            &empty_counts,
            &empty_counts,
            &reach,
        )
    });
    assert!(
        !ranked.iter().any(|s| s.format_key == "playthrough"),
        "the band's own decline outranks the label's experience"
    );
}
