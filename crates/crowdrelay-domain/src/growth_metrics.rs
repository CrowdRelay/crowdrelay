//! Normalized external growth metrics, their trends, and the deterministic rule
//! that turns a trend into an actionable opportunity.
//!
//! Everything external platforms report is reduced to the same shape: a series
//! identity plus absolute observations on a timeline. Deltas, velocity, the
//! baseline and the anomaly are derived here from those observations, so this
//! module is the single place that decides what "the number moved" means. It
//! contains no provider, SQL, HTTP or scheduling concept, holds no opinion
//! about how an opportunity is executed, and never claims a cause: it reports
//! that a tracked number left its own recent baseline and how confident that
//! statement is, given the coverage actually present in the window.
//!
//! All arithmetic is integer arithmetic. Rates are carried in milli-units per
//! day and comparisons in basis points, matching the rest of the domain.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{GrowthMetricSeriesId, autonomy::Confidence};

/// Scale applied to per-day rates so integer division keeps three decimals.
const RATE_SCALE: i64 = 1_000;

/// External platform a series is observed on. The list is the set of platforms
/// the ingestion contract accepts today; adding one is a migration plus a match
/// arm, never a new subsystem.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricPlatform {
    Spotify,
    YouTube,
    Bandsintown,
    Social,
    Website,
    Ticketing,
    Signal,
    Merch,
}

impl MetricPlatform {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spotify => "spotify",
            Self::YouTube => "youtube",
            Self::Bandsintown => "bandsintown",
            Self::Social => "social",
            Self::Website => "website",
            Self::Ticketing => "ticketing",
            Self::Signal => "signal",
            Self::Merch => "merch",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "spotify" => Some(Self::Spotify),
            "youtube" => Some(Self::YouTube),
            "bandsintown" => Some(Self::Bandsintown),
            "social" => Some(Self::Social),
            "website" => Some(Self::Website),
            "ticketing" => Some(Self::Ticketing),
            "signal" => Some(Self::Signal),
            "merch" => Some(Self::Merch),
            _ => None,
        }
    }
}

/// Whether a rising number is the good outcome. Refunds, unsubscribes and churn
/// are tracked as `LowerIsBetter` so one trend implementation serves both.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDirection {
    HigherIsBetter,
    LowerIsBetter,
}

impl MetricDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HigherIsBetter => "higher_is_better",
            Self::LowerIsBetter => "lower_is_better",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "higher_is_better" => Some(Self::HigherIsBetter),
            "lower_is_better" => Some(Self::LowerIsBetter),
            _ => None,
        }
    }

    /// Rewrites a raw movement so that positive always means "better".
    const fn orient(self, value: i64) -> i64 {
        match self {
            Self::HigherIsBetter => value,
            Self::LowerIsBetter => value.saturating_neg(),
        }
    }
}

/// How close a metric sits to value the business actually banks. The engine
/// uses this to refuse to spend attention on a vanity spike while a stronger
/// downstream metric for the same platform is being tracked.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricValueTier {
    /// Reach and follower counts: real, but far from an outcome.
    Vanity,
    /// Intent: saves, trackers, listing interest, session depth.
    Intermediate,
    /// Banked outcomes: tickets, attendance, merch, retained fans.
    Downstream,
}

impl MetricValueTier {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vanity => "vanity",
            Self::Intermediate => "intermediate",
            Self::Downstream => "downstream",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "vanity" => Some(Self::Vanity),
            "intermediate" => Some(Self::Intermediate),
            "downstream" => Some(Self::Downstream),
            _ => None,
        }
    }

    const fn weight(self) -> u16 {
        match self {
            Self::Vanity => 20,
            Self::Intermediate => 55,
            Self::Downstream => 100,
        }
    }
}

/// One absolute observation. Never a delta: a delta would make a missed
/// snapshot unrecoverable, and re-ingesting it would double-count.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MetricPoint {
    pub captured_at: OffsetDateTime,
    pub value: i64,
}

/// Derived movement of one series. Raw and direction-agnostic: orientation is
/// applied by the evaluator, so this stays a faithful description of the number.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MetricTrend {
    pub latest_value: i64,
    pub latest_at: OffsetDateTime,
    /// `None` when the window has no observation old enough to compare against.
    pub delta_24h: Option<i64>,
    pub delta_7d: Option<i64>,
    pub delta_28d: Option<i64>,
    /// Movement over the last 7 days, in milli-units per day.
    pub velocity_milli_per_day: Option<i64>,
    /// Movement over the 21 days preceding the last 7, in milli-units per day.
    pub baseline_milli_per_day: Option<i64>,
    /// Recent velocity expressed against the baseline. `10_000` is exactly on
    /// baseline. Only produced when the oriented baseline is meaningfully
    /// positive, because a ratio against a flat or negative baseline says
    /// nothing.
    pub velocity_ratio_basis_points: Option<u32>,
    pub points_in_window: u32,
    /// Age of the newest observation. Feeds the dead-feed check.
    pub age_seconds: i64,
}

/// Returns the newest value at or before `target`, provided some observation is
/// close enough to `target` to stand in for it.
fn value_at(points: &[MetricPoint], target: OffsetDateTime, tolerance: Duration) -> Option<i64> {
    let floor = target - tolerance;
    points
        .iter()
        .filter(|point| point.captured_at <= target && point.captured_at >= floor)
        .max_by_key(|point| point.captured_at)
        .map(|point| point.value)
}

const fn rate_milli_per_day(delta: i64, days: i64) -> Option<i64> {
    if days <= 0 {
        return None;
    }
    Some(delta.saturating_mul(RATE_SCALE) / days)
}

/// Derives the trend of one series from its observation window.
///
/// `points` may arrive in any order and may contain observations outside the
/// window; anything at or before `now` is considered and everything else is
/// ignored. Returns `None` when the window holds no usable observation.
#[must_use]
pub fn compute_trend(points: &[MetricPoint], now: OffsetDateTime) -> Option<MetricTrend> {
    let usable: Vec<MetricPoint> = points
        .iter()
        .copied()
        .filter(|point| point.captured_at <= now)
        .collect();
    let latest = usable.iter().max_by_key(|point| point.captured_at)?;

    let delta_for = |window: Duration, tolerance: Duration| -> Option<i64> {
        value_at(&usable, now - window, tolerance).map(|past| latest.value.saturating_sub(past))
    };

    let delta_24h = delta_for(Duration::hours(24), Duration::hours(6));
    let delta_7d = delta_for(Duration::days(7), Duration::hours(42));
    let delta_28d = delta_for(Duration::days(28), Duration::days(7));

    let velocity_milli_per_day = delta_7d.and_then(|delta| rate_milli_per_day(delta, 7));
    // The baseline deliberately excludes the last 7 days so a live anomaly
    // cannot raise the bar it is being measured against.
    let baseline_milli_per_day = match (delta_28d, delta_7d) {
        (Some(long), Some(short)) => rate_milli_per_day(long.saturating_sub(short), 21),
        _ => None,
    };

    let points_in_window = u32::try_from(
        usable
            .iter()
            .filter(|point| point.captured_at >= now - Duration::days(28))
            .count(),
    )
    .unwrap_or(u32::MAX);

    Some(MetricTrend {
        latest_value: latest.value,
        latest_at: latest.captured_at,
        delta_24h,
        delta_7d,
        delta_28d,
        velocity_milli_per_day,
        baseline_milli_per_day,
        // Orientation is unknown here, so the ratio is computed by the
        // evaluator, which knows whether up is good.
        velocity_ratio_basis_points: None,
        points_in_window,
        age_seconds: (now - latest.captured_at).whole_seconds(),
    })
}

/// Everything the rule needs about one tracked series at evaluation time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GrowthMetricSnapshot {
    pub series_id: GrowthMetricSeriesId,
    pub platform: MetricPlatform,
    pub metric_key: String,
    pub direction: MetricDirection,
    pub value_tier: MetricValueTier,
    /// Reporting cadence the operator declared for this series.
    pub expected_interval_hours: u32,
    pub trend: MetricTrend,
    /// Hours since this series last produced an Autopilot decision. `None` when
    /// it never has. Prevents the same anomaly from being raised every cycle.
    pub hours_since_last_signal: Option<u32>,
    /// True when the same platform also has a tracked series at a strictly
    /// stronger value tier.
    pub stronger_tier_tracked: bool,
}

/// Tunable thresholds for the `growth_metrics` context.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct GrowthMetricPolicy {
    /// Observations required in the 28-day window before any anomaly is trusted.
    pub minimum_points_in_window: u32,
    /// A feed is dead once it is this many basis points past its declared
    /// interval. `25_000` is 2.5 intervals.
    pub stale_interval_basis_points: u32,
    /// Recent velocity at or below this share of baseline is a stall.
    pub stall_ratio_basis_points: u32,
    /// Recent velocity at or above this share of baseline is a surge.
    pub surge_ratio_basis_points: u32,
    /// Movements smaller than this in absolute units are noise, whatever the
    /// ratio says. Protects small series from percentage theatre.
    pub minimum_absolute_delta: i64,
    /// Baselines flatter than this (milli-units per day) cannot support a ratio.
    pub minimum_baseline_milli_per_day: i64,
    /// Hours a series stays quiet after it produced a decision.
    pub cooldown_hours: u32,
}

impl Default for GrowthMetricPolicy {
    fn default() -> Self {
        Self {
            minimum_points_in_window: 8,
            stale_interval_basis_points: 25_000,
            stall_ratio_basis_points: 4_000,
            surge_ratio_basis_points: 20_000,
            minimum_absolute_delta: 25,
            minimum_baseline_milli_per_day: 1_000,
            cooldown_hours: 168,
        }
    }
}

/// What the evidence says, in the vocabulary an operator can act on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthSignal {
    /// The number is moving the wrong way outright.
    Decline,
    /// Still moving the right way, but far slower than its own baseline.
    Stall,
    /// Moving the right way far faster than its own baseline.
    Surge,
    /// No observation arrived when one was expected. Growth debt, not a trend.
    StaleFeed,
}

impl GrowthSignal {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decline => "decline",
            Self::Stall => "stall",
            Self::Surge => "surge",
            Self::StaleFeed => "stale_feed",
        }
    }

    /// Why this is worth an operator's attention. Deliberately describes the
    /// evidence and never a cause.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Decline => {
                "a tracked external metric is moving against its own direction of value"
            }
            Self::Stall => "a tracked external metric is growing far below its own 28-day baseline",
            Self::Surge => "a tracked external metric is growing far above its own 28-day baseline",
            Self::StaleFeed => {
                "a tracked external metric stopped reporting past its declared interval"
            }
        }
    }

    /// The class of response. Concrete playbooks are template concerns; the
    /// domain refuses to assume a platform can do something it was never told
    /// it can do.
    #[must_use]
    pub const fn recommended_action(self) -> &'static str {
        match self {
            Self::Decline => "investigate_decline",
            Self::Stall => "revive_stalled_channel",
            Self::Surge => "amplify_outperformance",
            Self::StaleFeed => "repair_metric_feed",
        }
    }

    #[must_use]
    pub const fn template_key(self) -> &'static str {
        match self {
            Self::Decline => "growth_metric_decline",
            Self::Stall => "growth_metric_stall",
            Self::Surge => "growth_metric_surge",
            Self::StaleFeed => "growth_metric_stale_feed",
        }
    }

    const fn base_priority(self) -> u16 {
        match self {
            Self::Decline => 80,
            Self::Stall => 55,
            Self::Surge => 65,
            Self::StaleFeed => 45,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct GrowthOpportunity {
    pub signal: GrowthSignal,
    pub confidence: Confidence,
    /// Priority in `0..=100`, combining the signal class with how far the metric
    /// sits from its baseline and how close it is to banked value.
    pub priority: u16,
    /// Size of the deviation from baseline, in basis points. This is the
    /// measured magnitude of the anomaly, not a forecast and not a currency
    /// amount: the domain has no evidence for either.
    pub deviation_basis_points: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum GrowthMetricDecision {
    Hold,
    Raise(GrowthOpportunity),
}

fn clamp_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn confidence_from(
    coverage_points: u32,
    required_points: u32,
    deviation_basis_points: u32,
) -> Confidence {
    // Coverage dominates: a large deviation computed from three observations is
    // not a finding, it is a gap in the data.
    let required = required_points.max(1);
    let coverage = u32::from(
        u16::try_from(coverage_points.min(required.saturating_mul(2))).unwrap_or(u16::MAX),
    );
    let coverage_bp = coverage
        .saturating_mul(6_000)
        .checked_div(required.saturating_mul(2))
        .unwrap_or(0)
        .min(6_000);
    let strength_bp = deviation_basis_points.saturating_div(5).min(4_000);
    Confidence::saturating_from_basis_points(
        u16::try_from(coverage_bp.saturating_add(strength_bp)).unwrap_or(u16::MAX),
    )
}

fn priority_from(signal: GrowthSignal, tier: MetricValueTier, deviation_basis_points: u32) -> u16 {
    let magnitude = u16::try_from(deviation_basis_points / 2_000)
        .unwrap_or(u16::MAX)
        .min(10);
    let tier_bonus = tier.weight() / 10;
    signal
        .base_priority()
        .saturating_add(magnitude)
        .saturating_add(tier_bonus)
        .min(100)
}

/// Decides whether one series is currently worth acting on.
///
/// Order matters. A dead feed is checked first, because trend arithmetic over
/// stale observations describes the past and would quietly outrank the fact
/// that the input stopped arriving.
#[must_use]
pub fn evaluate_growth_metric(
    snapshot: &GrowthMetricSnapshot,
    policy: GrowthMetricPolicy,
    now: OffsetDateTime,
) -> GrowthMetricDecision {
    let _ = now;
    if snapshot
        .hours_since_last_signal
        .is_some_and(|hours| hours < policy.cooldown_hours)
    {
        return GrowthMetricDecision::Hold;
    }

    let trend = snapshot.trend;

    let expected_seconds = i64::from(snapshot.expected_interval_hours).saturating_mul(3_600);
    let stale_after = expected_seconds
        .saturating_mul(i64::from(policy.stale_interval_basis_points))
        .saturating_div(10_000);
    if stale_after > 0 && trend.age_seconds > stale_after {
        let overdue_bp = clamp_u32(
            trend
                .age_seconds
                .saturating_mul(10_000)
                .saturating_div(expected_seconds.max(1)),
        );
        return GrowthMetricDecision::Raise(GrowthOpportunity {
            signal: GrowthSignal::StaleFeed,
            // A missing feed is a fact about our own pipeline, so confidence
            // comes from how overdue it is rather than from series coverage.
            confidence: Confidence::saturating_from_basis_points(
                u16::try_from(5_000_u32.saturating_add(overdue_bp / 10).min(10_000))
                    .unwrap_or(u16::MAX),
            ),
            priority: priority_from(GrowthSignal::StaleFeed, snapshot.value_tier, overdue_bp),
            deviation_basis_points: overdue_bp,
        });
    }

    if trend.points_in_window < policy.minimum_points_in_window {
        return GrowthMetricDecision::Hold;
    }

    // Refusing to spend attention on reach while a banked-value series for the
    // same platform is tracked. The stronger series will speak for itself.
    if snapshot.stronger_tier_tracked && snapshot.value_tier == MetricValueTier::Vanity {
        return GrowthMetricDecision::Hold;
    }

    let (Some(velocity), Some(baseline), Some(delta_7d)) = (
        trend.velocity_milli_per_day,
        trend.baseline_milli_per_day,
        trend.delta_7d,
    ) else {
        return GrowthMetricDecision::Hold;
    };

    let oriented_velocity = snapshot.direction.orient(velocity);
    let oriented_baseline = snapshot.direction.orient(baseline);
    let oriented_delta = snapshot.direction.orient(delta_7d);

    if oriented_delta.saturating_abs() < policy.minimum_absolute_delta {
        return GrowthMetricDecision::Hold;
    }

    // Outright reversal does not need a baseline to be meaningful.
    if oriented_velocity < 0 {
        let deviation = if oriented_baseline > 0 {
            clamp_u32(
                oriented_velocity
                    .saturating_sub(oriented_baseline)
                    .saturating_abs()
                    .saturating_mul(10_000)
                    .saturating_div(oriented_baseline),
            )
        } else {
            10_000
        };
        return GrowthMetricDecision::Raise(GrowthOpportunity {
            signal: GrowthSignal::Decline,
            confidence: confidence_from(
                trend.points_in_window,
                policy.minimum_points_in_window,
                deviation,
            ),
            priority: priority_from(GrowthSignal::Decline, snapshot.value_tier, deviation),
            deviation_basis_points: deviation,
        });
    }

    if oriented_baseline < policy.minimum_baseline_milli_per_day {
        // Nothing to deviate from. Reporting a ratio here would be arithmetic
        // theatre on a flat series.
        return GrowthMetricDecision::Hold;
    }

    let ratio = clamp_u32(
        oriented_velocity
            .saturating_mul(10_000)
            .saturating_div(oriented_baseline),
    );
    let signal = if ratio <= policy.stall_ratio_basis_points {
        GrowthSignal::Stall
    } else if ratio >= policy.surge_ratio_basis_points {
        GrowthSignal::Surge
    } else {
        return GrowthMetricDecision::Hold;
    };
    let deviation = ratio.abs_diff(10_000);

    GrowthMetricDecision::Raise(GrowthOpportunity {
        signal,
        confidence: confidence_from(
            trend.points_in_window,
            policy.minimum_points_in_window,
            deviation,
        ),
        priority: priority_from(signal, snapshot.value_tier, deviation),
        deviation_basis_points: deviation,
    })
}

/// Convenience for read models: the direction-aware ratio a trend implies.
#[must_use]
pub fn velocity_ratio_basis_points(
    trend: MetricTrend,
    direction: MetricDirection,
    minimum_baseline_milli_per_day: i64,
) -> Option<u32> {
    let velocity = direction.orient(trend.velocity_milli_per_day?);
    let baseline = direction.orient(trend.baseline_milli_per_day?);
    if baseline < minimum_baseline_milli_per_day.max(1) {
        return None;
    }
    Some(clamp_u32(
        velocity.saturating_mul(10_000).saturating_div(baseline),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series() -> GrowthMetricSeriesId {
        GrowthMetricSeriesId::from_uuid(uuid::Uuid::from_u128(7))
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_770_000_000).unwrap()
    }

    /// Daily observations over 30 days: `value(day)` gives the level at
    /// `now - (29 - day) days`.
    fn daily(values: impl Fn(i64) -> i64) -> Vec<MetricPoint> {
        (0..30)
            .map(|day| MetricPoint {
                captured_at: now() - Duration::days(29 - day),
                value: values(day),
            })
            .collect()
    }

    fn snapshot(points: &[MetricPoint], tier: MetricValueTier) -> GrowthMetricSnapshot {
        GrowthMetricSnapshot {
            series_id: series(),
            platform: MetricPlatform::Spotify,
            metric_key: "followers".to_owned(),
            direction: MetricDirection::HigherIsBetter,
            value_tier: tier,
            expected_interval_hours: 24,
            trend: compute_trend(points, now()).unwrap(),
            hours_since_last_signal: None,
            stronger_tier_tracked: false,
        }
    }

    #[test]
    fn trend_derives_deltas_velocity_and_baseline() {
        let points = daily(|day| 1_000 + day * 100);
        let trend = compute_trend(&points, now()).unwrap();

        assert_eq!(trend.latest_value, 3_900);
        assert_eq!(trend.delta_24h, Some(100));
        assert_eq!(trend.delta_7d, Some(700));
        assert_eq!(trend.delta_28d, Some(2_800));
        assert_eq!(trend.velocity_milli_per_day, Some(100_000));
        assert_eq!(trend.baseline_milli_per_day, Some(100_000));
        assert_eq!(trend.points_in_window, 29);
    }

    #[test]
    fn missing_history_reports_absent_windows_instead_of_zero() {
        let points = vec![
            MetricPoint {
                captured_at: now() - Duration::hours(24),
                value: 500,
            },
            MetricPoint {
                captured_at: now(),
                value: 540,
            },
        ];
        let trend = compute_trend(&points, now()).unwrap();

        assert_eq!(trend.delta_24h, Some(40));
        assert_eq!(trend.delta_7d, None);
        assert_eq!(trend.delta_28d, None);
        assert_eq!(trend.baseline_milli_per_day, None);
    }

    #[test]
    fn future_observations_are_ignored() {
        let mut points = daily(|day| 1_000 + day * 100);
        points.push(MetricPoint {
            captured_at: now() + Duration::days(2),
            value: 99_999,
        });
        let trend = compute_trend(&points, now()).unwrap();

        assert_eq!(trend.latest_value, 3_900);
    }

    #[test]
    fn steady_growth_raises_nothing() {
        let points = daily(|day| 1_000 + day * 100);
        let snapshot = snapshot(&points, MetricValueTier::Intermediate);

        assert_eq!(
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now()),
            GrowthMetricDecision::Hold
        );
    }

    #[test]
    fn collapse_against_a_healthy_baseline_is_a_stall() {
        // +100/day for 22 days, then +5/day.
        let points = daily(|day| {
            if day <= 22 {
                1_000 + day * 100
            } else {
                3_200 + (day - 22) * 5
            }
        });
        let snapshot = snapshot(&points, MetricValueTier::Intermediate);

        let GrowthMetricDecision::Raise(opportunity) =
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now())
        else {
            panic!("a 95% velocity collapse must be raised");
        };
        assert_eq!(opportunity.signal, GrowthSignal::Stall);
        assert!(opportunity.deviation_basis_points > 9_000);
    }

    #[test]
    fn reversal_is_a_decline_even_without_a_positive_baseline() {
        let points = daily(|day| {
            if day <= 22 {
                3_000
            } else {
                3_000 - (day - 22) * 60
            }
        });
        let snapshot = snapshot(&points, MetricValueTier::Downstream);

        let GrowthMetricDecision::Raise(opportunity) =
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now())
        else {
            panic!("a falling downstream metric must be raised");
        };
        assert_eq!(opportunity.signal, GrowthSignal::Decline);
        assert!(opportunity.priority >= 90);
    }

    #[test]
    fn lower_is_better_series_treats_a_rise_as_decline() {
        let points = daily(|day| {
            if day <= 22 {
                100
            } else {
                100 + (day - 22) * 60
            }
        });
        let mut snapshot = snapshot(&points, MetricValueTier::Downstream);
        snapshot.direction = MetricDirection::LowerIsBetter;
        snapshot.metric_key = "refunds".to_owned();

        let GrowthMetricDecision::Raise(opportunity) =
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now())
        else {
            panic!("rising refunds must be raised");
        };
        assert_eq!(opportunity.signal, GrowthSignal::Decline);
    }

    #[test]
    fn outperformance_is_a_surge() {
        let points = daily(|day| {
            if day <= 22 {
                1_000 + day * 20
            } else {
                1_440 + (day - 22) * 300
            }
        });
        let snapshot = snapshot(&points, MetricValueTier::Intermediate);

        let GrowthMetricDecision::Raise(opportunity) =
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now())
        else {
            panic!("a 15x velocity jump must be raised");
        };
        assert_eq!(opportunity.signal, GrowthSignal::Surge);
    }

    #[test]
    fn tiny_absolute_movement_is_noise_whatever_the_ratio() {
        let points = daily(|day| if day <= 22 { 40 + day } else { 62 });
        let snapshot = snapshot(&points, MetricValueTier::Intermediate);

        assert_eq!(
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now()),
            GrowthMetricDecision::Hold
        );
    }

    #[test]
    fn thin_coverage_holds_instead_of_guessing() {
        // Fresh at the head so the dead-feed check stays out of the way, but
        // only four observations across the window.
        let points: Vec<MetricPoint> = daily(|day| 1_000 + day * 100)
            .into_iter()
            .filter(|point| {
                let age_days = (now() - point.captured_at).whole_days();
                matches!(age_days, 0 | 7 | 21 | 27)
            })
            .collect();
        let snapshot = snapshot(&points, MetricValueTier::Intermediate);

        assert_eq!(
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now()),
            GrowthMetricDecision::Hold
        );
    }

    #[test]
    fn a_dead_feed_outranks_the_trend_it_would_have_produced() {
        let points: Vec<MetricPoint> =
            daily(|day| if day <= 22 { 1_000 + day * 100 } else { 3_200 })
                .into_iter()
                .filter(|point| point.captured_at <= now() - Duration::days(4))
                .collect();
        let snapshot = snapshot(&points, MetricValueTier::Intermediate);

        let GrowthMetricDecision::Raise(opportunity) =
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now())
        else {
            panic!("a feed 4 days past a 24h cadence must be raised");
        };
        assert_eq!(opportunity.signal, GrowthSignal::StaleFeed);
    }

    #[test]
    fn cooldown_suppresses_a_repeat_of_the_same_finding() {
        let points = daily(|day| {
            if day <= 22 {
                1_000 + day * 100
            } else {
                3_200 + (day - 22) * 5
            }
        });
        let mut snapshot = snapshot(&points, MetricValueTier::Intermediate);
        snapshot.hours_since_last_signal = Some(12);

        assert_eq!(
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now()),
            GrowthMetricDecision::Hold
        );
    }

    #[test]
    fn vanity_series_yields_to_a_stronger_tracked_metric() {
        let points = daily(|day| {
            if day <= 22 {
                1_000 + day * 100
            } else {
                3_200 + (day - 22) * 5
            }
        });
        let mut snapshot = snapshot(&points, MetricValueTier::Vanity);
        snapshot.stronger_tier_tracked = true;

        assert_eq!(
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now()),
            GrowthMetricDecision::Hold
        );

        snapshot.stronger_tier_tracked = false;
        assert!(matches!(
            evaluate_growth_metric(&snapshot, GrowthMetricPolicy::default(), now()),
            GrowthMetricDecision::Raise(_)
        ));
    }

    #[test]
    fn downstream_metrics_outrank_vanity_at_equal_deviation() {
        let points = daily(|day| {
            if day <= 22 {
                1_000 + day * 100
            } else {
                3_200 + (day - 22) * 5
            }
        });
        let vanity = snapshot(&points, MetricValueTier::Vanity);
        let downstream = snapshot(&points, MetricValueTier::Downstream);

        let (
            GrowthMetricDecision::Raise(vanity_opportunity),
            GrowthMetricDecision::Raise(downstream_opportunity),
        ) = (
            evaluate_growth_metric(&vanity, GrowthMetricPolicy::default(), now()),
            evaluate_growth_metric(&downstream, GrowthMetricPolicy::default(), now()),
        )
        else {
            panic!("both tiers must raise the same stall");
        };
        assert_eq!(
            vanity_opportunity.deviation_basis_points,
            downstream_opportunity.deviation_basis_points
        );
        assert!(downstream_opportunity.priority > vanity_opportunity.priority);
    }
}
