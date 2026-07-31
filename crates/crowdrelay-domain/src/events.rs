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
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl PublicEvent {
    /// Validates title, timezone, text fields, URLs, and schedule consistency.
    pub fn validate(&self) -> Result<(), PublicEventError> {
        validate_required_text(&self.title, 300).map_err(|_| PublicEventError::InvalidTitle)?;
        validate_required_text(&self.timezone, 128)
            .map_err(|_| PublicEventError::InvalidTimezone)?;
        validate_optional_text(self.description.as_deref(), 10_000)?;
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
            occurred_at,
        })
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
}
