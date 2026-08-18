//! Thread-safe immutable snapshots for the redirect fast path.
//!
//! Provides an `Arc`-shared, `RwLock`-guarded snapshot of smart-link
//! definitions. Readers always observe a complete, consistent snapshot;
//! writers build a replacement before swapping atomically.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crowdrelay_domain::{ResolvedSmartLink, SmartLinkSlug, WorkspaceId};
use thiserror::Error;

/// Immutable snapshot of smart-link definitions keyed by workspace and slug.
#[derive(Debug, Default)]
pub struct RedirectSnapshot {
    links: HashMap<WorkspaceId, HashMap<SmartLinkSlug, ResolvedSmartLink>>,
    len: usize,
}

impl RedirectSnapshot {
    /// Resolves a smart-link by workspace and slug.
    #[must_use]
    pub fn resolve(
        &self,
        workspace_id: WorkspaceId,
        slug: &SmartLinkSlug,
    ) -> Option<&ResolvedSmartLink> {
        self.links.get(&workspace_id)?.get(slug)
    }

    /// Returns the total number of smart-links in the snapshot.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the snapshot contains no smart-links.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Thread-safe redirect cache holding an `Arc`-shared immutable snapshot.
#[derive(Debug, Default)]
pub struct RedirectCache {
    snapshot: RwLock<Arc<RedirectSnapshot>>,
}

impl RedirectCache {
    /// Creates an empty redirect cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a complete replacement before taking the write lock. Duplicate
    /// tenant/slug keys reject the replacement and leave the old snapshot live.
    pub fn replace<I>(&self, links: I) -> Result<usize, RedirectCacheError>
    where
        I: IntoIterator<Item = ResolvedSmartLink>,
    {
        let mut replacement = RedirectSnapshot::default();

        for link in links {
            let workspace_id = link.workspace_id();
            let slug = link.slug().clone();
            let workspace_links = replacement.links.entry(workspace_id).or_default();
            // The displaced link carries the same slug, so the error path can
            // recover it instead of every accepted link paying for a second
            // copy that only a duplicate would ever read.
            if let Some(previous) = workspace_links.insert(slug, link) {
                return Err(RedirectCacheError::DuplicateLink {
                    workspace_id,
                    slug: previous.slug().clone(),
                });
            }
            replacement.len = replacement
                .len
                .checked_add(1)
                .ok_or(RedirectCacheError::CapacityExceeded)?;
        }

        let len = replacement.len;
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Arc::new(replacement);
        Ok(len)
    }

    /// Returns a clone of the current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<RedirectSnapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Resolves a smart-link by workspace and slug, returning a clone.
    #[must_use]
    pub fn resolve(
        &self,
        workspace_id: WorkspaceId,
        slug: &SmartLinkSlug,
    ) -> Option<ResolvedSmartLink> {
        self.snapshot().resolve(workspace_id, slug).cloned()
    }

    /// Returns the total number of smart-links in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// Returns `true` if the cache contains no smart-links.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.snapshot().is_empty()
    }
}

/// Error returned when a redirect cache replacement fails.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RedirectCacheError {
    /// The snapshot exceeded addressable capacity.
    #[error("redirect snapshot exceeds addressable capacity")]
    CapacityExceeded,
    /// The snapshot contained a duplicate slug within the same workspace.
    #[error("redirect snapshot contains duplicate slug {slug} in workspace {workspace_id}")]
    DuplicateLink {
        workspace_id: WorkspaceId,
        slug: SmartLinkSlug,
    },
}

#[cfg(test)]
mod tests {
    use std::thread;

    use crowdrelay_domain::{DestinationUrl, SmartLinkId};

    use super::*;

    fn link(
        workspace_id: WorkspaceId,
        slug: &str,
        destination: &str,
        version: u64,
    ) -> Result<ResolvedSmartLink, Box<dyn std::error::Error>> {
        Ok(ResolvedSmartLink::new(
            SmartLinkId::new(),
            workspace_id,
            None,
            SmartLinkSlug::parse(slug)?,
            DestinationUrl::parse(destination)?,
            version,
        )?)
    }

    #[test]
    fn replacement_is_tenant_scoped() -> Result<(), Box<dyn std::error::Error>> {
        let first_workspace = WorkspaceId::new();
        let second_workspace = WorkspaceId::new();
        let cache = RedirectCache::new();
        cache.replace([
            link(first_workspace, "join", "https://one.example/join", 1)?,
            link(second_workspace, "join", "https://two.example/join", 1)?,
        ])?;
        let slug = SmartLinkSlug::parse("join")?;

        assert_eq!(
            cache
                .resolve(first_workspace, &slug)
                .ok_or("missing first workspace link")?
                .destination_url()
                .as_str(),
            "https://one.example/join"
        );
        assert_eq!(
            cache
                .resolve(second_workspace, &slug)
                .ok_or("missing second workspace link")?
                .destination_url()
                .as_str(),
            "https://two.example/join"
        );
        Ok(())
    }

    #[test]
    fn duplicate_replacement_keeps_previous_snapshot() -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let cache = RedirectCache::new();
        cache.replace([link(workspace_id, "join", "https://stable.example/", 1)?])?;

        let error = cache
            .replace([
                link(workspace_id, "join", "https://new.example/", 2)?,
                link(workspace_id, "join", "https://duplicate.example/", 3)?,
            ])
            .unwrap_err();

        assert!(matches!(error, RedirectCacheError::DuplicateLink { .. }));
        assert_eq!(
            cache
                .resolve(workspace_id, &SmartLinkSlug::parse("join")?)
                .ok_or("missing link after duplicate replacement")?
                .destination_url()
                .as_str(),
            "https://stable.example/"
        );
        Ok(())
    }

    #[test]
    fn readers_observe_complete_old_or_new_snapshots() -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let slug = SmartLinkSlug::parse("join")?;
        let cache = Arc::new(RedirectCache::new());
        cache.replace([link(workspace_id, "join", "https://old.example/", 1)?])?;

        let reader_cache = Arc::clone(&cache);
        let reader_slug = slug.clone();
        let reader = thread::spawn(move || {
            for _ in 0..5_000 {
                let destination = reader_cache
                    .resolve(workspace_id, &reader_slug)
                    .ok_or("key exists in both snapshots")?;
                assert!(matches!(
                    destination.destination_url().as_str(),
                    "https://old.example/" | "https://new.example/"
                ));
            }
            Ok::<(), &str>(())
        });
        cache.replace([link(workspace_id, "join", "https://new.example/", 2)?])?;
        reader
            .join()
            .map_err(|e| format!("reader thread panicked: {e:?}"))?
            .map_err(|e| format!("reader error: {e}"))?;
        Ok(())
    }
}
