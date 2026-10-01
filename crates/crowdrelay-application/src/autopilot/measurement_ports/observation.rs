use super::*;
use crowdrelay_domain::growth_metrics::{MetricDirection, MetricPlatform};
use std::collections::BTreeSet;

/// One comparable release series, in its original units. This is a bounded
/// pre/post contrast, not proof of this action's incremental causal effect.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AutopilotSeriesLift {
    pub series_id: uuid::Uuid,
    pub platform: String,
    pub metric_key: String,
    pub direction: MetricDirection,
    pub lift: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measurement() -> ClaimedAutopilotMeasurement {
        let now = OffsetDateTime::UNIX_EPOCH;
        ClaimedAutopilotMeasurement {
            id: AutopilotMeasurementId::from(uuid::Uuid::nil()),
            action_id: AutopilotActionId::from(uuid::Uuid::nil()),
            kind: AutopilotMeasurementKind::ReleaseChannelLift14d,
            subject_id: uuid::Uuid::nil(),
            baseline_value: 0.0,
            action_finished_at: now,
            due_at: now,
            attempt_number: 1,
        }
    }

    fn series(platform: &str, metric: &str, lift: f64) -> AutopilotSeriesLift {
        AutopilotSeriesLift {
            series_id: uuid::Uuid::nil(),
            platform: platform.to_owned(),
            metric_key: metric.to_owned(),
            direction: MetricDirection::HigherIsBetter,
            lift,
        }
    }

    #[test]
    fn views_cannot_buy_an_aggregate_success_over_lost_followers() {
        let observation = AutopilotMeasurementObservation {
            value: 9990.0,
            series_lifts: vec![
                series("youtube", "views", 10000.0),
                series("spotify", "followers", -10.0),
            ],
        };
        assert_eq!(
            observation
                .assess_effect(&measurement(), &HarmObservation::default())
                .unwrap()
                .assessment,
            EffectAssessment::Neutral
        );
        let harm = HarmObservation {
            complaints: 1.0,
            ..Default::default()
        };
        assert_eq!(
            observation
                .assess_effect(&measurement(), &harm)
                .unwrap()
                .assessment,
            EffectAssessment::Worsened
        );
        assert_ne!(
            observation.series_lifts[0].learning_key(),
            observation.series_lifts[1].learning_key()
        );
    }

    #[test]
    fn lower_is_better_orients_the_verdict_without_rewriting_the_fact() {
        let mut loss = series("youtube", "unsubscribes", -10.0);
        loss.direction = MetricDirection::LowerIsBetter;
        let observation = AutopilotMeasurementObservation {
            value: -10.0,
            series_lifts: vec![loss],
        };
        assert_eq!(
            observation
                .assess_effect(&measurement(), &HarmObservation::default())
                .unwrap()
                .assessment,
            EffectAssessment::Improved
        );
        assert_eq!(observation.series_lifts[0].lift, -10.0);
    }

    #[test]
    fn invalid_dimensions_and_nonfinite_facts_cannot_be_completed() {
        let valid = series("spotify", "followers", 3.0);
        for series_lifts in [
            vec![valid.clone(), valid],
            vec![series("made-up-platform", "followers", 3.0)],
            vec![series("spotify", "", 3.0)],
            vec![series("spotify", "followers", f64::NAN)],
        ] {
            let observation = AutopilotMeasurementObservation {
                value: 3.0,
                series_lifts,
            };
            assert!(!observation.is_valid_for(measurement().kind));
            assert!(
                observation
                    .assess_effect(&measurement(), &HarmObservation::default())
                    .is_none()
            );
        }
        let observation = AutopilotMeasurementObservation {
            value: 3.0,
            series_lifts: vec![series("spotify", "followers", 3.0)],
        };
        assert!(!observation.is_valid_for(AutopilotMeasurementKind::BookingReply7d));
    }
}

impl AutopilotSeriesLift {
    #[must_use]
    pub fn learning_key(&self) -> String {
        format!("release_channel_lift:{}:{}", self.platform, self.metric_key)
    }
}

/// `value` preserves the existing scalar readout. When series are present,
/// their identities and units are the learning facts; their sum is not one.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AutopilotMeasurementObservation {
    pub value: f64,
    pub series_lifts: Vec<AutopilotSeriesLift>,
}

impl AutopilotMeasurementObservation {
    #[must_use]
    pub fn scalar(value: f64) -> Self {
        Self {
            value,
            series_lifts: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_valid_for(&self, kind: AutopilotMeasurementKind) -> bool {
        if !self.value.is_finite()
            || (!self.series_lifts.is_empty()
                && kind != AutopilotMeasurementKind::ReleaseChannelLift14d)
        {
            return false;
        }
        let mut dimensions = BTreeSet::new();
        self.series_lifts.iter().all(|series| {
            series.lift.is_finite()
                && MetricPlatform::parse(&series.platform).is_some()
                && !series.metric_key.trim().is_empty()
                && series.metric_key.chars().count() <= 64
                && dimensions.insert((&series.platform, &series.metric_key))
        })
    }

    #[must_use]
    pub fn assess_effect(
        &self,
        measurement: &ClaimedAutopilotMeasurement,
        harm: &HarmObservation,
    ) -> Option<EffectResult> {
        if !self.is_valid_for(measurement.kind) {
            return None;
        }
        if self.series_lifts.len() > 1 {
            // A view cannot compensate for a lost follower. Preserve the
            // individual facts; do not buy an aggregate success or hide harm.
            return Some(EffectResult {
                assessment: if harm.actionable() {
                    EffectAssessment::Worsened
                } else {
                    EffectAssessment::Neutral
                },
                delta_basis_points: 0,
            });
        }
        let oriented =
            self.series_lifts
                .first()
                .map_or(self.value, |series| match series.direction {
                    MetricDirection::HigherIsBetter => series.lift,
                    MetricDirection::LowerIsBetter => -series.lift,
                });
        assess_measurement_effect(measurement, oriented, harm)
    }
}
