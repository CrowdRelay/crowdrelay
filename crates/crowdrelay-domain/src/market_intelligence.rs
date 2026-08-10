//! Provider-neutral market-intelligence value objects.
//!
//! External adapters report bounded observations only. This domain normalizes
//! fresh signals and prevents one signal family from dominating merely because
//! an integration emitted more rows. No provider, SQL or HTTP concepts live here.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::autonomy::Confidence;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CityMarketSignalKind {
    StreamingMomentum,
    SearchInterest,
    SocialMomentum,
    LiveDemand,
}

impl CityMarketSignalKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StreamingMomentum => "streaming_momentum",
            Self::SearchInterest => "search_interest",
            Self::SocialMomentum => "social_momentum",
            Self::LiveDemand => "live_demand",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::StreamingMomentum => 0,
            Self::SearchInterest => 1,
            Self::SocialMomentum => 2,
            Self::LiveDemand => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CityMarketSignal {
    pub kind: CityMarketSignalKind,
    pub score_basis_points: u16,
    pub confidence: Confidence,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CityMarketEvidence {
    pub score_basis_points: u16,
    pub confidence: Confidence,
    pub signal_families: u8,
}

/// Aggregates fresh observations while giving each signal family equal weight.
/// Within a family, observations are confidence-weighted. This makes the result
/// stable when one provider retries or when many adapters report the same kind.
#[must_use]
pub fn aggregate_city_market_evidence(
    signals: impl IntoIterator<Item = CityMarketSignal>,
    now: OffsetDateTime,
) -> Option<CityMarketEvidence> {
    #[derive(Clone, Copy, Default)]
    struct FamilyAggregate {
        weighted_score: u64,
        confidence_weight: u64,
        confidence_sum: u64,
        count: u32,
    }

    let mut families = [FamilyAggregate::default(); 4];

    for signal in signals {
        if signal.score_basis_points > 10_000
            || signal.observed_at > now
            || signal.expires_at <= now
            || signal.expires_at <= signal.observed_at
        {
            continue;
        }
        let index = signal.kind.index();
        let confidence = u64::from(signal.confidence.basis_points());
        let Some(family) = families.get_mut(index) else {
            continue;
        };
        family.weighted_score = family
            .weighted_score
            .saturating_add(u64::from(signal.score_basis_points).saturating_mul(confidence));
        family.confidence_weight = family.confidence_weight.saturating_add(confidence);
        family.confidence_sum = family.confidence_sum.saturating_add(confidence);
        family.count = family.count.saturating_add(1);
    }

    let mut family_scores = 0_u64;
    let mut family_confidences = 0_u64;
    let mut family_count = 0_u64;
    for family in families {
        if family.count == 0 || family.confidence_weight == 0 {
            continue;
        }
        family_scores =
            family_scores.saturating_add(family.weighted_score / family.confidence_weight);
        family_confidences =
            family_confidences.saturating_add(family.confidence_sum / u64::from(family.count));
        family_count = family_count.saturating_add(1);
    }
    if family_count == 0 {
        return None;
    }

    let score_basis_points = u16::try_from((family_scores / family_count).min(10_000)).ok()?;
    let confidence_basis_points =
        u16::try_from((family_confidences / family_count).min(10_000)).ok()?;
    Some(CityMarketEvidence {
        score_basis_points,
        confidence: Confidence::saturating_from_basis_points(confidence_basis_points),
        signal_families: u8::try_from(family_count).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn signal(kind: CityMarketSignalKind, score: u16, confidence: u16) -> CityMarketSignal {
        CityMarketSignal {
            kind,
            score_basis_points: score,
            confidence: Confidence::saturating_from_basis_points(confidence),
            observed_at: now() - Duration::minutes(5),
            expires_at: now() + Duration::hours(2),
        }
    }

    #[test]
    fn signal_families_are_equal_weight_even_with_duplicate_sources() {
        let evidence = aggregate_city_market_evidence(
            [
                signal(CityMarketSignalKind::StreamingMomentum, 10_000, 10_000),
                signal(CityMarketSignalKind::StreamingMomentum, 10_000, 10_000),
                signal(CityMarketSignalKind::SearchInterest, 0, 10_000),
            ],
            now(),
        );
        assert_eq!(evidence.map(|value| value.score_basis_points), Some(5_000));
        assert_eq!(evidence.map(|value| value.signal_families), Some(2));
    }

    #[test]
    fn expired_and_future_signals_are_ignored() {
        let mut expired = signal(CityMarketSignalKind::LiveDemand, 10_000, 10_000);
        expired.expires_at = now();
        let mut future = signal(CityMarketSignalKind::SocialMomentum, 10_000, 10_000);
        future.observed_at = now() + Duration::minutes(1);
        future.expires_at = now() + Duration::hours(1);
        assert_eq!(
            aggregate_city_market_evidence([expired, future], now()),
            None
        );
    }

    #[test]
    fn zero_confidence_signal_cannot_move_market_evidence() {
        let evidence = aggregate_city_market_evidence(
            [
                signal(CityMarketSignalKind::StreamingMomentum, 10_000, 0),
                signal(CityMarketSignalKind::SearchInterest, 4_000, 8_000),
            ],
            now(),
        );
        assert_eq!(evidence.map(|value| value.score_basis_points), Some(4_000));
        assert_eq!(evidence.map(|value| value.signal_families), Some(1));
    }
}
