//! Event discovery, interest registration and conversion ports/use cases.
//!
//! Provides the in-memory event cache, repository port, and use cases for
//! loading published events, registering fan interest, and listing a fan's
//! event interests.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use crowdrelay_domain::{
    CampaignId, EventAction, EventInterestResult, EventSlug, FanEventInterest, FanSessionToken,
    PublicEvent, VisitorId, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{IdempotencyKey, RepositoryError, RequestId};

/// Maximum number of events returned in a single public listing request.
pub const MAX_PUBLIC_EVENT_LIMIT: u32 = 100;

/// Immutable snapshot of published events keyed by workspace and slug,
/// with chronological ordering preserved per workspace.
#[derive(Clone, Debug, Default)]
pub struct EventSnapshot {
    events: HashMap<WorkspaceId, HashMap<EventSlug, PublicEvent>>,
    ordered: HashMap<WorkspaceId, Vec<EventSlug>>,
    len: usize,
}

impl EventSnapshot {
    /// Resolves an event by workspace and slug.
    #[must_use]
    pub fn resolve(&self, workspace_id: WorkspaceId, slug: &EventSlug) -> Option<&PublicEvent> {
        self.events.get(&workspace_id)?.get(slug)
    }

    /// Lists up to `limit` events for the given workspace in chronological order.
    #[must_use]
    pub fn list(&self, workspace_id: WorkspaceId, limit: u32) -> Vec<PublicEvent> {
        self.ordered
            .get(&workspace_id)
            .into_iter()
            .flatten()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .filter_map(|slug| self.resolve(workspace_id, slug).cloned())
            .collect()
    }

    /// Returns the total number of events in the snapshot.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the snapshot contains no events.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Thread-safe event cache holding an `Arc`-shared immutable snapshot.
#[derive(Debug, Default)]
pub struct EventCache {
    snapshot: RwLock<Arc<EventSnapshot>>,
}

impl EventCache {
    /// Creates an empty event cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a clone of the current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<EventSnapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Resolves an event by workspace and slug, returning a clone.
    #[must_use]
    pub fn resolve(&self, workspace_id: WorkspaceId, slug: &EventSlug) -> Option<PublicEvent> {
        self.snapshot().resolve(workspace_id, slug).cloned()
    }

    /// Lists up to `limit` events for the given workspace in chronological order.
    #[must_use]
    pub fn list(&self, workspace_id: WorkspaceId, limit: u32) -> Vec<PublicEvent> {
        self.snapshot().list(workspace_id, limit)
    }
}

impl EventCache {
    /// Replaces all events for a single workspace, preserving other workspaces'
    /// entries. Rejects duplicate slugs and invalid event data.
    pub fn replace_for_workspace<I>(
        &self,
        workspace_id: WorkspaceId,
        events: I,
    ) -> Result<usize, EventCacheError>
    where
        I: IntoIterator<Item = PublicEvent>,
    {
        let mut workspace_events = HashMap::new();
        let mut ordered_events = Vec::new();
        for event in events {
            event
                .validate()
                .map_err(|_| EventCacheError::InvalidEvent)?;
            let slug = event.slug.clone();
            let starts_at = event.starts_at;
            if workspace_events.insert(slug.clone(), event).is_some() {
                return Err(EventCacheError::DuplicateEvent { workspace_id, slug });
            }
            ordered_events.push((slug, starts_at));
        }
        let workspace_len = workspace_events.len();
        ordered_events.sort_by(|(left_slug, left_start), (right_slug, right_start)| {
            left_start
                .cmp(right_start)
                .then_with(|| left_slug.cmp(right_slug))
        });
        let slugs = ordered_events.into_iter().map(|(slug, _)| slug).collect();

        // Merge from the latest snapshot while holding the write lock. Cloning
        // before taking this lock permits two concurrent tenant refreshes to
        // overwrite one another with stale snapshots.
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut replacement = (**current).clone();
        if let Some(previous) = replacement.events.remove(&workspace_id) {
            replacement.len = replacement.len.saturating_sub(previous.len());
        }
        replacement.ordered.remove(&workspace_id);
        replacement.len = replacement
            .len
            .checked_add(workspace_len)
            .ok_or(EventCacheError::CapacityExceeded)?;
        replacement.events.insert(workspace_id, workspace_events);
        replacement.ordered.insert(workspace_id, slugs);
        *current = Arc::new(replacement);
        Ok(workspace_len)
    }
}

/// Error returned when an event cache replacement fails.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EventCacheError {
    /// The snapshot exceeded addressable capacity.
    #[error("event snapshot exceeds addressable capacity")]
    CapacityExceeded,
    /// The snapshot contained a duplicate slug within the same workspace.
    #[error("event snapshot contains duplicate slug {slug} in workspace {workspace_id}")]
    DuplicateEvent {
        workspace_id: WorkspaceId,
        slug: EventSlug,
    },
    /// The snapshot contained invalid stored event data.
    #[error("event snapshot contains invalid stored event data")]
    InvalidEvent,
}

/// Command for registering a fan's interest in an event.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RegisterEventInterestCommand {
    workspace_id: WorkspaceId,
    event_slug: EventSlug,
    fan_session: FanSessionToken,
    idempotency_key: IdempotencyKey,
    request_id: RequestId,
    campaign_id: Option<CampaignId>,
    visitor_id: Option<VisitorId>,
    source: String,
}

/// Arguments for constructing a [`RegisterEventInterestCommand`].
pub struct RegisterEventInterestCommandArgs {
    pub workspace_id: WorkspaceId,
    pub event_slug: EventSlug,
    pub fan_session: FanSessionToken,
    pub idempotency_key: IdempotencyKey,
    pub request_id: RequestId,
    pub campaign_id: Option<CampaignId>,
    pub visitor_id: Option<VisitorId>,
    pub source: String,
}

impl RegisterEventInterestCommand {
    /// Creates an interest-registration command, validating the source string.
    pub fn new(
        args: RegisterEventInterestCommandArgs,
    ) -> Result<Self, RegisterEventInterestCommandError> {
        let RegisterEventInterestCommandArgs {
            workspace_id,
            event_slug,
            fan_session,
            idempotency_key,
            request_id,
            campaign_id,
            visitor_id,
            source,
        } = args;
        if source.trim() != source
            || source.is_empty()
            || source.len() > 128
            || source.chars().any(char::is_control)
        {
            return Err(RegisterEventInterestCommandError::InvalidSource);
        }
        Ok(Self {
            workspace_id,
            event_slug,
            fan_session,
            idempotency_key,
            request_id,
            campaign_id,
            visitor_id,
            source,
        })
    }

    /// Returns the workspace ID.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }
    /// Returns the event slug.
    #[must_use]
    pub fn event_slug(&self) -> &EventSlug {
        &self.event_slug
    }
    /// Returns the fan session token.
    #[must_use]
    pub fn fan_session(&self) -> &FanSessionToken {
        &self.fan_session
    }
    /// Returns the idempotency key.
    #[must_use]
    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }
    /// Returns the request ID for tracing.
    #[must_use]
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }
    /// Returns the optional campaign ID.
    #[must_use]
    pub const fn campaign_id(&self) -> Option<CampaignId> {
        self.campaign_id
    }
    /// Returns the optional visitor ID.
    #[must_use]
    pub const fn visitor_id(&self) -> Option<VisitorId> {
        self.visitor_id
    }
    /// Returns the source channel where interest was registered.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for RegisterEventInterestCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegisterEventInterestCommand")
            .field("workspace_id", &self.workspace_id)
            .field("event_slug", &self.event_slug)
            .field("fan_session", &"[REDACTED]")
            .field("idempotency_key", &self.idempotency_key)
            .field("request_id", &self.request_id)
            .field("campaign_id", &self.campaign_id)
            .field("visitor_id", &self.visitor_id)
            .field("source", &self.source)
            .finish()
    }
}

/// Error returned when constructing a [`RegisterEventInterestCommand`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RegisterEventInterestCommandError {
    /// The source string was empty, too long, or contained control characters.
    #[error("event interest source must contain 1 to 128 bytes and no control characters")]
    InvalidSource,
}

/// Repository port for event discovery, interest registration, and action tracking.
#[async_trait]
pub trait EventRepository: Send + Sync {
    /// Loads all published events for cache refresh.
    async fn load_published_events(&self) -> Result<Vec<PublicEvent>, RepositoryError>;
    /// Persists a batch of conversion actions.
    async fn persist_event_action(&self, actions: &[EventAction]) -> Result<(), RepositoryError>;
    /// Registers a fan's interest in an event. Idempotent replays return the original result.
    async fn register_interest(
        &self,
        command: &RegisterEventInterestCommand,
    ) -> Result<EventInterestResult, RepositoryError>;
    /// Lists a fan's event interests using their session token.
    async fn list_fan_interests(
        &self,
        workspace_id: WorkspaceId,
        session_token: &FanSessionToken,
        limit: u32,
    ) -> Result<Vec<FanEventInterest>, RepositoryError>;
}

/// Use case: loads published events from the repository and refreshes the event cache.
#[derive(Clone)]
pub struct LoadEvents {
    repository: Arc<dyn EventRepository>,
    cache: Arc<EventCache>,
    workspace_id: WorkspaceId,
}

impl LoadEvents {
    /// Creates a cache-refresh use case for the given workspace.
    #[must_use]
    pub fn new(
        repository: Arc<dyn EventRepository>,
        cache: Arc<EventCache>,
        workspace_id: WorkspaceId,
    ) -> Self {
        Self {
            repository,
            cache,
            workspace_id,
        }
    }

    /// Loads events from the repository and replaces the workspace's cache entries.
    pub async fn execute(&self) -> Result<usize, LoadEventsError> {
        let events = self.repository.load_published_events().await?;
        self.cache
            .replace_for_workspace(self.workspace_id, events)
            .map_err(LoadEventsError::Cache)
    }
}

/// Error returned by the event cache-refresh use case.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LoadEventsError {
    /// The repository returned an error.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// The cache replacement detected duplicates, invalid data, or capacity overflow.
    #[error(transparent)]
    Cache(#[from] EventCacheError),
}

/// Use case: registers a fan's interest in an event.
#[derive(Clone)]
pub struct RegisterEventInterest {
    repository: Arc<dyn EventRepository>,
}

impl RegisterEventInterest {
    /// Creates an interest-registration use case.
    #[must_use]
    pub fn new(repository: Arc<dyn EventRepository>) -> Self {
        Self { repository }
    }

    /// Registers interest in an event. Idempotent replays return the original result.
    pub async fn execute(
        &self,
        command: &RegisterEventInterestCommand,
    ) -> Result<EventInterestResult, RepositoryError> {
        self.repository.register_interest(command).await
    }
}

/// Use case: lists a fan's event interests using their session token.
#[derive(Clone)]
pub struct ListFanEventInterests {
    repository: Arc<dyn EventRepository>,
}

impl ListFanEventInterests {
    /// Creates a fan-interest listing use case.
    #[must_use]
    pub fn new(repository: Arc<dyn EventRepository>) -> Self {
        Self { repository }
    }

    /// Lists up to `limit` event interests for the authenticated fan.
    pub async fn execute(
        &self,
        workspace_id: WorkspaceId,
        session_token: &FanSessionToken,
        limit: u32,
    ) -> Result<Vec<FanEventInterest>, RepositoryError> {
        if !(1..=MAX_PUBLIC_EVENT_LIMIT).contains(&limit) {
            return Err(RepositoryError::Conflict);
        }
        self.repository
            .list_fan_interests(workspace_id, session_token, limit)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        mpsc::{Receiver, SyncSender},
    };

    use crowdrelay_domain::{EventId, EventSlug, PublicEvent, WorkspaceId};
    use time::OffsetDateTime;

    use super::EventCache;

    fn event(slug: &str, starts_at: i64) -> Result<PublicEvent, Box<dyn std::error::Error>> {
        Ok(PublicEvent {
            id: EventId::new(),
            slug: EventSlug::parse(slug)?,
            title: slug.to_owned(),
            description: None,
            city: None,
            venue: None,
            venue_address: None,
            timezone: "Europe/Warsaw".to_owned(),
            starts_at: OffsetDateTime::from_unix_timestamp(starts_at)?,
            doors_at: None,
            ends_at: None,
            ticket_url: None,
            listen_url: None,
            image_url: None,
            trailer_url: None,
            external_event_url: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        })
    }

    #[test]
    fn replacing_one_workspace_preserves_other_workspaces() -> Result<(), Box<dyn std::error::Error>>
    {
        let cache = EventCache::new();
        let virya = WorkspaceId::new();
        let another = WorkspaceId::new();

        assert_eq!(
            cache.replace_for_workspace(virya, [event("virya-one", 20)?])?,
            1
        );
        assert_eq!(
            cache.replace_for_workspace(another, [event("other-one", 30)?])?,
            1
        );
        assert!(
            cache
                .resolve(virya, &EventSlug::parse("virya-one")?)
                .is_some()
        );
        assert!(
            cache
                .resolve(another, &EventSlug::parse("other-one")?)
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn listing_is_chronological_and_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let cache = EventCache::new();
        let workspace = WorkspaceId::new();
        cache.replace_for_workspace(
            workspace,
            [
                event("later", 30)?,
                event("earlier", 10)?,
                event("middle", 20)?,
            ],
        )?;

        let events = cache.list(workspace, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].slug.as_str(), "earlier");
        assert_eq!(events[1].slug.as_str(), "middle");
        Ok(())
    }

    struct BlockingEvent {
        event: Option<PublicEvent>,
        entered: Option<SyncSender<()>>,
        release: Receiver<()>,
    }

    impl Iterator for BlockingEvent {
        type Item = PublicEvent;

        fn next(&mut self) -> Option<Self::Item> {
            let event = self.event.take()?;
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
                let _ = self.release.recv();
            }
            Some(event)
        }
    }

    #[test]
    fn concurrent_workspace_replacements_do_not_lose_updates()
    -> Result<(), Box<dyn std::error::Error>> {
        let cache = Arc::new(EventCache::new());
        let first_workspace = WorkspaceId::new();
        let second_workspace = WorkspaceId::new();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let blocked = BlockingEvent {
            event: Some(event("first-event", 10)?),
            entered: Some(entered_tx),
            release: release_rx,
        };

        let worker_cache = Arc::clone(&cache);
        let worker = std::thread::spawn(move || {
            worker_cache.replace_for_workspace(first_workspace, blocked)
        });
        entered_rx.recv()?;
        cache.replace_for_workspace(second_workspace, [event("second-event", 20)?])?;
        release_tx.send(())?;
        assert_eq!(worker.join().map_err(|_| "cache worker panicked")??, 1);

        assert!(
            cache
                .resolve(first_workspace, &EventSlug::parse("first-event")?)
                .is_some()
        );
        assert!(
            cache
                .resolve(second_workspace, &EventSlug::parse("second-event")?)
                .is_some()
        );
        assert_eq!(cache.snapshot().len(), 2);
        Ok(())
    }
}
