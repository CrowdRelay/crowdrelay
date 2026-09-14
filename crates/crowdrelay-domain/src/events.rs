//! Event discovery, interest and conversion domain types.
//!
//! Defines public event views, fan interest registration, and conversion
//! action tracking for the event discovery slice.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use crate::{CampaignId, CityId, EventId, EventSlug, FanId, VisitorId, WorkspaceId};
use url::Url;

/// Publication status of an event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    /// Event is being prepared and not visible to fans.
    Draft,
    /// Event is visible to fans.
    Published,
    /// Event has been cancelled.
    Cancelled,
    /// Event has concluded.
    Completed,
}

/// Kind of conversion action tracked on an event page.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventActionKind {
    /// Fan viewed the event page.
    PageView,
    /// Fan clicked the ticket link.
    TicketClick,
    /// Fan downloaded a calendar entry.
    CalendarDownload,
    /// Fan clicked a listen/streaming link.
    ListenClick,
    /// Fan clicked a share button.
    ShareClick,
}

impl EventActionKind {
    /// Returns the snake-case string representation used in serialized payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PageView => "page_view",
            Self::TicketClick => "ticket_click",
            Self::CalendarDownload => "calendar_download",
            Self::ListenClick => "listen_click",
            Self::ShareClick => "share_click",
        }
    }
}

/// City information embedded in a public event view.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventCity {
    pub id: CityId,
    pub slug: String,
    pub name: String,
    pub country_code: String,
    pub region: Option<String>,
}

/// One act on an event's bill, as the fan-facing page renders it.
///
/// `ticket_url` is the act's own tagged link — when absent the event's
/// shared `ticket_url` is the fallback, because a crossbill night often has
/// one door link for everyone.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublicEventAct {
    pub act_slug: String,
    pub act_name: String,
    pub ticket_url: Option<String>,
}

/// Fan-visible event detail view served from the in-memory cache.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublicEvent {
    pub id: EventId,
    pub slug: EventSlug,
    pub title: String,
    pub description: Option<String>,
    pub city: Option<EventCity>,
    pub venue: Option<String>,
    pub venue_address: Option<String>,
    pub timezone: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub doors_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub ends_at: Option<OffsetDateTime>,
    pub ticket_url: Option<String>,
    pub listen_url: Option<String>,
    pub image_url: Option<String>,
    pub trailer_url: Option<String>,
    pub external_event_url: Option<String>,
    #[serde(default)]
    pub acts: Vec<PublicEventAct>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl PublicEvent {
    /// Validates title, timezone, text fields, URLs, and schedule consistency.
    pub fn validate(&self) -> Result<(), PublicEventError> {
        validate_required_text(&self.title, 300).map_err(|_| PublicEventError::InvalidTitle)?;
        validate_required_text(&self.timezone, 128)
            .map_err(|_| PublicEventError::InvalidTimezone)?;
        validate_optional_multiline_text(self.description.as_deref(), 10_000)?;
        validate_optional_text(self.venue.as_deref(), 500)?;
        validate_optional_text(self.venue_address.as_deref(), 500)?;

        for value in [
            self.ticket_url.as_deref(),
            self.listen_url.as_deref(),
            self.image_url.as_deref(),
            self.trailer_url.as_deref(),
            self.external_event_url.as_deref(),
        ] {
            validate_optional_https_url(value)?;
        }

        if self.doors_at.is_some_and(|doors| doors > self.starts_at)
            || self.ends_at.is_some_and(|ends| ends < self.starts_at)
        {
            return Err(PublicEventError::InvalidSchedule);
        }

        // Acts are the bill: each carries its own tagged ticket link, so a
        // malformed one gets the same scrutiny as the event's own fields.
        if self.acts.len() > 32 {
            return Err(PublicEventError::InvalidActs);
        }
        for act in &self.acts {
            validate_act_fields(&act.act_slug, &act.act_name, act.ticket_url.as_deref())
                .map_err(|_| PublicEventError::InvalidActs)?;
        }
        Ok(())
    }
}

/// Error returned when a public event fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PublicEventError {
    /// The event title was empty, too long, or contained control characters.
    #[error("event title is invalid")]
    InvalidTitle,
    /// The timezone string was invalid.
    #[error("event timezone is invalid")]
    InvalidTimezone,
    /// The schedule had inconsistent doors/ends times relative to start time.
    #[error("event schedule is invalid")]
    InvalidSchedule,
    /// A text field was invalid.
    #[error("event text field is invalid")]
    InvalidText,
    /// A URL field was not a valid HTTPS URL.
    #[error("event URL is invalid")]
    InvalidUrl,
    /// An act on the bill was malformed (slug grammar, empty/oversized name,
    /// or the bill itself exceeded the bound).
    #[error("event act bill is invalid")]
    InvalidActs,
}

/// The `event_acts.act_slug` CHECK grammar: `^[a-z0-9][a-z0-9-]{0,63}$`.
/// One source shared by click attribution, public-event validation and the
/// staff write path so a slug that is legal in one place cannot be illegal in
/// the other.
#[must_use]
pub fn valid_act_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && slug
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Validates one act exactly as [`PublicEvent::validate`] will on the way back
/// out — write paths call this so a bill that stores cannot poison the public
/// event cache on the next refresh.
///
/// The checks mirror the event's own field rules: CHECK-grammar slug,
/// control-free trimmed name within bytes, HTTPS-only ticket URL with no
/// userinfo or fragment.
pub fn validate_act_fields(
    act_slug: &str,
    act_name: &str,
    ticket_url: Option<&str>,
) -> Result<(), PublicEventError> {
    if !valid_act_slug(act_slug) {
        return Err(PublicEventError::InvalidActs);
    }
    validate_required_text(act_name, 160).map_err(|_| PublicEventError::InvalidActs)?;
    validate_optional_https_url(ticket_url)
}

fn validate_required_text(value: &str, maximum_bytes: usize) -> Result<(), PublicEventError> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > maximum_bytes
        || value.chars().any(char::is_control)
    {
        return Err(PublicEventError::InvalidText);
    }
    Ok(())
}

fn validate_optional_text(
    value: Option<&str>,
    maximum_bytes: usize,
) -> Result<(), PublicEventError> {
    match value {
        Some(value) => validate_required_text(value, maximum_bytes),
        None => Ok(()),
    }
}

/// A concert description is prose: promoters write paragraphs, and providers
/// hand them to us with the line breaks intact. Treating `\n` as a control
/// character silently dropped otherwise-valid events out of the public feed.
///
/// Deliberately separate from `validate_optional_text` rather than loosening
/// it, so `venue`, `venue_address` and the rest stay strictly single-line.
fn validate_optional_multiline_text(
    value: Option<&str>,
    maximum_bytes: usize,
) -> Result<(), PublicEventError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim() != value
        || value.is_empty()
        || value.len() > maximum_bytes
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(PublicEventError::InvalidText);
    }
    Ok(())
}

fn validate_optional_https_url(value: Option<&str>) -> Result<(), PublicEventError> {
    let Some(value) = value else {
        return Ok(());
    };
    let url = Url::parse(value).map_err(|_| PublicEventError::InvalidUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(PublicEventError::InvalidUrl);
    }
    Ok(())
}

/// Result returned after registering fan interest in an event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventInterestResult {
    pub event_id: EventId,
    pub fan_id: FanId,
    pub created: bool,
    pub reminder_count: u32,
}

/// A fan's interest in a specific event, with the event detail view.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FanEventInterest {
    pub event: PublicEvent,
    #[serde(with = "time::serde::rfc3339")]
    pub interested_at: OffsetDateTime,
}

/// A conversion action tracked on an event page for analytics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventAction {
    workspace_id: WorkspaceId,
    event_id: EventId,
    action: EventActionKind,
    campaign_id: Option<CampaignId>,
    visitor_id: Option<VisitorId>,
    referrer_host: Option<String>,
    /// Which act's tagged link produced the action — recorded with the fact
    /// so it survives later edits to the bill.
    act_slug: Option<String>,
    occurred_at: OffsetDateTime,
}

impl EventAction {
    /// Creates an event action, normalizing the referrer host to lowercase
    /// and rejecting hosts exceeding 253 bytes or containing control characters.
    pub fn new(
        workspace_id: WorkspaceId,
        event_id: EventId,
        action: EventActionKind,
        campaign_id: Option<CampaignId>,
        visitor_id: Option<VisitorId>,
        referrer_host: Option<String>,
        occurred_at: OffsetDateTime,
    ) -> Result<Self, EventActionError> {
        let referrer_host = match referrer_host {
            Some(value) => {
                let value = value.trim().to_ascii_lowercase();
                if value.is_empty() || value.len() > 253 || value.chars().any(char::is_control) {
                    return Err(EventActionError::InvalidReferrer);
                }
                Some(value)
            }
            None => None,
        };
        Ok(Self {
            workspace_id,
            event_id,
            action,
            campaign_id,
            visitor_id,
            referrer_host,
            act_slug: None,
            occurred_at,
        })
    }

    /// Attributes the action to one act's tagged link. The slug grammar
    /// matches `event_acts.act_slug`; anything else is rejected rather than
    /// silently persisted into the analytics ledger. On failure the action
    /// itself is untouched — a click still counts unattributed.
    pub fn set_act_slug(&mut self, act_slug: &str) -> Result<(), EventActionError> {
        let slug = act_slug.trim().to_ascii_lowercase();
        if !valid_act_slug(&slug) {
            return Err(EventActionError::InvalidActSlug);
        }
        self.act_slug = Some(slug);
        Ok(())
    }

    /// Returns the workspace that owns the event.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }
    /// Returns the event identifier.
    #[must_use]
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }
    /// Returns the kind of conversion action.
    #[must_use]
    pub const fn action(&self) -> EventActionKind {
        self.action
    }
    /// Returns the optional campaign associated with the action.
    #[must_use]
    pub const fn campaign_id(&self) -> Option<CampaignId> {
        self.campaign_id
    }
    /// Returns the optional visitor identifier.
    #[must_use]
    pub const fn visitor_id(&self) -> Option<VisitorId> {
        self.visitor_id
    }
    /// Returns the normalized referrer host, if present.
    #[must_use]
    pub fn referrer_host(&self) -> Option<&str> {
        self.referrer_host.as_deref()
    }
    /// Returns the act whose tagged link produced the action, if any.
    #[must_use]
    pub fn act_slug(&self) -> Option<&str> {
        self.act_slug.as_deref()
    }
    /// Returns the timestamp at which the action occurred.
    #[must_use]
    pub const fn occurred_at(&self) -> OffsetDateTime {
        self.occurred_at
    }
}

/// Error returned when an event action fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum EventActionError {
    /// The referrer host was empty, too long, or contained invalid characters.
    #[error("event action referrer is invalid")]
    InvalidReferrer,
    /// The act slug was empty, too long, or outside the slug grammar.
    #[error("event action act slug is invalid")]
    InvalidActSlug,
}

#[cfg(test)]
mod description_tests {
    use super::*;

    pub(super) fn event(description: Option<&str>, venue: Option<&str>) -> PublicEvent {
        PublicEvent {
            id: EventId::new(),
            slug: EventSlug::parse("wlacz-sie-na-nowe").expect("slug should parse"),
            title: "WŁĄCZ SIĘ NA NOWE".to_owned(),
            description: description.map(ToOwned::to_owned),
            city: None,
            venue: venue.map(ToOwned::to_owned),
            venue_address: None,
            timezone: "Europe/Warsaw".to_owned(),
            starts_at: OffsetDateTime::UNIX_EPOCH,
            doors_at: None,
            ends_at: None,
            ticket_url: None,
            listen_url: None,
            image_url: None,
            trailer_url: None,
            external_event_url: None,
            acts: Vec::new(),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// The shape that actually fell out of the public feed: a real promoter
    /// description with paragraphs, no outer whitespace, rejected only because
    /// every `\n` counted as a control character.
    #[test]
    fn multiline_promoter_description_stays_publishable() {
        let description = concat!(
            "WŁĄCZ SIĘ NA NOWE!\n",
            "Zapraszamy na kolejny koncert cyklu!\n\n",
            "11.09.2026 (piątek) klub Łącznik\n",
            "start 20:00\n\n",
            "https://open.spotify.com/artist/1apVNEUv0uRWxeR7gLuKmf"
        );
        assert!(event(Some(description), None).validate().is_ok());
    }

    #[test]
    fn tabs_and_carriage_returns_are_accepted_too() {
        assert!(
            event(Some("line one\r\n\tindented"), None)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn other_control_characters_are_still_rejected() {
        assert!(
            event(Some("paragraph\u{0}injected"), None)
                .validate()
                .is_err()
        );
        assert!(event(Some("bell\u{7}"), None).validate().is_err());
    }

    #[test]
    fn surrounding_whitespace_is_still_rejected() {
        assert!(event(Some("\nleading"), None).validate().is_err());
        assert!(event(Some("trailing\n"), None).validate().is_err());
    }

    /// Loosening description must not loosen the single-line fields.
    #[test]
    fn venue_remains_single_line() {
        assert!(event(None, Some("Klub Łącznik")).validate().is_ok());
        assert!(event(None, Some("Klub\nŁącznik")).validate().is_err());
    }
}

#[cfg(test)]
mod act_tests {
    use super::*;

    #[test]
    fn act_slug_grammar_matches_the_check_constraint() {
        for slug in ["virya", "the-openers-2", "a", "x".repeat(64).as_str()] {
            assert!(valid_act_slug(slug), "expected valid: {slug:?}");
        }
        for slug in [
            "",
            "-leading-hyphen",
            "Uppercase",
            "with space",
            "under_score",
            "ünïcode",
            "x".repeat(65).as_str(),
        ] {
            assert!(!valid_act_slug(slug), "expected invalid: {slug:?}");
        }
        // A trailing hyphen is ugly but legal — the CHECK allows it, so the
        // shared predicate must too, or a stored row would fail reads.
        assert!(valid_act_slug("virya-"));
    }

    #[test]
    fn set_act_slug_normalizes_and_rejects() {
        let mut action = EventAction::new(
            WorkspaceId::new(),
            EventId::new(),
            EventActionKind::TicketClick,
            None,
            None,
            None,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("action should build");
        action
            .set_act_slug("  Virya  ")
            .expect("trimmed lowercase slug should set");
        assert_eq!(action.act_slug(), Some("virya"));
        assert_eq!(
            action.set_act_slug("not a slug!"),
            Err(EventActionError::InvalidActSlug)
        );
        // A failed set leaves the action untouched — the prior attribution
        // stands rather than being cleared by a bad update.
        assert_eq!(action.act_slug(), Some("virya"));
    }

    #[test]
    fn act_field_validation_matches_read_path_strictness() {
        assert!(validate_act_fields("virya", "Virya", None).is_ok());
        assert!(validate_act_fields("virya", "Virya", Some("https://tickets.example/x")).is_ok());
        // Write paths must refuse everything the read path would — otherwise
        // a stored bill ejects the whole event from the public cache.
        for bad_url in [
            "http://insecure.example/x",
            "https://exa mple.com",
            "https://user:pass@example.com/x",
            "https://example.com/#frag",
            "not a url",
        ] {
            assert!(
                validate_act_fields("virya", "Virya", Some(bad_url)).is_err(),
                "expected rejection: {bad_url:?}"
            );
        }
        assert!(validate_act_fields("virya", "with\nnewline", None).is_err());
        assert!(validate_act_fields("virya", " padded ", None).is_err());
        assert!(validate_act_fields("virya", "", None).is_err());
        assert!(validate_act_fields("virya", &"n".repeat(161), None).is_err());
        assert!(validate_act_fields("Bad Slug", "Virya", None).is_err());
    }

    #[test]
    fn event_validation_rejects_a_poisoned_bill() {
        let mut event = description_tests::event(None, None);
        event.acts.push(PublicEventAct {
            act_slug: "virya".to_owned(),
            act_name: "Virya".to_owned(),
            ticket_url: Some("https://tickets.example/virya".to_owned()),
        });
        assert!(event.validate().is_ok());
        event.acts.push(PublicEventAct {
            act_slug: "bad".to_owned(),
            act_name: "Line\nBreak".to_owned(),
            ticket_url: None,
        });
        assert_eq!(event.validate(), Err(PublicEventError::InvalidActs));
    }
}
