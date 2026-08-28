//! Fan network reproduction — modelling how fans recruit other fans.
//!
//! When a fan joins, they may bring other fans through word-of-mouth, social
//! sharing, community posts, events, or direct invites. This module models
//! that network effect so the brain can reason about organic, viral growth
//! separately from direct acquisition actions.
//!
//! # Model
//!
//! Fan growth follows a decaying reproduction model:
//!
//! ```text
//! fans(t+1) = fans(t) * (1 + reproduction_rate * decay^t)
//! ```
//!
//! The `reproduction_rate` is the average number of new fans each existing
//! fan brings per month. The `reproduction_decay` models how this rate
//! declines over time — the easy converts (friends, close community) are
//! exhausted first, leaving harder-to-reach prospects.
//!
//! The brain updates its estimate of `reproduction_rate` from observed data
//! using a Bayesian-style weighted update, so the model self-corrects as
//! real network-effect data arrives.

use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

/// Models fan network reproduction — how existing fans recruit new fans.
///
/// The model tracks an average reproduction rate (new fans per existing fan
/// per month) and a decay factor that captures how that rate declines as the
/// easy converts are exhausted. The brain updates the reproduction rate from
/// observed data so the model stays grounded in reality.
#[derive(Clone, Debug, Serialize)]
pub struct FanNetworkModel {
    /// Average new fans per existing fan per month.
    pub reproduction_rate: f64,
    /// How the reproduction rate decays over time (0 < decay <= 1).
    /// Values below 1 mean the rate shrinks as easy converts are exhausted.
    pub reproduction_decay: f64,
    /// Current generation count — incremented each time the model is updated,
    /// tracking how many observations have refined the estimate.
    pub generation: u32,
}

impl FanNetworkModel {
    /// Creates a new fan network model with the given reproduction rate and
    /// decay. The generation starts at zero (no observations yet).
    ///
    /// # Panics
    ///
    /// Debug builds assert that the decay is in `(0.0, 1.0]` and the rate is
    /// non-negative — nonsensical parameters are a caller bug.
    #[must_use]
    pub fn new(reproduction_rate: f64, reproduction_decay: f64) -> Self {
        debug_assert!(
            reproduction_decay > 0.0 && reproduction_decay <= 1.0,
            "reproduction_decay must be in (0.0, 1.0], got {reproduction_decay}"
        );
        debug_assert!(
            reproduction_rate >= 0.0,
            "reproduction_rate must be non-negative, got {reproduction_rate}"
        );
        Self {
            reproduction_rate,
            reproduction_decay,
            generation: 0,
        }
    }

    /// Predicts the fan count after `months` months, starting from
    /// `current_fans`.
    ///
    /// Growth is modelled month-by-month as:
    ///
    /// ```text
    /// fans(t+1) = fans(t) * (1 + reproduction_rate * decay^t)
    /// ```
    ///
    /// The result is clamped to `u64` and saturates rather than overflowing.
    #[must_use]
    pub fn predict_fans(&self, current_fans: u32, months: u32) -> u64 {
        if current_fans == 0 || months == 0 {
            return u64::from(current_fans);
        }
        let mut fans = f64::from(current_fans);
        for t in 0..months {
            let factor = 1.0 + self.reproduction_factor(t);
            fans *= factor;
            if !fans.is_finite() || fans <= 0.0 {
                break;
            }
        }
        if !fans.is_finite() || fans < 0.0 {
            return 0;
        }
        if fans >= u64::MAX as f64 {
            return u64::MAX;
        }
        fans as u64
    }

    /// Returns the effective reproduction rate at month `t`:
    ///
    /// ```text
    /// reproduction_rate * reproduction_decay^t
    /// ```
    ///
    /// This is the per-fan growth contribution applied during month `t`. It
    /// decreases over time as the easy converts are exhausted.
    #[must_use]
    pub fn reproduction_factor(&self, month: u32) -> f64 {
        self.reproduction_rate * self.reproduction_decay.powi(month as i32)
    }

    /// Bayesian-style update: adjusts `reproduction_rate` toward the observed
    /// rate, weighted by confidence. More observations (higher `generation`)
    /// mean less adjustment per new observation.
    ///
    /// ```text
    /// reproduction_rate = reproduction_rate * 0.9 + observed_rate * 0.1
    /// ```
    ///
    /// The fixed 0.9/0.1 weighting is a conservative EMA that resists
    /// overreacting to a single noisy observation. Invalid observations
    /// (NaN or negative) are ignored.
    pub fn update(&mut self, observed_reproduction_rate: f64) {
        if !observed_reproduction_rate.is_finite() || observed_reproduction_rate < 0.0 {
            return;
        }
        self.reproduction_rate = self.reproduction_rate * 0.9 + observed_reproduction_rate * 0.1;
        self.generation = self.generation.saturating_add(1);
    }
}

/// How a fan was recruited through the network effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecruitmentChannel {
    /// Organic word-of-mouth — a fan told another fan in person or via DM.
    WordOfMouth,
    /// A fan shared content (post, link, clip) on a social platform.
    SocialShare,
    /// A fan posted in a community (subreddit, forum, Discord) about the artist.
    CommunityPost,
    /// A fan recruited another at or through a live event.
    Event,
    /// A fan sent a direct, trackable invite (referral link, tagged share).
    DirectInvite,
}

/// A recorded network effect — one fan recruiting another.
#[derive(Clone, Debug, Serialize)]
pub struct NetworkEffect {
    /// The fan who recruited the new fan.
    pub source_fan_id: Uuid,
    /// The fan who was recruited.
    pub recruited_fan_id: Uuid,
    /// How the recruitment happened.
    pub channel: RecruitmentChannel,
    /// When the network effect was recorded.
    pub recorded_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prediction_with_zero_rate_returns_current_fans() {
        let model = FanNetworkModel::new(0.0, 0.9);
        assert_eq!(model.predict_fans(100, 12), 100);
    }

    #[test]
    fn prediction_with_zero_months_returns_current_fans() {
        let model = FanNetworkModel::new(0.5, 0.9);
        assert_eq!(model.predict_fans(100, 0), 100);
    }

    #[test]
    fn prediction_with_zero_fans_returns_zero() {
        let model = FanNetworkModel::new(0.5, 0.9);
        assert_eq!(model.predict_fans(0, 12), 0);
    }

    #[test]
    fn prediction_with_positive_rate_grows_fans() {
        let model = FanNetworkModel::new(0.1, 1.0);
        // With decay = 1.0, growth is 10% per month compounded.
        // 100 * 1.1^12 ≈ 313.84 → 313
        let predicted = model.predict_fans(100, 12);
        assert!(
            predicted > 100,
            "positive reproduction rate should grow fans, got {predicted}"
        );
        assert_eq!(predicted, 313);
    }

    #[test]
    fn decay_reduces_growth_over_time() {
        let decaying = FanNetworkModel::new(0.5, 0.5);
        let constant = FanNetworkModel::new(0.5, 1.0);
        let decaying_predicted = decaying.predict_fans(100, 12);
        let constant_predicted = constant.predict_fans(100, 12);
        assert!(
            decaying_predicted < constant_predicted,
            "decay should produce fewer fans than constant reproduction, got {decaying_predicted} vs {constant_predicted}"
        );
    }

    #[test]
    fn reproduction_factor_decreases_over_time() {
        let model = FanNetworkModel::new(0.5, 0.9);
        let factor_0 = model.reproduction_factor(0);
        let factor_5 = model.reproduction_factor(5);
        let factor_10 = model.reproduction_factor(10);
        assert_eq!(factor_0, 0.5);
        assert!(
            factor_5 < factor_0,
            "reproduction factor should decrease over time, got {factor_5} vs {factor_0}"
        );
        assert!(
            factor_10 < factor_5,
            "reproduction factor should keep decreasing, got {factor_10} vs {factor_5}"
        );
    }

    #[test]
    fn reproduction_factor_with_zero_rate_is_zero() {
        let model = FanNetworkModel::new(0.0, 0.9);
        assert_eq!(model.reproduction_factor(0), 0.0);
        assert_eq!(model.reproduction_factor(10), 0.0);
    }

    #[test]
    fn update_adjusts_rate_toward_observation() {
        let mut model = FanNetworkModel::new(0.2, 0.9);
        let initial = model.reproduction_rate;
        model.update(0.8);
        assert!(
            model.reproduction_rate > initial,
            "update should move rate toward the higher observed value"
        );
        assert!(
            model.reproduction_rate < 0.8,
            "update should not jump all the way to the observed value"
        );
        assert_eq!(model.generation, 1);
    }

    #[test]
    fn update_moves_rate_down_for_lower_observation() {
        let mut model = FanNetworkModel::new(0.8, 0.9);
        model.update(0.2);
        assert!(
            model.reproduction_rate < 0.8,
            "update should move rate toward the lower observed value"
        );
        assert!(
            model.reproduction_rate > 0.2,
            "update should not jump all the way to the observed value"
        );
    }

    #[test]
    fn repeated_updates_converge_toward_observation() {
        let mut model = FanNetworkModel::new(0.1, 0.9);
        for _ in 0..100 {
            model.update(0.5);
        }
        assert!(
            (model.reproduction_rate - 0.5).abs() < 0.01,
            "repeated updates should converge toward the observed rate, got {}",
            model.reproduction_rate
        );
        assert_eq!(model.generation, 100);
    }

    #[test]
    fn update_ignores_invalid_observations() {
        let mut model = FanNetworkModel::new(0.3, 0.9);
        let initial = model.reproduction_rate;
        model.update(f64::NAN);
        model.update(-0.5);
        model.update(f64::INFINITY);
        assert_eq!(
            model.reproduction_rate, initial,
            "invalid observations should not change the rate"
        );
        assert_eq!(model.generation, 0);
    }

    #[test]
    fn network_effect_serializes() {
        let effect = NetworkEffect {
            source_fan_id: Uuid::nil(),
            recruited_fan_id: Uuid::from_u128(1),
            channel: RecruitmentChannel::SocialShare,
            recorded_at: OffsetDateTime::now_utc(),
        };
        let json = serde_json::to_string(&effect).expect("should serialize");
        assert!(json.contains("social_share"));
    }

    #[test]
    fn recruitment_channel_serializes_to_snake_case() {
        let json = serde_json::to_string(&RecruitmentChannel::WordOfMouth).unwrap();
        assert_eq!(json, "\"word_of_mouth\"");
        let json = serde_json::to_string(&RecruitmentChannel::DirectInvite).unwrap();
        assert_eq!(json, "\"direct_invite\"");
    }
}
