//! What a room's terms look like when nobody's answer is identifiable
//! (§4h-9, Sprint 4.8).
//!
//! `typical_draw` averages fairly because a draw is our own observation of a
//! ticket sale. A fee is different: "Klub X paid this band €400" publishes a
//! counterparty's negotiating position and identifies who told us. So fee
//! evidence on a venue bands across a **minimum contributor count** — below
//! [`MIN_CONTRIBUTORS`] distinct workspaces the honest answer is
//! [`VenueTermsEvidence::InsufficientEvidence`], not a thin number.
//!
//! What the aggregate may never do:
//!
//! - return a band fewer than three distinct workspaces stand behind;
//! - name or single out a contributor — the workspace key exists only to be
//!   counted and never leaves the function's output;
//! - merge currencies — a fee means nothing through a conversion the tenant
//!   never agreed to, so each currency bands on its own.
//!
//! The source is `place_event_contributions` rows of kind `terms` and only
//! those. The tenant's own negotiation archive
//! (`viryaos_team_opportunity_terms`) settles against an opportunity id that
//! carries no venue key, so no venue-attributable fee row lands anywhere
//! else — contributions are the whole honest scope, not a partial one.
//!
//! The contributor's own rows are shown to that contributor in full on their
//! own surfaces (the night's `own_terms`); the k-anonymity floor binds only
//! this cross-tenant aggregate. And declining to contribute costs no read
//! access: the band is one answer about the room, the same for every reader.

use std::collections::{BTreeMap, BTreeSet};

use time::OffsetDateTime;
use uuid::Uuid;

/// The k-anonymity floor: a venue's fee evidence needs at least this many
/// distinct contributing workspaces before it may be shown. With two, each
/// side could recognise the other's number by elimination — three is where
/// a band stops being attributable.
pub const MIN_CONTRIBUTORS: usize = 3;

/// One terms contribution as the read hands it over: the contributing
/// workspace (counted toward the floor, never surfaced), the fee in minor
/// units, its ISO-4217 currency, and when the row was contributed.
pub type TermsContribution = (Uuid, i64, String, OffsetDateTime);

/// What the evidence supports saying about one venue's fees in one
/// currency.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VenueTermsEvidence {
    /// The cleared band: quartiles over every contributed fee observation
    /// in the currency, pooled across at least [`MIN_CONTRIBUTORS`]
    /// distinct workspaces.
    Band {
        currency: String,
        fee_p25_minor: i64,
        fee_median_minor: i64,
        fee_p75_minor: i64,
        /// The count the floor cleared — never which workspaces.
        contributor_count: usize,
        /// The instant this aggregate speaks as of — the read's own `now`.
        as_of: OffsetDateTime,
    },
    /// Contributions exist but too few distinct workspaces stand behind
    /// them — the honest refusal, not a quartile of two rows.
    InsufficientEvidence,
    /// No terms contribution at all: the venue has no fee evidence yet.
    NotContributed,
}

/// Aggregates one venue's contributed terms into per-currency bands.
///
/// `contributions` is every active `terms` contribution on the venue's
/// nights — all workspaces, all dates, mixed currencies. Rows pool per
/// currency: the quartiles run over every fee observation, while the
/// contributor count — and the [`MIN_CONTRIBUTORS`] floor — runs over
/// distinct workspaces, so one tenant reporting many nights can never
/// clear the floor alone.
///
/// The result is one entry per currency present, ordered by currency code:
/// [`VenueTermsEvidence::Band`] where the currency clears the floor,
/// [`VenueTermsEvidence::InsufficientEvidence`] where it does not. An
/// empty input returns one [`VenueTermsEvidence::NotContributed`]. A row
/// dated after `now` is a clock error, not evidence, and is excluded; so
/// is a negative amount.
///
/// This function deliberately takes no requesting workspace: the answer
/// is about the venue, and a reader who never contributed gets the same
/// band as one who did. The contributor's own value is never identifiable
/// in the output — quartiles exist only over at least three workspaces.
#[must_use]
pub fn aggregate_venue_terms(
    contributions: &[TermsContribution],
    now: OffsetDateTime,
) -> Vec<VenueTermsEvidence> {
    let mut by_currency: BTreeMap<&str, (BTreeSet<Uuid>, Vec<i64>)> = BTreeMap::new();
    for (workspace_id, amount_minor, currency, contributed_at) in contributions {
        if *contributed_at > now || *amount_minor < 0 {
            continue;
        }
        let (workspaces, amounts) = by_currency.entry(currency.as_str()).or_default();
        workspaces.insert(*workspace_id);
        amounts.push(*amount_minor);
    }
    if by_currency.is_empty() {
        return vec![VenueTermsEvidence::NotContributed];
    }
    by_currency
        .into_iter()
        .map(|(currency, (workspaces, mut amounts))| {
            if workspaces.len() < MIN_CONTRIBUTORS {
                return VenueTermsEvidence::InsufficientEvidence;
            }
            let (fee_p25_minor, fee_median_minor, fee_p75_minor) = quartiles(&mut amounts);
            VenueTermsEvidence::Band {
                currency: currency.to_owned(),
                fee_p25_minor,
                fee_median_minor,
                fee_p75_minor,
                contributor_count: workspaces.len(),
                as_of: now,
            }
        })
        .collect()
}

/// Quartiles as Tukey-style hinges over the pooled observations: sort, take
/// the median, and let p25 and p75 be the medians of the halves below and
/// above it — for an odd pool the median element sits out of both halves.
/// With exactly three observations the band is the three sorted values,
/// which is exactly as much as a floor of three permits a reader to know.
fn quartiles(amounts: &mut [i64]) -> (i64, i64, i64) {
    amounts.sort_unstable();
    let sorted: &[i64] = amounts;
    let mid = sorted.len() / 2;
    let lower = sorted.get(..mid).unwrap_or(&[]);
    // An odd pool keeps its middle element out of both halves.
    let upper = sorted
        .get(
            if sorted.len().is_multiple_of(2) {
                mid
            } else {
                mid + 1
            }..,
        )
        .unwrap_or(&[]);
    (median(lower), median(sorted), median(upper))
}

/// The median of an already-sorted slice — the middle element, or the
/// midpoint of the two middles. Fees are whole minor units, so an odd pair
/// lands on a whole unit rather than a fractional fee. Empty is not
/// reachable from `quartiles`; it still returns 0 rather than panicking.
fn median(sorted: &[i64]) -> i64 {
    let mid = sorted.len() / 2;
    if sorted.is_empty() {
        return 0;
    }
    if sorted.len().is_multiple_of(2) {
        match (sorted.get(mid - 1), sorted.get(mid)) {
            (Some(a), Some(b)) => a.midpoint(*b),
            _ => 0,
        }
    } else {
        sorted.get(mid).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_500)
    }

    fn row(workspace: Uuid, amount_minor: i64, currency: &str) -> TermsContribution {
        (
            workspace,
            amount_minor,
            currency.to_owned(),
            now() - Duration::days(3),
        )
    }

    #[test]
    fn no_contributions_is_not_contributed() {
        assert_eq!(
            aggregate_venue_terms(&[], now()),
            vec![VenueTermsEvidence::NotContributed]
        );
    }

    #[test]
    fn two_workspaces_is_insufficient_not_a_thin_band() {
        let alpha = Uuid::now_v7();
        let bravo = Uuid::now_v7();
        let rows = [row(alpha, 40_000, "PLN"), row(bravo, 60_000, "PLN")];
        assert_eq!(
            aggregate_venue_terms(&rows, now()),
            vec![VenueTermsEvidence::InsufficientEvidence]
        );
    }

    #[test]
    fn one_workspace_on_many_nights_is_still_one_contributor() {
        let alpha = Uuid::now_v7();
        let rows: Vec<TermsContribution> = (0..5)
            .map(|fee| row(alpha, 30_000 + fee * 10_000, "PLN"))
            .collect();
        // Five rows, one workspace — the floor counts workspaces, not rows.
        assert_eq!(
            aggregate_venue_terms(&rows, now()),
            vec![VenueTermsEvidence::InsufficientEvidence]
        );
    }

    #[test]
    fn three_workspaces_clear_the_band() {
        let rows = [
            row(Uuid::now_v7(), 40_000, "PLN"),
            row(Uuid::now_v7(), 60_000, "PLN"),
            row(Uuid::now_v7(), 100_000, "PLN"),
        ];
        let bands = aggregate_venue_terms(&rows, now());
        assert_eq!(
            bands,
            vec![VenueTermsEvidence::Band {
                currency: "PLN".to_owned(),
                fee_p25_minor: 40_000,
                fee_median_minor: 60_000,
                fee_p75_minor: 100_000,
                contributor_count: 3,
                as_of: now(),
            }]
        );
    }

    #[test]
    fn quartiles_pool_every_observation_across_nights() {
        let alpha = Uuid::now_v7();
        let rows = [
            // alpha played the room twice and reported both fees; the pool
            // still has three workspaces behind it.
            row(alpha, 40_000, "PLN"),
            row(alpha, 50_000, "PLN"),
            row(Uuid::now_v7(), 60_000, "PLN"),
            row(Uuid::now_v7(), 100_000, "PLN"),
        ];
        let bands = aggregate_venue_terms(&rows, now());
        // Sorted [40, 50, 60, 100]k — Tukey halves [40,50] and [60,100].
        assert_eq!(
            bands,
            vec![VenueTermsEvidence::Band {
                currency: "PLN".to_owned(),
                fee_p25_minor: 45_000,
                fee_median_minor: 55_000,
                fee_p75_minor: 80_000,
                contributor_count: 3,
                as_of: now(),
            }]
        );
    }

    #[test]
    fn currencies_split_into_one_band_each_and_never_merge() {
        let rows = [
            // Two PLN contributors — below the floor alone.
            row(Uuid::now_v7(), 40_000, "PLN"),
            row(Uuid::now_v7(), 60_000, "PLN"),
            // Three EUR contributors — a band in its own currency.
            row(Uuid::now_v7(), 20_000, "EUR"),
            row(Uuid::now_v7(), 30_000, "EUR"),
            row(Uuid::now_v7(), 50_000, "EUR"),
        ];
        let bands = aggregate_venue_terms(&rows, now());
        // Four workspaces total would clear the floor if currencies merged;
        // the split is what keeps a PLN fee out of a EUR band.
        assert_eq!(
            bands,
            vec![
                VenueTermsEvidence::Band {
                    currency: "EUR".to_owned(),
                    fee_p25_minor: 20_000,
                    fee_median_minor: 30_000,
                    fee_p75_minor: 50_000,
                    contributor_count: 3,
                    as_of: now(),
                },
                VenueTermsEvidence::InsufficientEvidence,
            ]
        );
    }

    #[test]
    fn a_future_dated_row_is_not_evidence() {
        let rows = [
            row(Uuid::now_v7(), 40_000, "PLN"),
            row(Uuid::now_v7(), 60_000, "PLN"),
            (
                Uuid::now_v7(),
                100_000,
                "PLN".to_owned(),
                now() + Duration::days(1),
            ),
        ];
        assert_eq!(
            aggregate_venue_terms(&rows, now()),
            vec![VenueTermsEvidence::InsufficientEvidence]
        );
    }

    #[test]
    fn the_band_speaks_of_no_workspace() {
        // The contributor that paid least cannot be named: with three
        // workspaces behind the pool the extreme values are quartile edges,
        // not attributions — nothing on the band carries a workspace key.
        let lowest = Uuid::now_v7();
        let rows = [
            row(lowest, 10_000, "PLN"),
            row(Uuid::now_v7(), 60_000, "PLN"),
            row(Uuid::now_v7(), 90_000, "PLN"),
        ];
        let bands = aggregate_venue_terms(&rows, now());
        let [
            VenueTermsEvidence::Band {
                contributor_count, ..
            },
        ] = bands.as_slice()
        else {
            panic!("expected one band");
        };
        assert_eq!(*contributor_count, 3);
    }
}
