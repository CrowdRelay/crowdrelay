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

//! CrowdRelay use cases and infrastructure ports.
//!
//! This crate orchestrates domain logic through use-case structs that depend
//! on repository trait ports. Infrastructure crates implement these ports
//! against PostgreSQL or other backends. Thread-safe immutable caches power
//! the redirect and event-discovery fast paths.

pub mod admission;
pub mod agent_outcomes;
pub mod area_admin;
pub mod autopilot;
pub mod beacon_release;
pub mod cache;
pub mod ecosystem;
pub mod events;
pub mod fan_lifecycle;
pub mod ports;
pub mod referrals;
pub mod use_cases;

pub use admission::{
    AdmissionRepository, AdmissionUseCaseError, ClaimAdmissionPass, ClaimAdmissionPassCommand,
    IssueAdmissionPass, IssueAdmissionPassCommand, LoadAdmissionPass, RedeemAdmissionPass,
    RedeemAdmissionPassCommand, RevokeAdmissionPass, RevokeAdmissionPassCommand,
};
pub use area_admin::{
    AreaAdminError, AreaAdminRepository, AreaAdminService, AreaCity, AreaDropDetail,
    AreaDropSummary, AreaOverview, AreaValidationResult, CreateAreaCityCommand,
    CreateAreaDropCommand,
};
pub use beacon_release::{
    BeaconReleaseActivationCopy, BeaconReleaseRecipientTransition, BeaconReleaseTransitionError,
    beacon_release_activation_copy, validate_beacon_release_recipient_transition,
};
pub use cache::{RedirectCache, RedirectCacheError, RedirectSnapshot};
pub use ecosystem::{
    EcosystemControlPlaneRepository, EcosystemRepositoryError, FeatureFlagMutation,
    FeatureFlagState, ReconciliationFindingState, ReconciliationOutcome, ReconciliationRunState,
    RunReconciliationCommand, ShowChecklistItemState, ShowChecklistMutation,
    UpdateFeatureFlagCommand, UpdateShowChecklistCommand,
};
pub use events::{
    EventCache, EventCacheError, EventRepository, EventSnapshot, ListFanEventInterests, LoadEvents,
    LoadEventsError, MAX_PUBLIC_EVENT_LIMIT, RegisterEventInterest, RegisterEventInterestCommand,
    RegisterEventInterestCommandArgs, RegisterEventInterestCommandError,
};
pub use fan_lifecycle::{
    ConfirmFan, ConfirmFanCommand, FanLifecycleError, FanLifecycleRepository, UnsubscribeFan,
};
pub use ports::{
    AcquisitionRepository, IdempotencyKey, RepositoryError, RequestId, SignupFanCommand,
    TextKeyError, UpsertSmartLinkCommand, UpsertedSmartLink,
};
pub use referrals::{
    LoadReferralProgress, RedeemCoupon, RedeemCouponCommand, RedeemCouponCommandError,
    ReferralRepository, ResolveReferralCode,
};
pub use use_cases::{
    ListCities, ListCitiesError, LoadSmartLinks, LoadSmartLinksError, SignupFan, SignupFanError,
};
