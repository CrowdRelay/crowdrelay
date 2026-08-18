//! AREA Designer use cases and repository boundary.

use std::sync::Arc;

use async_trait::async_trait;
use crowdrelay_domain::{AreaDropDraft, AreaDropStatus, AreaValidationIssue, WorkspaceId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaCity {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub country_code: String,
    pub region: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub moderation_status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaDropSummary {
    pub id: String,
    pub number: String,
    pub city_id: Uuid,
    pub city: String,
    pub region: String,
    pub status: AreaDropStatus,
    pub active: bool,
    pub revision: i64,
    pub has_draft: bool,
    pub has_exact_location: bool,
    pub claim_count: i64,
    pub max_claims: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaDropDetail {
    pub summary: AreaDropSummary,
    pub published: AreaDropDraft,
    pub draft: Option<AreaDropDraft>,
    pub draft_base_revision: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaOverview {
    pub enabled: bool,
    pub total: usize,
    pub live: usize,
    pub scheduled: usize,
    pub drafts: usize,
    pub ended: usize,
    pub paused: usize,
    pub archived: usize,
    pub total_claims: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaValidationResult {
    pub valid: bool,
    pub issues: Vec<AreaValidationIssue>,
}

#[derive(Debug, Error)]
pub enum AreaAdminError {
    #[error("AREA record was not found")]
    NotFound,
    #[error("AREA command conflicts with current state: {0}")]
    Conflict(&'static str),
    #[error("AREA draft is invalid")]
    Invalid(Vec<AreaValidationIssue>),
    #[error("AREA repository failed: {0}")]
    Repository(String),
}

#[async_trait]
pub trait AreaAdminRepository: Send + Sync {
    async fn enabled(&self, workspace_id: WorkspaceId) -> Result<bool, AreaAdminError>;
    async fn set_enabled(
        &self,
        workspace_id: WorkspaceId,
        enabled: bool,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<bool, AreaAdminError>;
    async fn list_cities(
        &self,
        query: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AreaCity>, AreaAdminError>;
    async fn create_city(
        &self,
        workspace_id: WorkspaceId,
        city: CreateAreaCityCommand,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaCity, AreaAdminError>;
    async fn list_drops(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<AreaDropSummary>, AreaAdminError>;
    async fn get_drop(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
    ) -> Result<AreaDropDetail, AreaAdminError>;
    async fn create_draft(
        &self,
        workspace_id: WorkspaceId,
        command: CreateAreaDropCommand,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError>;
    async fn save_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        base_revision: i64,
        draft: AreaDropDraft,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError>;
    async fn discard_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<(), AreaAdminError>;
    async fn validate_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
    ) -> Result<AreaValidationResult, AreaAdminError>;
    async fn publish(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        confirmations: &[String],
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError>;
    async fn set_active(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        active: bool,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError>;
    async fn archive(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError>;
    async fn duplicate(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        new_drop_id: &str,
        new_city_id: Uuid,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError>;
    async fn delete_unpublished(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<(), AreaAdminError>;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateAreaCityCommand {
    pub slug: String,
    pub name: String,
    pub country_code: String,
    pub region: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone)]
pub struct CreateAreaDropCommand {
    pub drop_id: String,
    pub draft: AreaDropDraft,
}

#[derive(Clone)]
pub struct AreaAdminService {
    repository: Arc<dyn AreaAdminRepository>,
}

impl AreaAdminService {
    #[must_use]
    pub fn new(repository: Arc<dyn AreaAdminRepository>) -> Self {
        Self { repository }
    }
    pub async fn enabled(&self, workspace_id: WorkspaceId) -> Result<bool, AreaAdminError> {
        self.repository.enabled(workspace_id).await
    }
    pub async fn set_enabled(
        &self,
        workspace_id: WorkspaceId,
        enabled: bool,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<bool, AreaAdminError> {
        self.repository
            .set_enabled(workspace_id, enabled, actor, request_id)
            .await
    }
    pub async fn list_cities(
        &self,
        query: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AreaCity>, AreaAdminError> {
        self.repository.list_cities(query, limit).await
    }
    pub async fn create_city(
        &self,
        workspace_id: WorkspaceId,
        city: CreateAreaCityCommand,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaCity, AreaAdminError> {
        self.repository
            .create_city(workspace_id, city, actor, request_id)
            .await
    }
    pub async fn list_drops(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<AreaDropSummary>, AreaAdminError> {
        self.repository.list_drops(workspace_id).await
    }
    pub async fn get_drop(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        self.repository.get_drop(workspace_id, drop_id).await
    }
    pub async fn create_draft(
        &self,
        workspace_id: WorkspaceId,
        command: CreateAreaDropCommand,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        self.repository
            .create_draft(workspace_id, command, actor, request_id)
            .await
    }
    pub async fn save_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        base_revision: i64,
        draft: AreaDropDraft,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        self.repository
            .save_draft(
                workspace_id,
                drop_id,
                base_revision,
                draft,
                actor,
                request_id,
            )
            .await
    }
    pub async fn discard_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<(), AreaAdminError> {
        self.repository
            .discard_draft(workspace_id, drop_id, actor, request_id)
            .await
    }
    pub async fn validate_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
    ) -> Result<AreaValidationResult, AreaAdminError> {
        self.repository.validate_draft(workspace_id, drop_id).await
    }
    pub async fn publish(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        confirmations: &[String],
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        self.repository
            .publish(workspace_id, drop_id, confirmations, actor, request_id)
            .await
    }
    pub async fn set_active(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        active: bool,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        self.repository
            .set_active(workspace_id, drop_id, active, actor, request_id)
            .await
    }
    pub async fn archive(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        self.repository
            .archive(workspace_id, drop_id, actor, request_id)
            .await
    }
    pub async fn duplicate(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        new_drop_id: &str,
        new_city_id: Uuid,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        self.repository
            .duplicate(
                workspace_id,
                drop_id,
                new_drop_id,
                new_city_id,
                actor,
                request_id,
            )
            .await
    }
    pub async fn delete_unpublished(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<(), AreaAdminError> {
        self.repository
            .delete_unpublished(workspace_id, drop_id, actor, request_id)
            .await
    }

    pub async fn overview(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<AreaOverview, AreaAdminError> {
        let items = self.list_drops(workspace_id).await?;
        let count = |status| items.iter().filter(|item| item.status == status).count();
        Ok(AreaOverview {
            enabled: self.enabled(workspace_id).await?,
            total: items.len(),
            live: count(AreaDropStatus::Live),
            scheduled: count(AreaDropStatus::Scheduled),
            drafts: count(AreaDropStatus::Draft),
            ended: count(AreaDropStatus::Ended),
            paused: count(AreaDropStatus::Paused),
            archived: count(AreaDropStatus::Archived),
            total_claims: items.iter().map(|item| item.claim_count).sum(),
        })
    }
}
