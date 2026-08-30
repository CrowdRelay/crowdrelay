#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used,
    )
)]
#![deny(clippy::dbg_macro)]

//! Pure CrowdRelay domain types and validation.
//!
//! This crate defines the core domain model: strongly typed identifiers,
//! validated value objects (slugs, emails, URLs, referral codes, country
//! codes), and the aggregate entities that flow through the application and
//! infrastructure layers. All types enforce their invariants at construction
//! time, so downstream code can rely on structural correctness without
//! re-validating.
//!
//! # Two tiers of modules — the engine/domain boundary
//!
//! Modules here split into two tiers, enforced by
//! `scripts/test_engine_boundaries_v1.py`:
//!
//! **Engine core** (domain-agnostic mechanics, reusable by any vertical):
//! `autonomy`, `action_class`, `growth_envelope`, `deliverability`,
//! `learning`, `performance`, `next_best_action`. These may not import or
//! name any bounded-context module. When a rule needs a domain fact (a
//! ceiling, a threshold, a subject id), it takes that fact as a parameter —
//! see `learning::effective_recipient_ceiling` taking a plain count rather
//! than a play policy.
//!
//! **Bounded contexts** (`plays`, `beacons`, `live_opportunities`,
//! `growth_metrics`, `fan_activation`, …): everything music-specific, plus
//! whatever a future vertical would swap in. Adding a new business domain
//! means adding contexts here and adapters in infra — never touching engine
//! core.
//!
//! # Privacy
//!
//! Secret-bearing types (`NormalizedEmail`, `ReferralCode`, `CouponCode`,
//! `PassClaimToken`, `PassSessionToken`, `FanActionToken`, `FanSessionToken`)
//! implement `Debug` with redacted fields to prevent accidental exposure in
//! logs and error messages.

pub mod acquisition;
pub mod acquisition_channel;
pub mod action_class;
pub mod admission;
pub mod area;
pub mod audience_graph;
pub mod audience_lifecycle;
pub mod autonomy;
pub mod beacon_release;
pub mod beacons;
pub mod booking;
pub mod booking_discovery;
pub mod calendar_routing;
pub mod campaign_lifecycle;
pub mod content_supply;
pub mod deliverability;
pub mod events;
pub mod experimentation;
pub mod fan_activation;
pub mod fan_lifecycle;
pub mod fanbase;
pub mod free_reach;
pub mod funding;
pub mod growth_debt;
pub mod growth_envelope;
pub mod growth_metrics;
pub mod ids;
pub mod learning;
pub mod live_opportunities;
pub mod market_intelligence;
pub mod merch_bundle;
pub mod merchandising;
pub mod negotiation;
pub mod next_best_action;
pub mod objectives;
pub mod outreach;
pub mod performance;
pub mod play_measurement;
pub mod playlist_placement;
pub mod plays;
pub mod portfolio;
pub mod pricing;
pub mod promotion;
pub mod referrals;
pub mod release_autopilot;
pub mod reply_triage;
pub mod show_growth;
pub mod show_operations;
pub mod show_settlement;
pub mod target_discovery;
pub mod team_operations;
pub mod tour_economics;
pub mod value_tier;
pub mod values;

pub use acquisition::{
    CitySignal, CitySignalError, ClickEvent, ClickEventError, FanSignup, FanSignupEmailKind,
    FanSignupError, FanSignupInput, FanSignupResult, FanStatus, MarketingConsent,
    MarketingConsentError, ResolvedSmartLink, ResolvedSmartLinkError,
};
pub use admission::{
    AdmissionPassClaimed, AdmissionPassIssued, AdmissionPassStatus, AdmissionPassView,
    AdmissionQrClaims, AdmissionQrError, AdmissionRedemptionResult, AdmissionRedemptionStatus,
    PassClaimToken, PassClaimTokenError, PassSessionToken, PassSessionTokenError,
};
pub use area::{
    AreaCollectible, AreaDropDraft, AreaDropStatus, AreaLocalizedClue, AreaValidationIssue,
    MAX_AREA_CLUE_CHARS, MAX_AREA_COLLECTIBLE_LINE_CHARS, MAX_AREA_LABEL_CHARS,
    changed_area_fields, derive_area_status, live_change_confirmation_issues, valid_area_drop_id,
};
pub use beacon_release::{
    BeaconReleaseCampaignPhase, BeaconReleaseCampaignState, BeaconReleaseProgress,
    BeaconReleaseRecipientState, BeaconReleaseStateError,
};
pub use beacons::BeaconContactIdentity;
pub use events::{
    EventAction, EventActionError, EventActionKind, EventCity, EventInterestResult, EventStatus,
    FanEventInterest, PublicEvent, PublicEventError,
};
pub use fan_lifecycle::{
    FanActionToken, FanActionTokenError, FanConfirmationResult, FanUnsubscribeResult,
};
pub use ids::{
    AdmissionPassId, AdmissionPoolId, AutopilotActionId, AutopilotDecisionId,
    AutopilotMeasurementId, BeaconId, BookingTargetId, CampaignId, CityId, ContentSourceId,
    EventId, ExperimentId, ExperimentVariantId, FanId, GrowthMetricSeriesId, MarketSignalId,
    MerchCouponId, MerchProductId, MerchVariantId, OutreachOpportunityId, OutreachTargetId,
    PassSessionId, PlayId, PromotionCampaignId, ReferralAttributionId, ReleasePlanId, RewardDrawId,
    RewardGrantId, RewardRuleId, SmartLinkId, TeamAssignmentId, TeamOpportunityId, TicketTypeId,
    VisitorId, WorkspaceId, WorkspaceMemberId, WorkspaceMemberSessionId,
};
pub use referrals::{
    CouponCode, CouponCodeError, CouponRedemptionResult, CouponStatus, FanSessionToken,
    FanSessionTokenError, MerchCoupon, MerchCouponError, PhysicalRewardGrant, PhysicalRewardStatus,
    QualifiedReferral, ReferralProgress, ReferralStatus, RewardDrawPrizeKind, WeightedDrawEntry,
};
pub use values::{
    CitySlug, CountryCode, CountryCodeError, DestinationUrl, DestinationUrlError, EventSlug,
    NormalizedEmail, NormalizedEmailError, ReferralCode, ReferralCodeError, SlugError,
    SmartLinkSlug, WorkspaceSlug,
};
