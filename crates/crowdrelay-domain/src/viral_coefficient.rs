//! K: how many retained fans one activated fan brings.
//!
//! > **K** = qualified, retained fans created per activated fan.
//!
//! K above 1 means the fanbase grows itself; below 1 the system has to top the
//! funnel up from outside, which is the whole job of the scouting lanes. It is
//! the one number that says whether the person layer is working, and it is
//! computed here only from facts the system owns — referral attributions that
//! *qualified*, and referred fans who were still doing something a month later
//! — never from clicks, shares, invites sent or taps.
//!
//! Two series are reported, because they answer different questions:
//!
//! - **fans**: over every fan who was active a window ago. This is the organic
//!   coefficient of the whole fanbase.
//! - **latarnik**: over the fans who now hold an active Latarnik role. This is
//!   what the role is *for*; comparing the two says whether asking people to
//!   carry the band changed anything beyond what fans were already doing.
//!
//! **A number below its evidence floor is `None`, not a small number.** With a
//! cohort of three fans, "K = 0.33" is one referral and says nothing about the
//! system; reporting it would let a dashboard rank on noise (the project's own
//! rule for rates: below the floor, the answer is "insufficient evidence").

use serde::Serialize;

/// Fewer fans than this in a cohort and K is not reported.
pub const MIN_COHORT: u32 = 4;

/// Counts behind one series. All of them are first-party rows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct KCounts {
    /// Fans in the cohort: active a window ago, so their referrals have had a
    /// full window to arrive and to be retained.
    pub cohort: u32,
    /// Qualified referrals the cohort made in the measured window.
    pub qualified_referrals: u32,
    /// Of those, referred fans who are themselves active now.
    pub retained_referrals: u32,
}

/// One reported series. `k_milli` is K × 1000 (so 1.5 is 1500) to keep the wire
/// integer-valued; `None` below the evidence floor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct KReading {
    pub counts: KCounts,
    pub k_milli: Option<u32>,
    /// Why `k_milli` is absent, in words an operator can act on.
    pub withheld: Option<Withheld>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Withheld {
    /// The cohort is smaller than [`MIN_COHORT`].
    CohortBelowFloor,
    /// More retained referrals than qualified ones: the counts disagree, so
    /// neither is trusted. Fails closed rather than reporting an impossible K.
    ContradictoryCounts,
}

/// K for one series. Retained can never exceed qualified (a retained referral
/// *is* a qualified one); if it does, the inputs are contradictory and nothing
/// is reported.
#[must_use]
pub fn read(counts: KCounts) -> KReading {
    if counts.retained_referrals > counts.qualified_referrals {
        return KReading {
            counts,
            k_milli: None,
            withheld: Some(Withheld::ContradictoryCounts),
        };
    }
    if counts.cohort < MIN_COHORT {
        return KReading {
            counts,
            k_milli: None,
            withheld: Some(Withheld::CohortBelowFloor),
        };
    }
    let k = u64::from(counts.retained_referrals) * 1_000 / u64::from(counts.cohort);
    KReading {
        counts,
        k_milli: Some(u32::try_from(k).unwrap_or(u32::MAX)),
        withheld: None,
    }
}

/// Whether the system is self-sustaining at this reading. `None` when K is
/// withheld: an unknown is neither yes nor no.
#[must_use]
pub fn self_sustaining(reading: &KReading) -> Option<bool> {
    reading.k_milli.map(|k| k > 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(cohort: u32, qualified: u32, retained: u32) -> KCounts {
        KCounts {
            cohort,
            qualified_referrals: qualified,
            retained_referrals: retained,
        }
    }

    #[test]
    fn k_is_retained_referrals_per_cohort_fan() {
        let r = read(counts(20, 12, 10));
        assert_eq!(r.k_milli, Some(500));
        assert_eq!(r.withheld, None);
        assert_eq!(self_sustaining(&r), Some(false));
        assert_eq!(read(counts(10, 30, 15)).k_milli, Some(1_500));
        assert_eq!(self_sustaining(&read(counts(10, 30, 15))), Some(true));
    }

    #[test]
    fn k_of_exactly_one_is_not_yet_self_sustaining() {
        let r = read(counts(10, 12, 10));
        assert_eq!(r.k_milli, Some(1_000));
        assert_eq!(
            self_sustaining(&r),
            Some(false),
            "1.0 replaces, it does not grow"
        );
    }

    #[test]
    fn below_the_cohort_floor_k_is_withheld_not_small() {
        for cohort in 0..MIN_COHORT {
            let r = read(counts(cohort, 2, 1));
            assert_eq!(r.k_milli, None, "cohort {cohort}");
            assert_eq!(r.withheld, Some(Withheld::CohortBelowFloor));
            assert_eq!(self_sustaining(&r), None, "an unknown is not a no");
        }
        assert!(read(counts(MIN_COHORT, 0, 0)).k_milli.is_some());
    }

    #[test]
    fn a_real_zero_is_reported_as_zero_once_the_cohort_is_big_enough() {
        // Nobody in a cohort of ten referred anybody: that IS the measurement.
        let r = read(counts(10, 0, 0));
        assert_eq!(r.k_milli, Some(0));
        assert_eq!(self_sustaining(&r), Some(false));
    }

    #[test]
    fn contradictory_counts_fail_closed() {
        let r = read(counts(50, 3, 7));
        assert_eq!(r.k_milli, None);
        assert_eq!(r.withheld, Some(Withheld::ContradictoryCounts));
        // The contradiction outranks the floor: a bad count is bad at any size.
        assert_eq!(
            read(counts(1, 0, 2)).withheld,
            Some(Withheld::ContradictoryCounts)
        );
    }
}
