//! Referral qualification, deterministic rewards, and merch coupon views.
//!
//! Defines referral status tracking, fan session tokens, coupon codes and
//! their lifecycle, merch discount coupons, physical reward grants, and
//! referral progress views shown to fans.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;
use time::OffsetDateTime;

use crate::{
    FanId, MerchCouponId, ReferralAttributionId, ReferralCode, RewardDrawId, RewardGrantId,
    RewardRuleId, WorkspaceId,
};

/// Lifecycle status of a referral attribution.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferralStatus {
    /// Referred fan has not yet qualified (e.g. email not confirmed).
    Pending,
    /// Referred fan has qualified and the referral counts toward rewards.
    Qualified,
    /// Referral was rejected (e.g. self-referral or abuse).
    Rejected,
    /// Previously qualified referral was reversed (e.g. fan unsubscribed).
    Reversed,
}

/// A qualified referral linking a referrer to a referred fan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QualifiedReferral {
    pub attribution_id: ReferralAttributionId,
    pub workspace_id: WorkspaceId,
    pub referrer_fan_id: FanId,
    pub referred_fan_id: FanId,
    pub status: ReferralStatus,
    pub qualified_at: OffsetDateTime,
}

/// Opaque session token issued to a fan after email confirmation.
///
/// `Debug` is deliberately redacted to prevent accidental logging.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FanSessionToken(String);

impl FanSessionToken {
    /// Parses a 256-bit hexadecimal fan session token.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, FanSessionTokenError> {
        let value = value.as_ref();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(FanSessionTokenError);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the normalized token as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for FanSessionToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Debug for FanSessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FanSessionToken")
            .field(&"[REDACTED]")
            .finish()
    }
}

/// Error returned when a fan session token fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("fan session token must contain exactly 64 hexadecimal characters")]
pub struct FanSessionTokenError;

/// A merch discount coupon code, validated and normalized to uppercase.
///
/// `Debug` is deliberately redacted to prevent accidental logging.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CouponCode(String);

impl CouponCode {
    /// Parses a coupon code, accepting 8–128 ASCII letters, digits, `-`, or `_`,
    /// and normalizing to uppercase.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, CouponCodeError> {
        let value = value.as_ref();
        if !(8..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(CouponCodeError);
        }
        Ok(Self(value.to_ascii_uppercase()))
    }

    /// Returns the normalized coupon code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for CouponCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Debug for CouponCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CouponCode")
            .field(&"[REDACTED]")
            .finish()
    }
}

/// Error returned when a coupon code fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("coupon code must contain 8 to 128 ASCII letters, digits, `-` or `_`")]
pub struct CouponCodeError;

/// Lifecycle status of a merch discount coupon.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CouponStatus {
    /// Coupon has been issued and is available for use.
    Issued,
    /// Coupon has been redeemed.
    Redeemed,
    /// Coupon has passed its expiry date.
    Expired,
    /// Coupon has been administratively revoked.
    Revoked,
}

/// Fan-visible merch discount coupon view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MerchCoupon {
    pub id: MerchCouponId,
    pub reward_grant_id: RewardGrantId,
    pub reward_rule_id: RewardRuleId,
    pub code: CouponCode,
    pub discount_percent: f64,
    pub max_uses: u32,
    pub used_count: u32,
    pub status: CouponStatus,
    pub expires_at: Option<OffsetDateTime>,
}

impl MerchCoupon {
    /// Validates the discount percentage and usage counters.
    pub fn validate(&self) -> Result<(), MerchCouponError> {
        if !(0.0 < self.discount_percent
            && self.discount_percent <= 100.0
            && self.discount_percent.is_finite())
        {
            return Err(MerchCouponError::InvalidDiscount);
        }
        if self.max_uses == 0 || self.used_count > self.max_uses {
            return Err(MerchCouponError::InvalidUsage);
        }
        Ok(())
    }
}

/// Error returned when a merch coupon fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MerchCouponError {
    /// The discount percentage was not finite or not in the range (0, 100].
    #[error("discount percent must be finite and between 0 and 100")]
    InvalidDiscount,
    /// The usage counters were inconsistent (zero max uses or used > max).
    #[error("coupon usage counters are invalid")]
    InvalidUsage,
}

/// Prize category of an active weighted draw.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RewardDrawPrizeKind {
    /// A claimable admission pass for a concert.
    AdmissionPass,
    /// A physical item fulfilled by the operator, for example an album.
    PhysicalItem,
}

/// Fan-visible entry balance for one active referral-weighted draw.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WeightedDrawEntry {
    pub draw_id: RewardDrawId,
    pub slug: String,
    pub name: String,
    pub prize_kind: RewardDrawPrizeKind,
    pub closes_at: OffsetDateTime,
    pub draw_at: OffsetDateTime,
    pub qualified_referrals: u64,
    pub base_entries: u32,
    pub referral_entries: u32,
    pub concert_checkins: u32,
    pub checkin_entries: u32,
    pub total_entries: u32,
    pub max_entries: u32,
}

/// Fan-visible referral progress view, including qualified/pending counts,
/// active weighted-draw entries, and granted rewards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferralProgress {
    pub referral_code: ReferralCode,
    pub qualified_referrals: u64,
    pub pending_referrals: u64,
    pub next_reward_threshold: Option<u32>,
    pub draw_entries: Vec<WeightedDrawEntry>,
    pub coupons: Vec<MerchCoupon>,
    pub physical_rewards: Vec<PhysicalRewardGrant>,
}

/// Fulfillment state of a physical-item reward grant (for example a free
/// physical copy of an album), tracked on the shared `reward_grants` lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalRewardStatus {
    /// Granted; not yet marked shipped by an operator.
    Issued,
    /// Marked shipped/handed over by an operator.
    Fulfilled,
    /// The fan did not qualify in time or the grant lapsed.
    Expired,
    /// An operator cancelled the grant.
    Revoked,
}

/// A fan-visible view of one physical-item reward (for example a free
/// physical copy of an album), granted either by a deterministic referral rule
/// or as the fulfillment result of an audited weighted draw.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhysicalRewardGrant {
    pub reward_grant_id: RewardGrantId,
    pub reward_rule_id: RewardRuleId,
    pub item_name: String,
    pub sku: String,
    pub status: PhysicalRewardStatus,
    pub granted_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
}

/// Result returned after redeeming a merch coupon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CouponRedemptionResult {
    pub coupon_id: MerchCouponId,
    pub reward_grant_id: RewardGrantId,
    pub status: CouponStatus,
    pub used_count: u32,
    pub max_uses: u32,
    pub redeemed_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coupon_code_debug_is_redacted() -> Result<(), Box<dyn std::error::Error>> {
        let code = CouponCode::parse("VIRYA-ABC12345")?;
        assert!(!format!("{code:?}").contains(code.as_str()));
        Ok(())
    }

    #[test]
    fn coupon_rejects_unsafe_text() {
        for value in ["short", "contains space", "contains/slash"] {
            assert!(CouponCode::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn coupon_codes_are_case_insensitive_at_the_boundary() -> Result<(), Box<dyn std::error::Error>>
    {
        let code = CouponCode::parse("virya-abc12345")?;
        assert_eq!(code.as_str(), "VIRYA-ABC12345");
        Ok(())
    }
}
