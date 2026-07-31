//! Acquisition application use cases.
//!
//! Provides the use-case entry points for fan signup, city listing, and
//! smart-link cache refresh. Each use case validates input at the boundary
//! before delegating to the repository port.

use std::sync::Arc;

use crowdrelay_domain::{CitySignal, FanSignupError, FanSignupResult, WorkspaceId};
use thiserror::Error;

use crate::{
    AcquisitionRepository, RedirectCache, RedirectCacheError, RepositoryError, SignupFanCommand,
};

/// Maximum number of city signals returned in a single public listing request.
pub const MAX_PUBLIC_CITY_LIMIT: u32 = 100;

/// Use case: persists a fan signup with consent, city interest, and optional referral.
#[derive(Clone)]
pub struct SignupFan {
    repository: Arc<dyn AcquisitionRepository>,
}

impl SignupFan {
    /// Creates a signup use case backed by the supplied repository.
    #[must_use]
    pub fn new(repository: Arc<dyn AcquisitionRepository>) -> Self {
        Self { repository }
    }

    /// Validates the signup and persists it atomically. Idempotent replays
    /// return the original result.
    pub async fn execute(
        &self,
        command: &SignupFanCommand,
    ) -> Result<FanSignupResult, SignupFanError> {
        command.signup().validate()?;
        self.repository
            .persist_fan_signup(command)
            .await
            .map_err(SignupFanError::Repository)
    }
}

/// Error returned by the signup use case.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SignupFanError {
    /// The signup input failed domain validation.
    #[error(transparent)]
    InvalidInput(#[from] FanSignupError),
    /// The repository returned an error.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Use case: lists anonymous city demand signals for a workspace.
#[derive(Clone)]
pub struct ListCities {
    repository: Arc<dyn AcquisitionRepository>,
}

impl ListCities {
    /// Creates a city-listing use case backed by the supplied repository.
    #[must_use]
    pub fn new(repository: Arc<dyn AcquisitionRepository>) -> Self {
        Self { repository }
    }

    /// Returns up to `limit` city signals, sorted by fan count descending.
    pub async fn execute(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<CitySignal>, ListCitiesError> {
        if !(1..=MAX_PUBLIC_CITY_LIMIT).contains(&limit) {
            return Err(ListCitiesError::InvalidLimit {
                max: MAX_PUBLIC_CITY_LIMIT,
            });
        }
        self.repository
            .list_city_signals(workspace_id, limit)
            .await
            .map_err(ListCitiesError::Repository)
    }
}

/// Error returned by the city-listing use case.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ListCitiesError {
    /// The requested limit was outside the range `1..=MAX_PUBLIC_CITY_LIMIT`.
    #[error("city limit must be between 1 and {max}")]
    InvalidLimit { max: u32 },
    /// The repository returned an error.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Use case: loads active smart-links from the repository and replaces the
/// in-memory redirect cache atomically.
#[derive(Clone)]
pub struct LoadSmartLinks {
    repository: Arc<dyn AcquisitionRepository>,
    cache: Arc<RedirectCache>,
}

impl LoadSmartLinks {
    /// Creates a cache-refresh use case backed by the supplied repository and cache.
    #[must_use]
    pub fn new(repository: Arc<dyn AcquisitionRepository>, cache: Arc<RedirectCache>) -> Self {
        Self { repository, cache }
    }

    /// Refreshes the cache only after a complete repository load. Both load and
    /// cache-validation failures leave the previous immutable snapshot live.
    pub async fn execute(&self) -> Result<usize, LoadSmartLinksError> {
        let links = self
            .repository
            .load_active_smart_links()
            .await
            .map_err(LoadSmartLinksError::Repository)?;
        self.cache
            .replace(links)
            .map_err(LoadSmartLinksError::Cache)
    }
}

/// Error returned by the smart-link cache-refresh use case.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LoadSmartLinksError {
    /// The repository returned an error.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// The cache replacement detected duplicates or capacity overflow.
    #[error(transparent)]
    Cache(#[from] RedirectCacheError),
}
