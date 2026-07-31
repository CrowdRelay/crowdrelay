//! Referral and merch reward application ports and use cases.
//!
//! Provides the repository port, coupon redemption command, and use cases
//! for resolving referral codes, loading referral progress, and redeeming
//! merch coupons.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use crowdrelay_domain::{
    CouponCode, CouponRedemptionResult, FanSessionToken, ReferralCode, ReferralProgress,
    WorkspaceId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{IdempotencyKey, RepositoryError, RequestId};

/// Command for redeeming a merch coupon with idempotency and tracing metadata.
///
/// `Debug` is deliberately redacted to prevent leaking the coupon code and order reference.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RedeemCouponCommand {
    workspace_id: WorkspaceId,
    idempotency_key: IdempotencyKey,
    request_id: RequestId,
    coupon_code: CouponCode,
    order_reference: String,
}

impl RedeemCouponCommand {
    /// Creates a redemption command, validating the order reference.
    pub fn new(
        workspace_id: WorkspaceId,
        idempotency_key: IdempotencyKey,
        request_id: RequestId,
        coupon_code: CouponCode,
        order_reference: impl Into<String>,
    ) -> Result<Self, RedeemCouponCommandError> {
        let order_reference = order_reference.into();
        if order_reference.trim() != order_reference
            || order_reference.is_empty()
            || order_reference.len() > 128
            || order_reference.chars().any(char::is_control)
        {
            return Err(RedeemCouponCommandError::InvalidOrderReference);
        }
        Ok(Self {
            workspace_id,
            idempotency_key,
            request_id,
            coupon_code,
            order_reference,
        })
    }

    /// Returns the workspace ID.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
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
    /// Returns the coupon code to redeem.
    #[must_use]
    pub fn coupon_code(&self) -> &CouponCode {
        &self.coupon_code
    }
    /// Returns the external order reference.
    #[must_use]
    pub fn order_reference(&self) -> &str {
        &self.order_reference
    }
}

impl fmt::Debug for RedeemCouponCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedeemCouponCommand")
            .field("workspace_id", &self.workspace_id)
            .field("idempotency_key", &self.idempotency_key)
            .field("request_id", &self.request_id)
            .field("coupon_code", &self.coupon_code)
            .field("order_reference", &"[REDACTED]")
            .finish()
    }
}

/// Error returned when constructing a [`RedeemCouponCommand`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RedeemCouponCommandError {
    /// The order reference was empty, too long, or contained control characters.
    #[error("order reference must contain 1 to 128 bytes and no control characters")]
    InvalidOrderReference,
}

/// Repository port for referral code resolution, progress loading, and coupon redemption.
#[async_trait]
pub trait ReferralRepository: Send + Sync {
    /// Checks whether a referral code is active for the given workspace.
    async fn referral_code_is_active(
        &self,
        workspace_id: WorkspaceId,
        code: &ReferralCode,
    ) -> Result<bool, RepositoryError>;

    /// Loads the referral progress view for the authenticated fan.
    async fn load_referral_progress(
        &self,
        workspace_id: WorkspaceId,
        session_token: &FanSessionToken,
    ) -> Result<ReferralProgress, RepositoryError>;

    /// Redeems a merch coupon. Idempotent replays return the original result.
    async fn redeem_coupon(
        &self,
        command: &RedeemCouponCommand,
    ) -> Result<CouponRedemptionResult, RepositoryError>;
}

/// Use case: checks whether a referral code is active.
#[derive(Clone)]
pub struct ResolveReferralCode {
    repository: Arc<dyn ReferralRepository>,
}

impl ResolveReferralCode {
    /// Creates a referral-code resolution use case.
    #[must_use]
    pub fn new(repository: Arc<dyn ReferralRepository>) -> Self {
        Self { repository }
    }

    /// Returns `true` if the referral code is active for the given workspace.
    pub async fn execute(
        &self,
        workspace_id: WorkspaceId,
        code: &ReferralCode,
    ) -> Result<bool, RepositoryError> {
        self.repository
            .referral_code_is_active(workspace_id, code)
            .await
    }
}

/// Use case: loads the referral progress view for the authenticated fan.
#[derive(Clone)]
pub struct LoadReferralProgress {
    repository: Arc<dyn ReferralRepository>,
}

impl LoadReferralProgress {
    /// Creates a referral-progress loading use case.
    #[must_use]
    pub fn new(repository: Arc<dyn ReferralRepository>) -> Self {
        Self { repository }
    }

    /// Loads referral progress using the fan's session token.
    pub async fn execute(
        &self,
        workspace_id: WorkspaceId,
        session_token: &FanSessionToken,
    ) -> Result<ReferralProgress, RepositoryError> {
        self.repository
            .load_referral_progress(workspace_id, session_token)
            .await
    }
}

/// Use case: redeems a merch coupon.
#[derive(Clone)]
pub struct RedeemCoupon {
    repository: Arc<dyn ReferralRepository>,
}

impl RedeemCoupon {
    /// Creates a coupon-redemption use case.
    #[must_use]
    pub fn new(repository: Arc<dyn ReferralRepository>) -> Self {
        Self { repository }
    }

    /// Redeems the coupon. Idempotent replays return the original result.
    pub async fn execute(
        &self,
        command: &RedeemCouponCommand,
    ) -> Result<CouponRedemptionResult, RepositoryError> {
        self.repository.redeem_coupon(command).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redemption_command_redacts_sensitive_values() -> Result<(), Box<dyn std::error::Error>> {
        let command = RedeemCouponCommand::new(
            WorkspaceId::new(),
            IdempotencyKey::parse("coupon-redeem-0001")?,
            RequestId::parse("request-0001")?,
            CouponCode::parse("VIRYA-ABC12345")?,
            "order-123",
        )?;
        let debug = format!("{command:?}");
        assert!(!debug.contains("VIRYA-ABC12345"));
        assert!(!debug.contains("order-123"));
        Ok(())
    }
}
