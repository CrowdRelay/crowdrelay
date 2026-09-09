//! Fan-growth attribution — why did the fan count change?
//!
//! Ported from Kern's `brain::attribution`, adapted from capital to fans.
//! The brain attributes every fan gained to its cause:
//! - Which template/strategy generated the dispatch?
//! - What context was the dispatch in (growth trend, event proximity)?
//! - Was the gain from the treatment effect or secular drift?
//! - Which hypothesis was being tested?
//!
//! This is the feedback loop that makes the brain honest. A template that
//! claims fan growth but actually just rode a viral wave is exposed here.
//! A strategy that looks effective but loses fans to audience fatigue
//! is exposed here.
//!
//! # Difference from treatment-effect estimation
//!
//! The treatment-effect posterior (`TreatmentEffectPosterior`) estimates
//! the *expected* effect of a template. Attribution explains the *actual*
//! outcome after the fact — it closes the loop by linking observed fan
//! growth to the decisions and actions that produced it.
//!
//! Attribution is read-only: it summarizes evidence that has already been
//! replayed into the causal model. It does not mutate any posterior.

use serde::Serialize;

use crate::evidence::{EvidenceQuality, GrowthEvidence};

/// Full fan-growth attribution for one cycle or period.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FanGrowthAttribution {
    /// Total observed fan growth across all evidence rows.
    pub total_observed_fans: f64,
    /// Total incremental (counterfactual-adjusted) fan growth.
    pub total_incremental_fans: f64,
    /// Total durable (30-day) fan growth.
    pub total_durable_fans: f64,
    /// Number of evidence rows with observed outcomes.
    pub resolved_observations: u32,
    /// Number of evidence rows with partial (3-day) outcomes.
    pub partial_observations: u32,
    /// Attribution by template.
    pub by_template: Vec<TemplateAttribution>,
    /// Attribution by strategy.
    pub by_strategy: Vec<StrategyAttribution>,
    /// Attribution by evidence quality.
    pub by_quality: Vec<QualityAttribution>,
}

/// Fan growth attributed to one worker template.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TemplateAttribution {
    pub template_id: String,
    /// Total observed fans from this template's dispatches.
    pub observed_fans: f64,
    /// Total incremental (treatment effect) fans.
    pub incremental_fans: f64,
    /// Total durable (30-day) fans.
    pub durable_fans: f64,
    /// Number of resolved evidence rows.
    pub observations: u32,
    /// Mean observed fans per dispatch.
    pub mean_observed_fans: f64,
    /// Mean incremental fans per dispatch.
    pub mean_incremental_fans: f64,
    /// Strongest evidence quality achieved.
    pub best_quality: EvidenceQuality,
}

/// Fan growth attributed to one growth strategy.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StrategyAttribution {
    pub strategy: String,
    /// Total observed fans from dispatches using this strategy.
    pub observed_fans: f64,
    /// Total incremental fans.
    pub incremental_fans: f64,
    /// Number of resolved evidence rows.
    pub observations: u32,
    /// Mean incremental fans per dispatch.
    pub mean_incremental_fans: f64,
}

/// Fan growth attributed by evidence quality tier.
#[derive(Clone, Debug, Default, Serialize)]
pub struct QualityAttribution {
    pub quality: EvidenceQuality,
    /// Total incremental fans at this quality level.
    pub incremental_fans: f64,
    /// Number of evidence rows at this quality level.
    pub observations: u32,
}

/// Attributes fan growth from resolved evidence.
///
/// The brain calls this to understand where its fan growth came from.
/// Secular drift (the control arm's pre/post change) is subtracted from
/// the treatment arm's raw outcome to get the incremental (treatment
/// effect) contribution. Templates that produced no incremental growth
/// are visible here — a template that ran 50 times and produced 0
/// incremental fans is not "untested," it is "ineffective."
#[must_use]
pub fn attribute_fan_growth(evidence: &[GrowthEvidence]) -> FanGrowthAttribution {
    let mut by_template: std::collections::HashMap<String, TemplateAttribution> =
        std::collections::HashMap::new();
    let mut by_strategy: std::collections::HashMap<String, StrategyAttribution> =
        std::collections::HashMap::new();
    let mut by_quality: std::collections::HashMap<EvidenceQuality, QualityAttribution> =
        std::collections::HashMap::new();

    let mut total_observed = 0.0;
    let mut total_incremental = 0.0;
    let mut total_durable = 0.0;
    let mut resolved_count = 0u32;
    let mut partial_count = 0u32;

    for ev in evidence {
        // Skip rows without any outcome — they haven't been measured yet.
        let has_outcome = ev.observed_fans.is_some()
            || ev.observed_incremental_fans.is_some()
            || ev.y30_outcome().is_some();
        if !has_outcome {
            continue;
        }

        // Count resolved vs partial.
        if ev.resolved_at.is_some() {
            resolved_count += 1;
        } else if ev.partial_resolution_count > 0 {
            partial_count += 1;
        }

        // Aggregate observed fans.
        if let Some(observed) = ev.observed_fans {
            total_observed += observed;
        }

        // Aggregate incremental (treatment effect) fans.
        if let Some(incremental) = ev.observed_incremental_fans {
            total_incremental += incremental;
        }

        // Aggregate durable (30-day) fans.
        if let Some(durable) = ev.y30_outcome() {
            total_durable += durable;
        }

        // Template attribution.
        let template_key = ev
            .context
            .subreddit_type
            .as_deref()
            .unwrap_or("unknown")
            .to_owned();
        // The template_id is not directly on GrowthEvidence; it's in the
        // prediction context. We use the creative_family or target_key as
        // the template proxy when available, falling back to the
        // subreddit_type. This is a simplification — the full attribution
        // would join back to the dispatch prediction's template_id.
        let template_id = ev
            .creative_family
            .map(|f| format!("{f:?}"))
            .unwrap_or(template_key);

        let template_entry = by_template.entry(template_id.clone()).or_default();
        template_entry.template_id = template_id;
        if let Some(observed) = ev.observed_fans {
            template_entry.observed_fans += observed;
        }
        if let Some(incremental) = ev.observed_incremental_fans {
            template_entry.incremental_fans += incremental;
        }
        if let Some(durable) = ev.y30_outcome() {
            template_entry.durable_fans += durable;
        }
        template_entry.observations += 1;
        if ev.evidence_quality.rank() > template_entry.best_quality.rank() {
            template_entry.best_quality = ev.evidence_quality;
        }

        // Strategy attribution.
        let strategy_key = ev.strategy.clone().unwrap_or_else(|| "unknown".to_owned());
        let strategy_entry = by_strategy.entry(strategy_key.clone()).or_default();
        strategy_entry.strategy = strategy_key;
        if let Some(observed) = ev.observed_fans {
            strategy_entry.observed_fans += observed;
        }
        if let Some(incremental) = ev.observed_incremental_fans {
            strategy_entry.incremental_fans += incremental;
        }
        strategy_entry.observations += 1;

        // Quality attribution.
        let quality_entry = by_quality.entry(ev.evidence_quality).or_default();
        quality_entry.quality = ev.evidence_quality;
        if let Some(incremental) = ev.observed_incremental_fans {
            quality_entry.incremental_fans += incremental;
        }
        quality_entry.observations += 1;
    }

    // Compute means.
    let mut by_template_vec: Vec<TemplateAttribution> = by_template.into_values().collect();
    for t in &mut by_template_vec {
        if t.observations > 0 {
            t.mean_observed_fans = t.observed_fans / f64::from(t.observations);
            t.mean_incremental_fans = t.incremental_fans / f64::from(t.observations);
        }
    }
    by_template_vec.sort_by(|a, b| {
        b.incremental_fans
            .partial_cmp(&a.incremental_fans)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut by_strategy_vec: Vec<StrategyAttribution> = by_strategy.into_values().collect();
    for s in &mut by_strategy_vec {
        if s.observations > 0 {
            s.mean_incremental_fans = s.incremental_fans / f64::from(s.observations);
        }
    }
    by_strategy_vec.sort_by(|a, b| {
        b.incremental_fans
            .partial_cmp(&a.incremental_fans)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut by_quality_vec: Vec<QualityAttribution> = by_quality.into_values().collect();
    by_quality_vec.sort_by_key(|a| std::cmp::Reverse(a.quality.rank()));

    FanGrowthAttribution {
        total_observed_fans: total_observed,
        total_incremental_fans: total_incremental,
        total_durable_fans: total_durable,
        resolved_observations: resolved_count,
        partial_observations: partial_count,
        by_template: by_template_vec,
        by_strategy: by_strategy_vec,
        by_quality: by_quality_vec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TreatmentAssignment;
    use crate::evidence::GrowthEvidence;
    use crate::reach::ReachChannel;

    fn make_evidence(
        observed: Option<f64>,
        incremental: Option<f64>,
        durable: Option<f64>,
        strategy: Option<&str>,
        quality: EvidenceQuality,
    ) -> GrowthEvidence {
        GrowthEvidence {
            observed_fans: observed,
            observed_incremental_fans: incremental,
            durable_fans_30d: durable,
            strategy: strategy.map(|s| s.to_owned()),
            evidence_quality: quality,
            treatment: TreatmentAssignment::Treatment,
            channel: ReachChannel::RedditPost,
            ..GrowthEvidence::default()
        }
    }

    /// Like `make_evidence` but sets the subreddit_type for template grouping.
    fn make_evidence_with_template(
        template: &str,
        observed: Option<f64>,
        incremental: Option<f64>,
        durable: Option<f64>,
        strategy: Option<&str>,
        quality: EvidenceQuality,
    ) -> GrowthEvidence {
        let mut ev = make_evidence(observed, incremental, durable, strategy, quality);
        ev.context.subreddit_type = Some(template.to_owned());
        ev
    }

    #[test]
    fn empty_evidence_produces_zero_attribution() {
        let attr = attribute_fan_growth(&[]);
        assert_eq!(attr.total_observed_fans, 0.0);
        assert_eq!(attr.total_incremental_fans, 0.0);
        assert_eq!(attr.total_durable_fans, 0.0);
        assert_eq!(attr.resolved_observations, 0);
        assert!(attr.by_template.is_empty());
    }

    #[test]
    fn evidence_without_outcomes_is_skipped() {
        let ev = make_evidence(None, None, None, None, EvidenceQuality::Observational);
        let attr = attribute_fan_growth(&[ev]);
        assert_eq!(attr.resolved_observations, 0);
        assert!(attr.by_template.is_empty());
    }

    #[test]
    fn observed_fans_aggregated_correctly() {
        let ev1 = make_evidence(
            Some(10.0),
            Some(5.0),
            None,
            Some("aggressive"),
            EvidenceQuality::Observational,
        );
        let ev2 = make_evidence(
            Some(20.0),
            Some(8.0),
            None,
            Some("aggressive"),
            EvidenceQuality::Observational,
        );
        let attr = attribute_fan_growth(&[ev1, ev2]);
        assert_eq!(attr.total_observed_fans, 30.0);
        assert_eq!(attr.total_incremental_fans, 13.0);
        assert_eq!(attr.resolved_observations, 0); // resolved_at is None
    }

    #[test]
    fn durable_fans_aggregated() {
        let ev = make_evidence(
            Some(10.0),
            Some(5.0),
            Some(3.0),
            None,
            EvidenceQuality::Observational,
        );
        let attr = attribute_fan_growth(&[ev]);
        assert_eq!(attr.total_durable_fans, 3.0);
    }

    #[test]
    fn template_attribution_sorted_by_incremental() {
        let ev1 = make_evidence_with_template(
            "reddit",
            Some(10.0),
            Some(5.0),
            None,
            Some("a"),
            EvidenceQuality::Observational,
        );
        let ev2 = make_evidence_with_template(
            "telegram",
            Some(20.0),
            Some(15.0),
            None,
            Some("b"),
            EvidenceQuality::Observational,
        );
        let attr = attribute_fan_growth(&[ev1, ev2]);
        assert_eq!(attr.by_template.len(), 2);
        // Sorted by incremental descending — telegram (15) comes first.
        assert!(attr.by_template[0].incremental_fans >= attr.by_template[1].incremental_fans);
    }

    #[test]
    fn strategy_attribution_groups_by_strategy() {
        let ev1 = make_evidence(
            Some(10.0),
            Some(5.0),
            None,
            Some("aggressive"),
            EvidenceQuality::Observational,
        );
        let ev2 = make_evidence(
            Some(20.0),
            Some(8.0),
            None,
            Some("aggressive"),
            EvidenceQuality::Observational,
        );
        let ev3 = make_evidence(
            Some(5.0),
            Some(2.0),
            None,
            Some("conservative"),
            EvidenceQuality::Observational,
        );
        let attr = attribute_fan_growth(&[ev1, ev2, ev3]);
        assert_eq!(attr.by_strategy.len(), 2);
        let aggressive = attr
            .by_strategy
            .iter()
            .find(|s| s.strategy == "aggressive")
            .expect("aggressive strategy should exist");
        assert_eq!(aggressive.observations, 2);
        assert_eq!(aggressive.incremental_fans, 13.0);
        assert!((aggressive.mean_incremental_fans - 6.5).abs() < f64::EPSILON);
    }

    #[test]
    fn quality_attribution_groups_by_quality() {
        let ev1 = make_evidence(
            Some(10.0),
            Some(5.0),
            None,
            None,
            EvidenceQuality::Observational,
        );
        let ev2 = make_evidence(
            Some(20.0),
            Some(8.0),
            None,
            None,
            EvidenceQuality::MatchedQuasiExperiment,
        );
        let attr = attribute_fan_growth(&[ev1, ev2]);
        assert_eq!(attr.by_quality.len(), 2);
    }

    #[test]
    fn mean_observed_fans_computed_correctly() {
        let ev1 = make_evidence(Some(10.0), None, None, None, EvidenceQuality::Observational);
        let ev2 = make_evidence(Some(30.0), None, None, None, EvidenceQuality::Observational);
        let attr = attribute_fan_growth(&[ev1, ev2]);
        assert!(!attr.by_template.is_empty());
        let t = &attr.by_template[0];
        assert_eq!(t.observations, 2);
        assert!((t.mean_observed_fans - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn best_quality_tracked_per_template() {
        let ev1 = make_evidence(
            Some(10.0),
            Some(5.0),
            None,
            None,
            EvidenceQuality::Observational,
        );
        let ev2 = make_evidence(
            Some(20.0),
            Some(8.0),
            None,
            None,
            EvidenceQuality::RandomizedHoldout,
        );
        let attr = attribute_fan_growth(&[ev1, ev2]);
        assert_eq!(attr.by_template.len(), 1);
        assert_eq!(
            attr.by_template[0].best_quality,
            EvidenceQuality::RandomizedHoldout
        );
    }

    #[test]
    fn partial_observations_counted() {
        let mut ev = make_evidence(
            Some(10.0),
            Some(5.0),
            None,
            None,
            EvidenceQuality::Observational,
        );
        ev.partial_resolution_count = 1;
        ev.resolved_at = None;
        let attr = attribute_fan_growth(&[ev]);
        assert_eq!(attr.partial_observations, 1);
        assert_eq!(attr.resolved_observations, 0);
    }

    #[test]
    fn resolved_observations_counted() {
        let mut ev = make_evidence(
            Some(10.0),
            Some(5.0),
            None,
            None,
            EvidenceQuality::Observational,
        );
        ev.resolved_at = Some(time::OffsetDateTime::now_utc());
        let attr = attribute_fan_growth(&[ev]);
        assert_eq!(attr.resolved_observations, 1);
    }
}
