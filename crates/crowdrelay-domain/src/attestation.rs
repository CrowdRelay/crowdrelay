//! A number about an audience that somebody with money can act on.
//!
//! # The gap this closes
//!
//! Everything else in this system either helps a band act, or reports to the
//! band about itself. Both stop at the workspace boundary. The moment a label,
//! an agent, a promoter or an investor asks *how big is this act, really*, the
//! honest answer the product can give today is a screenshot, and a screenshot
//! is worth nothing to somebody signing a contract.
//!
//! `crate::listing` is the closest thing and it is deliberately not this: a
//! listing carries claims **the band typed**, with a basis in the band's own
//! words. That is an advertisement, and an advertisement is the right shape for
//! a band introducing itself. It is the wrong shape for due diligence, because
//! the reader still has to take the band's word.
//!
//! An attestation is the other half: a figure **the platform measured from its
//! own ledger**, carrying the method that produced it, the window it covers,
//! when it was observed, and a digest over all of that. Nothing here is typed
//! by a tenant. The tenant chooses whether to publish it and to whom; the
//! tenant cannot choose what it says.
//!
//! That distinction is the entire product claim, so it is enforced structurally
//! rather than by convention: there is no constructor on this module that takes
//! a number and a label from a person. A `MeasuredFigure` names a metric from a
//! closed set, and each metric carries its own method sentence, so a figure
//! cannot be relabelled into something it did not measure.
//!
//! # Why a small number is dangerous, and what happens instead
//!
//! "Reachable fans in Namysłów: 3" is not a statistic, it is three people. A
//! reader who knows the town, the genre and the date can often name them, and
//! the tenant publishing the figure did not consent on their behalf — the fans
//! consented to hear from the band, not to be counted in a pitch deck at a
//! granularity that identifies them.
//!
//! So a count below the cohort floor is published as `fewer than N` rather than
//! exactly. This is what statistical agencies do with small cells, and it keeps
//! the figure useful: a promoter learns "not many here yet", which is true and
//! actionable, and learns nothing about individuals. Refusing outright would be
//! safe and would also delete the honest signal, which is how a product ends up
//! only ever showing its good numbers.
//!
//! # Refuse rather than flatter
//!
//! An attestation with nothing banked in it is the press kit this replaces, so
//! the same rule as `listing::review_listing` applies: something above
//! `Vanity` or it does not publish. A measurement older than the freshness
//! bound does not publish either, because attesting a six-month-old number
//! today is a claim about freshness that is false, and freshness is most of
//! what the reader is buying.
//!
//! # What this module does not do
//!
//! It does not sign. The digest here is canonical and deterministic — the same
//! figures always produce the same bytes — which is what makes an attestation
//! checkable at all, but a digest alone only proves the document is internally
//! consistent. Binding it to *this platform* needs a secret, and secrets live
//! in infrastructure. The domain decides what may be attested and how it is
//! canonicalised; `crowdrelay-infra` decides how it is signed and served.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::value_tier::MetricValueTier;

/// Counts below this are published as a band rather than exactly.
///
/// Twenty-five is the smallest cell that has survived being argued about: large
/// enough that a reader who knows the town cannot enumerate it, small enough
/// that a real early-stage act in a real city still gets an exact number most
/// of the time. It is a privacy floor, not a quality floor — a figure under it
/// is still published, just less precisely.
pub const MINIMUM_COHORT: u32 = 25;

/// How old a measurement may be and still be attested, in days.
///
/// The reader is buying freshness as much as size. A number observed in March
/// and attested in September describes a band that no longer exists, and the
/// document would carry no hint of that unless the bound is enforced here.
pub const MAX_MEASUREMENT_AGE_DAYS: i64 = 30;

/// How long an issued attestation stays valid, in days.
///
/// Deliberately short. An attestation is a snapshot, and a snapshot that never
/// expires becomes a claim about the present that nobody re-checked. Re-issuing
/// is cheap — the figures are recomputed from the ledger either way.
pub const VALIDITY_DAYS: i64 = 30;

/// What was measured. A closed set, because the method sentence is the product.
///
/// Each variant names one computation over the platform's own records. A tenant
/// cannot add a metric, and cannot attach a metric's name to a number that came
/// from somewhere else: the figure carries the variant, and the variant carries
/// the method.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestedMetric {
    /// People who signed up, granted marketing consent, and are inside the
    /// radius they set for the city in question.
    ReachableFans,
    /// Fans with any recorded activity in the window.
    ActiveFans,
    /// Paid ticket orders. The first figure here that is money.
    TicketsSold,
    /// Check-ins scanned at the door. Observed attendance, not inferred.
    ObservedAttendance,
    /// People who paid for at least two different shows.
    RepeatAttenders,
    /// Followers across connected platforms, deduplicated.
    ConnectedFollowers,
}

impl AttestedMetric {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReachableFans => "reachable_fans",
            Self::ActiveFans => "active_fans",
            Self::TicketsSold => "tickets_sold",
            Self::ObservedAttendance => "observed_attendance",
            Self::RepeatAttenders => "repeat_attenders",
            Self::ConnectedFollowers => "connected_followers",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reachable_fans" => Some(Self::ReachableFans),
            "active_fans" => Some(Self::ActiveFans),
            "tickets_sold" => Some(Self::TicketsSold),
            "observed_attendance" => Some(Self::ObservedAttendance),
            "repeat_attenders" => Some(Self::RepeatAttenders),
            "connected_followers" => Some(Self::ConnectedFollowers),
            _ => None,
        }
    }

    /// Where the metric sits relative to something the business banks.
    ///
    /// The same ladder the rest of the system ranks by, applied here so an
    /// attestation of nothing but follower counts is refused rather than issued.
    #[must_use]
    pub const fn tier(self) -> MetricValueTier {
        match self {
            Self::ConnectedFollowers => MetricValueTier::Vanity,
            Self::ReachableFans | Self::ActiveFans => MetricValueTier::Intermediate,
            Self::TicketsSold | Self::ObservedAttendance | Self::RepeatAttenders => {
                MetricValueTier::Downstream
            }
        }
    }

    /// How the number was produced, in a sentence the reader can argue with.
    ///
    /// This is the part a label actually reads. A number with no method is a
    /// number they have to trust; a number with a method is one they can
    /// challenge, and a figure that survives a challenge is worth more than one
    /// that was never examined.
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::ReachableFans => {
                "People who signed up to this act's own fanbase, granted marketing consent \
                 that is still current, and set a location inside the radius shown. Counted \
                 from the consent ledger, not from a platform's follower number."
            }
            Self::ActiveFans => {
                "Fans of this act with at least one recorded interaction inside the window \
                 — an open, a click, a check-in or an order. Signups with no activity are \
                 not counted."
            }
            Self::TicketsSold => {
                "Tickets sold across paid orders, counted from the order ledger — one \
                 order can carry several tickets. Fully refunded orders are excluded; \
                 partially refunded orders still count."
            }
            Self::ObservedAttendance => {
                "Distinct people scanned at the door — somebody scanned twice is one \
                 attender. Observed attendance rather than an estimate from ticket sales, \
                 and normally lower than tickets sold because not everyone who buys \
                 turns up."
            }
            Self::RepeatAttenders => {
                "People who paid for at least two different shows by this act, matched on \
                 the buyer's email. The clearest evidence in this document that an audience \
                 came back rather than turned up once."
            }
            Self::ConnectedFollowers => {
                "Followers across the platforms this act has connected, deduplicated where \
                 the same person is identifiable across two. A reach number, not an \
                 audience: nobody here has agreed to hear from the act."
            }
        }
    }
}

/// What the figure covers.
///
/// City scope is what a promoter or an agent actually asks about — "can you
/// draw in Wrocław" — and it is also where the cohort floor matters most,
/// because a city slices the audience small enough to identify people.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum FigureScope {
    /// The whole audience, wherever they are.
    Everywhere,
    /// One city, named as the tenant's own city catalogue names it.
    City(String),
    /// One show.
    Event(String),
}

impl FigureScope {
    /// The stable key used in the digest. Deterministic across runs, which is
    /// what makes the digest recomputable by a reader.
    fn key(&self) -> String {
        match self {
            Self::Everywhere => "everywhere".to_owned(),
            Self::City(name) => format!("city:{}", name.trim().to_lowercase()),
            Self::Event(slug) => format!("event:{}", slug.trim().to_lowercase()),
        }
    }
}

/// A number the platform computed, before the privacy floor is applied.
///
/// Constructed by infrastructure from a query, never from tenant input. The
/// `observed_at` is when the measurement ran, not when the attestation was
/// issued — the gap between the two is exactly what the freshness bound checks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MeasuredFigure {
    pub metric: AttestedMetric,
    pub scope: FigureScope,
    pub value: u32,
    /// The lookback the measurement used. `0` means a lifetime figure, which is
    /// correct for repeat attenders and wrong for active fans.
    pub window_days: u16,
    #[serde(with = "crate::wire_time")]
    pub observed_at: OffsetDateTime,
}

/// What a reader is shown: exact, or a bound when the cohort is too small to
/// name precisely.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum PublishedValue {
    Exact(u32),
    /// The true value is below this bound. Never the true value itself.
    FewerThan(u32),
}

impl PublishedValue {
    /// The stable key used in the digest.
    fn key(self) -> String {
        match self {
            Self::Exact(value) => format!("exact:{value}"),
            Self::FewerThan(bound) => format!("fewer_than:{bound}"),
        }
    }

    /// How it reads to a person. Kept here rather than in a view layer so the
    /// digest and the rendering cannot drift apart.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Exact(value) => value.to_string(),
            Self::FewerThan(bound) => format!("fewer than {bound}"),
        }
    }
}

/// One line of the attestation as a reader receives it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttestedFigure {
    pub metric: AttestedMetric,
    pub scope: FigureScope,
    pub value: PublishedValue,
    pub window_days: u16,
    #[serde(with = "crate::wire_time")]
    pub observed_at: OffsetDateTime,
}

impl AttestedFigure {
    /// Applies the privacy floor.
    ///
    /// Only counts scoped narrowly enough to identify people are banded. An
    /// audience-wide figure of 12 is a small band, not a small group of
    /// identifiable individuals, and banding it would hide the one number an
    /// early act most needs to show honestly.
    fn from_measured(figure: &MeasuredFigure) -> Self {
        let value = match figure.scope {
            FigureScope::Everywhere => PublishedValue::Exact(figure.value),
            FigureScope::City(_) | FigureScope::Event(_) if figure.value < MINIMUM_COHORT => {
                PublishedValue::FewerThan(MINIMUM_COHORT)
            }
            _ => PublishedValue::Exact(figure.value),
        };
        Self {
            metric: figure.metric,
            scope: figure.scope.clone(),
            value,
            window_days: figure.window_days,
            observed_at: figure.observed_at,
        }
    }

    fn digest_line(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.metric.as_str(),
            self.scope.key(),
            self.value.key(),
            self.window_days,
            self.observed_at.unix_timestamp()
        )
    }
}

/// An issued attestation: figures, when it was issued, when it stops being
/// current, and the digest over all of it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attestation {
    pub act_name: String,
    /// Sorted deterministically at issue time, so the digest does not depend on
    /// the order a query happened to return rows in.
    pub figures: Vec<AttestedFigure>,
    #[serde(with = "crate::wire_time")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "crate::wire_time")]
    pub valid_until: OffsetDateTime,
    /// Hex SHA-256 over the canonical form. Recomputable by anyone holding the
    /// document, which is what makes an edited copy detectable.
    pub digest: String,
}

impl Attestation {
    /// True when the reader is looking at it inside its validity window.
    #[must_use]
    pub fn is_current(&self, now: OffsetDateTime) -> bool {
        now >= self.issued_at && now < self.valid_until
    }

    /// Recomputes the digest from the fields as they stand.
    ///
    /// A reader compares this against the `digest` they were given. They match
    /// unless a field was changed after issue, which is the whole point: a band
    /// that edits the number in the PDF breaks the document.
    #[must_use]
    pub fn recompute_digest(&self) -> String {
        canonical_digest(
            &self.act_name,
            &self.figures,
            self.issued_at,
            self.valid_until,
        )
    }

    /// Whether the document is internally consistent.
    ///
    /// Not the same as authentic: this proves nothing was edited, not that we
    /// issued it. Authenticity needs the signature infrastructure holds.
    #[must_use]
    pub fn digest_matches(&self) -> bool {
        self.recompute_digest() == self.digest
    }
}

/// Why an attestation may not be issued.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttestationRefusal {
    MissingActName,
    /// Nothing measured. An empty attestation is worse than none: it implies
    /// the platform looked and found nothing worth stating.
    NothingMeasured,
    /// Every figure is a reach number. The press kit this replaces.
    NothingBanked,
    /// A measurement older than the freshness bound. Naming the metric so the
    /// operator knows which query to re-run.
    StaleMeasurement {
        metric: AttestedMetric,
        age_days: i64,
    },
    /// A measurement timestamped in the future. Fail closed: a clock that is
    /// wrong in one direction is wrong in both, and every other figure in the
    /// document came from the same clock.
    MeasurementFromTheFuture {
        metric: AttestedMetric,
    },
}

impl AttestationRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingActName => "an attestation needs the act's name".to_owned(),
            Self::NothingMeasured => {
                "there is nothing measured to attest — an empty document reads as \
                 'we looked and found nothing'"
                    .to_owned()
            }
            Self::NothingBanked => {
                "every figure here is a reach number; the reader decides on what was \
                 banked, so include tickets, attendance, or fans who came back"
                    .to_owned()
            }
            Self::StaleMeasurement { metric, age_days } => format!(
                "`{}` was measured {age_days} days ago and the bound is \
                 {MAX_MEASUREMENT_AGE_DAYS} — re-run the measurement rather than \
                 attesting a number this old",
                metric.as_str()
            ),
            Self::MeasurementFromTheFuture { metric } => format!(
                "`{}` is timestamped in the future, so the clock that produced every \
                 figure here cannot be trusted; nothing is issued",
                metric.as_str()
            ),
        }
    }
}

/// The canonical byte form, hashed.
///
/// Field order and separators are fixed, figures are already sorted by the
/// caller, and every value is rendered through the same `key` helpers the
/// document displays. A reader re-deriving this from what they can see gets the
/// same answer or the document was edited.
fn canonical_digest(
    act_name: &str,
    figures: &[AttestedFigure],
    issued_at: OffsetDateTime,
    valid_until: OffsetDateTime,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"crowdrelay.attestation.v1\n");
    hasher.update(act_name.trim().to_lowercase().as_bytes());
    hasher.update(b"\n");
    for figure in figures {
        hasher.update(figure.digest_line().as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(issued_at.unix_timestamp().to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(valid_until.unix_timestamp().to_string().as_bytes());
    // Hex-encoded by hand, the same way `publish_guard::content_hash` does it,
    // rather than pulling in a hex crate for sixty-four characters.
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Issues an attestation over measured figures.
///
/// # Errors
///
/// Refuses an unnamed act, an empty set, a set with nothing above `Vanity`, a
/// measurement older than [`MAX_MEASUREMENT_AGE_DAYS`], or any measurement
/// timestamped after `now`.
pub fn issue(
    act_name: &str,
    measured: &[MeasuredFigure],
    now: OffsetDateTime,
) -> Result<Attestation, AttestationRefusal> {
    if act_name.trim().is_empty() {
        return Err(AttestationRefusal::MissingActName);
    }
    if measured.is_empty() {
        return Err(AttestationRefusal::NothingMeasured);
    }

    for figure in measured {
        if figure.observed_at > now {
            return Err(AttestationRefusal::MeasurementFromTheFuture {
                metric: figure.metric,
            });
        }
        let age_days = (now - figure.observed_at).whole_days();
        if age_days > MAX_MEASUREMENT_AGE_DAYS {
            return Err(AttestationRefusal::StaleMeasurement {
                metric: figure.metric,
                age_days,
            });
        }
    }

    if !measured
        .iter()
        .any(|figure| figure.metric.tier() > MetricValueTier::Vanity)
    {
        return Err(AttestationRefusal::NothingBanked);
    }

    let mut figures: Vec<AttestedFigure> =
        measured.iter().map(AttestedFigure::from_measured).collect();
    // Sorted so the digest depends on the content and not on the order rows
    // came back in. Two attestations over the same facts must be byte-identical
    // or "the digest changed" stops meaning "the document changed".
    figures.sort_by(|a, b| {
        a.metric
            .as_str()
            .cmp(b.metric.as_str())
            .then_with(|| a.scope.key().cmp(&b.scope.key()))
            .then_with(|| a.window_days.cmp(&b.window_days))
    });

    let valid_until = now + time::Duration::days(VALIDITY_DAYS);
    let digest = canonical_digest(act_name, &figures, now, valid_until);
    Ok(Attestation {
        act_name: act_name.trim().to_owned(),
        figures,
        issued_at: now,
        valid_until,
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(days_ago: i64) -> OffsetDateTime {
        now() - time::Duration::days(days_ago)
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_789_000_000).expect("fixed instant")
    }

    fn figure(metric: AttestedMetric, scope: FigureScope, value: u32) -> MeasuredFigure {
        MeasuredFigure {
            metric,
            scope,
            value,
            window_days: 30,
            observed_at: at(1),
        }
    }

    fn banked() -> Vec<MeasuredFigure> {
        vec![
            figure(
                AttestedMetric::ReachableFans,
                FigureScope::City("Wrocław".to_owned()),
                300,
            ),
            figure(AttestedMetric::TicketsSold, FigureScope::Everywhere, 120),
        ]
    }

    #[test]
    fn a_measured_set_with_something_banked_issues() {
        let attestation = issue("Virya", &banked(), now()).expect("issues");
        assert_eq!(attestation.act_name, "Virya");
        assert_eq!(attestation.figures.len(), 2);
        assert!(attestation.is_current(now()));
        assert!(attestation.digest_matches());
    }

    /// The press kit this replaces: all reach, nothing banked.
    #[test]
    fn an_attestation_of_only_follower_counts_is_refused() {
        let reach = vec![figure(
            AttestedMetric::ConnectedFollowers,
            FigureScope::Everywhere,
            4_000,
        )];
        assert_eq!(
            issue("Virya", &reach, now()),
            Err(AttestationRefusal::NothingBanked)
        );
    }

    #[test]
    fn an_empty_or_unnamed_attestation_is_refused() {
        assert_eq!(
            issue("Virya", &[], now()),
            Err(AttestationRefusal::NothingMeasured)
        );
        assert_eq!(
            issue("   ", &banked(), now()),
            Err(AttestationRefusal::MissingActName)
        );
    }

    /// Freshness is most of what the reader is buying.
    #[test]
    fn a_measurement_past_the_freshness_bound_is_refused() {
        let mut stale = banked();
        stale[1].observed_at = at(MAX_MEASUREMENT_AGE_DAYS + 1);
        assert_eq!(
            issue("Virya", &stale, now()),
            Err(AttestationRefusal::StaleMeasurement {
                metric: AttestedMetric::TicketsSold,
                age_days: MAX_MEASUREMENT_AGE_DAYS + 1,
            })
        );

        // Exactly at the bound still issues — the refusal is for older than,
        // not for as old as, and an off-by-one here silently narrows the
        // window every tenant is working inside.
        let mut edge = banked();
        edge[1].observed_at = at(MAX_MEASUREMENT_AGE_DAYS);
        assert!(issue("Virya", &edge, now()).is_ok());
    }

    /// A clock wrong in one direction is wrong in both, and every figure in the
    /// document came from the same clock.
    #[test]
    fn a_measurement_from_the_future_fails_closed() {
        let mut ahead = banked();
        ahead[0].observed_at = now() + time::Duration::hours(1);
        assert_eq!(
            issue("Virya", &ahead, now()),
            Err(AttestationRefusal::MeasurementFromTheFuture {
                metric: AttestedMetric::ReachableFans,
            })
        );
    }

    // ── The privacy floor ───────────────────────────────────────────────────

    /// Three reachable fans in a small town is three people, and a reader who
    /// knows the town can often name them.
    #[test]
    fn a_small_city_cohort_is_banded_never_exact() {
        let small = vec![
            figure(
                AttestedMetric::ReachableFans,
                FigureScope::City("Namysłów".to_owned()),
                3,
            ),
            figure(AttestedMetric::TicketsSold, FigureScope::Everywhere, 120),
        ];
        let attestation = issue("Virya", &small, now()).expect("issues");
        let city = attestation
            .figures
            .iter()
            .find(|f| matches!(f.scope, FigureScope::City(_)))
            .expect("city figure present");
        assert_eq!(city.value, PublishedValue::FewerThan(MINIMUM_COHORT));
        assert_eq!(city.value.describe(), "fewer than 25");

        // And the true value must not survive anywhere in the document.
        let serialized = serde_json::to_string(&attestation).expect("serializes");
        assert!(
            !serialized.contains("\"value\":3") && !serialized.contains(":3,"),
            "the suppressed count leaked into the document: {serialized}"
        );
    }

    #[test]
    fn the_floor_is_a_boundary_not_a_vibe() {
        let below = issue(
            "Virya",
            &[
                figure(
                    AttestedMetric::ReachableFans,
                    FigureScope::City("Wrocław".to_owned()),
                    MINIMUM_COHORT - 1,
                ),
                figure(AttestedMetric::TicketsSold, FigureScope::Everywhere, 120),
            ],
            now(),
        )
        .expect("issues");
        assert_eq!(
            below.figures[0].value,
            PublishedValue::FewerThan(MINIMUM_COHORT)
        );

        let at_floor = issue(
            "Virya",
            &[
                figure(
                    AttestedMetric::ReachableFans,
                    FigureScope::City("Wrocław".to_owned()),
                    MINIMUM_COHORT,
                ),
                figure(AttestedMetric::TicketsSold, FigureScope::Everywhere, 120),
            ],
            now(),
        )
        .expect("issues");
        assert_eq!(
            at_floor.figures[0].value,
            PublishedValue::Exact(MINIMUM_COHORT)
        );
    }

    /// An audience-wide 12 is a small band, not a small group of identifiable
    /// people. Banding it would hide the number an early act most needs to be
    /// able to show honestly.
    #[test]
    fn an_audience_wide_figure_is_never_banded() {
        let tiny = vec![
            figure(AttestedMetric::ReachableFans, FigureScope::Everywhere, 12),
            figure(AttestedMetric::TicketsSold, FigureScope::Everywhere, 4),
        ];
        let attestation = issue("Virya", &tiny, now()).expect("issues");
        assert!(
            attestation
                .figures
                .iter()
                .all(|f| matches!(f.value, PublishedValue::Exact(_))),
            "an audience-wide figure was banded"
        );
    }

    // ── The digest ──────────────────────────────────────────────────────────

    /// The whole reason a reader can check the document.
    #[test]
    fn editing_any_field_breaks_the_digest() {
        let issued = issue("Virya", &banked(), now()).expect("issues");

        let mut edited = issued.clone();
        edited.figures[0].value = PublishedValue::Exact(9_000);
        assert!(!edited.digest_matches(), "an edited figure still verified");

        let mut renamed = issued.clone();
        renamed.act_name = "Somebody Else".to_owned();
        assert!(
            !renamed.digest_matches(),
            "an edited act name still verified"
        );

        let mut extended = issued.clone();
        extended.valid_until += time::Duration::days(365);
        assert!(
            !extended.digest_matches(),
            "an extended validity window still verified"
        );
    }

    /// Two issues over the same facts must be byte-identical, or "the digest
    /// changed" stops meaning "the document changed".
    #[test]
    fn the_digest_does_not_depend_on_query_order() {
        let forward = issue("Virya", &banked(), now()).expect("issues");
        let mut reversed = banked();
        reversed.reverse();
        let backward = issue("Virya", &reversed, now()).expect("issues");
        assert_eq!(forward.digest, backward.digest);
        assert_eq!(forward.figures, backward.figures);
    }

    /// A banded figure and an exact one must not collide: if `FewerThan(25)`
    /// hashed the same as `Exact(25)`, suppression would be undetectable.
    #[test]
    fn a_banded_value_and_an_exact_value_hash_differently() {
        assert_ne!(
            PublishedValue::FewerThan(MINIMUM_COHORT).key(),
            PublishedValue::Exact(MINIMUM_COHORT).key()
        );
    }

    // ── Expiry ──────────────────────────────────────────────────────────────

    #[test]
    fn an_attestation_stops_being_current_when_it_expires() {
        let issued = issue("Virya", &banked(), now()).expect("issues");
        assert!(issued.is_current(now()));
        assert!(issued.is_current(now() + time::Duration::days(VALIDITY_DAYS - 1)));
        assert!(!issued.is_current(now() + time::Duration::days(VALIDITY_DAYS)));
        // And a reader whose clock is behind the issue time is not shown a
        // document that has not been issued yet.
        assert!(!issued.is_current(now() - time::Duration::seconds(1)));
    }

    // ── The method is the product ───────────────────────────────────────────

    #[test]
    fn every_metric_states_a_method_a_reader_can_argue_with() {
        for metric in [
            AttestedMetric::ReachableFans,
            AttestedMetric::ActiveFans,
            AttestedMetric::TicketsSold,
            AttestedMetric::ObservedAttendance,
            AttestedMetric::RepeatAttenders,
            AttestedMetric::ConnectedFollowers,
        ] {
            let method = metric.method();
            assert!(method.len() > 60, "too thin to argue with: {method}");
            assert_eq!(AttestedMetric::parse(metric.as_str()), Some(metric));
        }
    }

    /// The tier ladder decides what counts as banked, so a mislabelled tier
    /// would let a follower-only document through the `NothingBanked` gate.
    #[test]
    fn follower_counts_are_the_only_vanity_metric() {
        assert_eq!(
            AttestedMetric::ConnectedFollowers.tier(),
            MetricValueTier::Vanity
        );
        for banked_metric in [
            AttestedMetric::TicketsSold,
            AttestedMetric::ObservedAttendance,
            AttestedMetric::RepeatAttenders,
        ] {
            assert_eq!(banked_metric.tier(), MetricValueTier::Downstream);
        }
    }

    #[test]
    fn every_refusal_tells_the_operator_what_to_do() {
        for refusal in [
            AttestationRefusal::MissingActName,
            AttestationRefusal::NothingMeasured,
            AttestationRefusal::NothingBanked,
            AttestationRefusal::StaleMeasurement {
                metric: AttestedMetric::TicketsSold,
                age_days: 90,
            },
            AttestationRefusal::MeasurementFromTheFuture {
                metric: AttestedMetric::ReachableFans,
            },
        ] {
            let message = refusal.message();
            assert!(message.len() > 30, "too terse: {message}");
            assert!(
                !message.contains("Err(") && !message.contains("None"),
                "leaks Rust at the operator: {message}"
            );
        }
    }
}
