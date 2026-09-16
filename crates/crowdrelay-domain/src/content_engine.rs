//! The content engine's units — sprint 3.5a.
//!
//! Peers, peer observations, the format catalogue, production events, capture
//! plans, arcs, suggestions and outcomes: the vocabulary the suggestion engine
//! (3.5b) works in. These types mirror migration `0280` one column to a field;
//! the only policy here is the handful of invariants a row cannot express as
//! a `CHECK`:
//!
//! - a format's *marginal* effort is what a band reads when a production day
//!   already covers it (§4b-3) — `ContentFormatEntry::effort_for`;
//! - a suggestion with an empty distribution promise is not worth raising —
//!   `distribution_promise_is_empty`;
//! - status graphs (peer confirmation, arc approval, capture-plan issuing)
//!   only move forward — `*_can_transition`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Date, OffsetDateTime};

use crate::{
    ArcId, CapturePlanId, ContentSuggestionId, EventId, PeerId, ProductionEventId, WorkspaceId,
    WorkspaceMemberId, team_operations::TeamSkill,
};

macro_rules! str_enum {
    ($(#[$meta:meta])* pub enum $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// Derived from `as_str` over `ALL`, so a variant becomes
            /// parseable without a second list to remember.
            #[must_use]
            pub fn parse(value: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|item| item.as_str() == value)
            }
        }
    };
}

str_enum! {
    /// Who the peer is relative to the band — which questions watching them
    /// answers. Aspirational peers show where to aim; near-peers show what is
    /// working at the same level *right now*; lateral peers import moves a
    /// genre has not tried yet.
    pub enum PeerTier {
        Aspirational => "aspirational",
        NearPeer => "near_peer",
        Lateral => "lateral",
    }
}

str_enum! {
    /// The system may propose a peer; the operator confirms. A candidate is
    /// not a peer until then — sweeps only observe `Confirmed` rows.
    pub enum PeerStatus {
        Proposed => "proposed",
        Confirmed => "confirmed",
        Rejected => "rejected",
    }
}

impl PeerStatus {
    /// `Proposed` resolves to exactly one of the terminals; terminals are
    /// final. A rejected peer is never resurrected — a new row with a new
    /// reason is the honest way back.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Proposed, Self::Confirmed) | (Self::Proposed, Self::Rejected)
        )
    }
}

str_enum! {
    /// Effort as a band reads it: low is under an hour of one person's time,
    /// medium is an hour to a day, high is a day or more — or money.
    pub enum Effort {
        Low => "low",
        Medium => "medium",
        High => "high",
    }
}

str_enum! {
    pub enum FormatCategory {
        Release => "release",
        Collaboration => "collaboration",
        Live => "live",
        Evergreen => "evergreen",
        Credibility => "credibility",
    }
}

str_enum! {
    /// What the format is *for* in north-star terms. Credibility is its own
    /// purpose — press and playlist surfaces convert indirectly and counting
    /// them as acquisition would inflate what they deliver.
    pub enum FormatPurpose {
        Acquisition => "acquisition",
        Retention => "retention",
        Conversion => "conversion",
        Credibility => "credibility",
    }
}

str_enum! {
    /// What a format needs to exist. `Nothing` is what makes daily cadence
    /// possible without inventing a premise.
    pub enum FormatRequirement {
        Release => "release",
        Show => "show",
        Nothing => "nothing",
    }
}

str_enum! {
    pub enum FormatCadence {
        OneOff => "one_off",
        Recurring => "recurring",
        ReleaseTied => "release_tied",
    }
}

str_enum! {
    pub enum ProductionEventKind {
        Shoot => "shoot",
        Studio => "studio",
        Rehearsal => "rehearsal",
        Show => "show",
        Drive => "drive",
        Photoshoot => "photoshoot",
        Festival => "festival",
        Other => "other",
    }
}

str_enum! {
    pub enum ProductionEventStatus {
        Scheduled => "scheduled",
        InProgress => "in_progress",
        Done => "done",
        Cancelled => "cancelled",
    }
}

impl ProductionEventStatus {
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Scheduled, Self::InProgress)
                | (Self::Scheduled, Self::Done)
                | (Self::Scheduled, Self::Cancelled)
                | (Self::InProgress, Self::Done)
                | (Self::InProgress, Self::Cancelled)
        )
    }
}

str_enum! {
    /// A capture plan is issued *before* the production day — `Issued` means
    /// it reached the responsible member. `Abandoned` covers the day that
    /// never happened.
    pub enum CapturePlanStatus {
        Draft => "draft",
        Issued => "issued",
        Done => "done",
        Abandoned => "abandoned",
    }
}

impl CapturePlanStatus {
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Draft, Self::Issued)
                | (Self::Draft, Self::Abandoned)
                | (Self::Issued, Self::Done)
                | (Self::Issued, Self::Abandoned)
        )
    }
}

str_enum! {
    /// The band approves an arc as a whole; `Active` is the one currently
    /// feeding beats into suggestions.
    pub enum ArcStatus {
        Proposed => "proposed",
        Approved => "approved",
        Active => "active",
        Completed => "completed",
        Retired => "retired",
    }
}

impl ArcStatus {
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Proposed, Self::Approved)
                | (Self::Proposed, Self::Retired)
                | (Self::Approved, Self::Active)
                | (Self::Approved, Self::Retired)
                | (Self::Active, Self::Completed)
                | (Self::Active, Self::Retired)
        )
    }
}

str_enum! {
    pub enum SuggestionStatus {
        Raised => "raised",
        Approved => "approved",
        Declined => "declined",
        Expired => "expired",
        Done => "done",
    }
}

impl SuggestionStatus {
    /// Whether the suggestion can still reach a person. `Declined`,
    /// `Expired` and `Done` are terminal — a dead suggestion stays dead so
    /// its outcome row remains the last word.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Raised | Self::Approved)
    }
}

str_enum! {
    /// Every suggestion resolves to exactly one outcome — including
    /// `DoneDifferently`, the honest row for "the band did something inspired
    /// by this" that still teaches the engine what landed.
    pub enum SuggestionOutcomeKind {
        Done => "done",
        Declined => "declined",
        DoneDifferently => "done_differently",
        Expired => "expired",
    }
}

/// One seeded, stable content format — a prior, not a rule. The learning
/// loop measures which entries work for this band and retires the rest;
/// nothing here is a hard filter, `genre_fit` least of all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentFormatEntry {
    pub key: String,
    pub name: String,
    pub category: FormatCategory,
    pub purpose: FormatPurpose,
    /// What the format costs on its own day.
    pub effort_standalone: Effort,
    /// What it costs while a production event already covers it — the number
    /// a suggestion actually shows the band.
    pub effort_marginal: Effort,
    /// The roster skill the beat routes to.
    pub skill: TeamSkill,
    pub requires: FormatRequirement,
    /// Which artifacts and channels the format feeds — the surface a
    /// suggestion's distribution promise is assembled from.
    pub distribution: String,
    pub cadence: FormatCadence,
    /// Where the format lands hardest. A bias, never a filter.
    pub genre_fit: Vec<String>,
    pub notes: String,
    pub active: bool,
}

impl ContentFormatEntry {
    /// Effort is marginal, not absolute: a making-of is a day on its own and
    /// near-zero while the shoot is already happening.
    #[must_use]
    pub const fn effort_for(&self, covered_by_production_event: bool) -> Effort {
        if covered_by_production_event {
            self.effort_marginal
        } else {
            self.effort_standalone
        }
    }
}

/// A named artist the band watches. Operator-curated — `proposed_by` records
/// whether a human typed the name or a scanner proposed it, and only a
/// `Confirmed` peer is ever observed.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: PeerId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// `{"spotify": "...", "youtube": "..."}` — platform name to handle.
    pub handles: Value,
    pub tier: PeerTier,
    /// Which dimensions are worth observing — trend dimensions plus free
    /// entries like `tour_routing`.
    pub watch_for: Vec<String>,
    /// Why they are on the list, in the operator's words.
    pub why: String,
    pub proposed_by: String,
    pub status: PeerStatus,
    pub rejection_reason: Option<String>,
    pub confirmed_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One dated fact about a peer, with the link that proves it. Never a
/// summary, never a vibe — a trend over these is only honest if each row is.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerObservation {
    pub id: i64,
    pub workspace_id: WorkspaceId,
    pub peer_id: PeerId,
    /// The date the fact happened — not when the sweep noticed it.
    pub observed_at: Date,
    pub platform: String,
    pub kind: String,
    /// One sentence: "posted a playthrough, 1.2M views in 9 days".
    pub fact: String,
    pub url: Option<String>,
    /// Views/likes as the source reported them.
    pub metrics: Value,
}

/// One dated fact about what an admitted community's fans engaged with —
/// the demand-side twin of `PeerObservation` (what peers publish). The
/// trend detector reads both; `place_id` is a `discovery_places` row, kept
/// untyped like the community-intelligence code that owns those places.
#[derive(Clone, Debug, PartialEq)]
pub struct FanObservation {
    pub id: i64,
    pub workspace_id: WorkspaceId,
    pub place_id: uuid::Uuid,
    /// The post's own date when the source reports it, else the sweep day.
    pub observed_at: Date,
    pub platform: String,
    pub kind: String,
    /// The thing people engaged with: the post title, one line.
    pub fact: String,
    pub url: Option<String>,
    /// Score/comments as the source reported them.
    pub metrics: Value,
}

/// A day the band generates material. When it is a show, `event_id` is the
/// gig — the T-21 ladder already knows the date and venue.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductionEvent {
    pub id: ProductionEventId,
    pub workspace_id: WorkspaceId,
    pub kind: ProductionEventKind,
    pub title: String,
    pub scheduled_for: Date,
    pub event_id: Option<EventId>,
    pub status: ProductionEventStatus,
    pub notes: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The harvest rule: a production day yields more than one output only if
/// someone planned to capture it. Issued *before* the day to the member who
/// holds the camera.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturePlan {
    pub id: CapturePlanId,
    pub workspace_id: WorkspaceId,
    pub production_event_id: ProductionEventId,
    /// Ordered shot list — `[{"item": "...", "skill": "video"}, ...]`.
    pub items: Value,
    /// Who holds the camera — routed through `select_team_assignee`; soft
    /// reference, same convention as team assignments.
    pub assignee_member_id: Option<WorkspaceMemberId>,
    pub status: CapturePlanStatus,
    pub issued_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A campaign with a horizon and a spine of planned beats — the unit the
/// band approves. `evidence` stores the observations and trends behind the
/// shape as references, because the band approves the arc *because of* it.
#[derive(Clone, Debug, PartialEq)]
pub struct Arc {
    pub id: ArcId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub summary: String,
    pub horizon_start: Option<Date>,
    pub horizon_end: Option<Date>,
    /// `[{"at": "2026-10-02", "beat": "playthrough", "format_key": "..."}]` —
    /// dates are intentions, not commitments.
    pub spine: Value,
    pub evidence: Value,
    pub status: ArcStatus,
    pub approved_at: Option<OffsetDateTime>,
    pub approved_by: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Concept + reason + window + the distribution promise. The promise is the
/// part that makes the suggestion worth raising: it names the communities,
/// press contacts and consented fans the piece would actually reach, counted
/// from live workspace data.
#[derive(Clone, Debug, PartialEq)]
pub struct ContentSuggestion {
    pub id: ContentSuggestionId,
    pub workspace_id: WorkspaceId,
    /// The arc this beat serves; `None` for suggestions the calendar or the
    /// peers produced on their own.
    pub arc_id: Option<ArcId>,
    /// The catalogue entry the concept maps to; `None` for bespoke concepts.
    pub format_key: Option<String>,
    pub concept: String,
    pub reason: String,
    pub evidence: Value,
    pub suggested_after: Option<Date>,
    pub suggested_before: Option<Date>,
    /// The marginal estimate once covering production events are counted.
    pub effort: Option<Effort>,
    pub proposed_assignee_member_id: Option<WorkspaceMemberId>,
    /// The assembled promise, clause by clause.
    pub distribution_promise: Value,
    pub status: SuggestionStatus,
    pub expires_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// What the band did with a suggestion — and what happened after. Declines
/// carry their reason verbatim so the concept is suppressed for this tenant;
/// results carry the measured reach so the learning loop has labels.
#[derive(Clone, Debug, PartialEq)]
pub struct SuggestionOutcome {
    pub id: i64,
    pub workspace_id: WorkspaceId,
    pub suggestion_id: ContentSuggestionId,
    pub outcome: SuggestionOutcomeKind,
    pub resolved_at: OffsetDateTime,
    pub decided_by: Option<String>,
    pub reason: Option<String>,
    pub results: Value,
}

/// A distribution promise is a JSON object of clauses — `{"communities":
/// [...], "press_contacts": 12, "consented_fans": 340}`. An empty promise is
/// a suggestion not worth making: the engine declines rather than raise one.
/// Only `{}` and `null` count as empty — a promise with a single real clause
/// is still a promise.
#[must_use]
pub fn distribution_promise_is_empty(promise: &Value) -> bool {
    match promise {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// The persisted `skill` text must always be a `TeamSkill` — the catalogue
/// `CHECK` enforces it in SQL and this is the domain's own copy for rows
/// decoded before they reach the table.
#[must_use]
pub fn parse_format_skill(value: &str) -> Option<TeamSkill> {
    TeamSkill::ALL
        .iter()
        .copied()
        .find(|skill| skill.as_str() == value)
}

/// The trend dimensions an observation can feed and a `watch_for` can name.
/// Not exhaustive of `watch_for` — free entries like `tour_routing` are
/// allowed — but these five are the dimensions the trend detector (3.5b)
/// aggregates over.
pub const TREND_DIMENSIONS: &[&str] = &["format", "theme", "styling", "timing", "platform"];

/// `watch_for` and `genre_fit` are `text[]` columns; the row readers hand
/// them back as `Vec<String>` already lowercased so comparisons never depend
/// on how the operator typed them.
#[must_use]
pub fn normalize_watch_for(values: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = values
        .iter()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn every_str_enum_round_trips_through_parse() {
        for tier in PeerTier::ALL {
            assert_eq!(PeerTier::parse(tier.as_str()), Some(*tier));
        }
        for status in PeerStatus::ALL {
            assert_eq!(PeerStatus::parse(status.as_str()), Some(*status));
        }
        for kind in ProductionEventKind::ALL {
            assert_eq!(ProductionEventKind::parse(kind.as_str()), Some(*kind));
        }
        for outcome in SuggestionOutcomeKind::ALL {
            assert_eq!(
                SuggestionOutcomeKind::parse(outcome.as_str()),
                Some(*outcome)
            );
        }
        // `serde(rename_all = "snake_case")` and the CHECK literals must agree
        // — NearPeer is the variant where they could silently drift.
        assert_eq!(PeerTier::NearPeer.as_str(), "near_peer");
    }

    #[test]
    fn marginal_effort_is_what_a_covered_day_costs() {
        let entry = ContentFormatEntry {
            key: "making_of".to_owned(),
            name: "Making-of".to_owned(),
            category: FormatCategory::Release,
            purpose: FormatPurpose::Retention,
            effort_standalone: Effort::Medium,
            effort_marginal: Effort::Low,
            skill: TeamSkill::Video,
            requires: FormatRequirement::Release,
            distribution: "video artifact → YouTube".to_owned(),
            cadence: FormatCadence::ReleaseTied,
            genre_fit: vec![],
            notes: String::new(),
            active: true,
        };
        assert_eq!(entry.effort_for(false), Effort::Medium);
        assert_eq!(
            entry.effort_for(true),
            Effort::Low,
            "a shoot already happening makes the making-of near-free"
        );
    }

    #[test]
    fn an_empty_promise_is_a_decline() {
        assert!(distribution_promise_is_empty(&Value::Null));
        assert!(distribution_promise_is_empty(&json!({})));
        assert!(!distribution_promise_is_empty(
            &json!({"consented_fans": 12})
        ));
    }

    #[test]
    fn peer_confirmation_is_one_way() {
        assert!(PeerStatus::Proposed.can_transition_to(PeerStatus::Confirmed));
        assert!(PeerStatus::Proposed.can_transition_to(PeerStatus::Rejected));
        assert!(!PeerStatus::Confirmed.can_transition_to(PeerStatus::Proposed));
        assert!(!PeerStatus::Rejected.can_transition_to(PeerStatus::Confirmed));
    }

    #[test]
    fn dead_suggestions_stay_dead() {
        assert!(SuggestionStatus::Raised.is_open());
        assert!(SuggestionStatus::Approved.is_open());
        assert!(!SuggestionStatus::Declined.is_open());
        assert!(!SuggestionStatus::Expired.is_open());
        assert!(!SuggestionStatus::Done.is_open());
    }

    #[test]
    fn catalogue_skills_parse_to_team_skills() {
        assert_eq!(parse_format_skill("video"), Some(TeamSkill::Video));
        assert_eq!(
            parse_format_skill("english_copy"),
            Some(TeamSkill::EnglishCopy)
        );
        assert_eq!(parse_format_skill("not_a_skill"), None);
    }

    #[test]
    fn watch_for_normalizes_and_dedupes() {
        let normalized = normalize_watch_for(&[
            " Format ".to_owned(),
            "theme".to_owned(),
            "FORMAT".to_owned(),
            String::new(),
        ]);
        assert_eq!(normalized, vec!["format".to_owned(), "theme".to_owned()]);
    }

    #[test]
    fn id_newtypes_stay_uuid_v7() {
        assert_eq!(PeerId::new().as_uuid().get_version_num(), 7);
        assert_eq!(ArcId::new().as_uuid().get_version_num(), 7);
        assert_eq!(ContentSuggestionId::new().as_uuid().get_version_num(), 7);
        let _ = Uuid::from(CapturePlanId::new());
    }
}
