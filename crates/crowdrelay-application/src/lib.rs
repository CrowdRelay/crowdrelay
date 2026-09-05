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
pub mod beacon_release_admin;
pub mod beacon_signal;
pub mod cache;
pub mod commerce_inventory;
pub mod concert_qr;
pub mod ecosystem;
pub mod events;
pub mod fan_lifecycle;
pub mod ports;
pub mod referrals;
pub mod use_cases;

/// The brain's assessment of its own performance, re-exported so the HTTP
/// layer can report it without taking a direct dependency on the brain.
/// The rule is policy and belongs above infrastructure; the dependency edge
/// api -> brain does not exist and is not worth creating for a read model.
pub use crowdrelay_brain::self_assessment;

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
pub use beacon_release_admin::{
    BeaconReleaseAdminError, BeaconReleaseAdminRepository, CloseReleaseCampaignCommand,
    CloseReleaseCampaignResult, CreateReleaseCampaignCommand, CreateReleaseCampaignResult,
    LaunchReleaseCampaignCommand, LaunchReleaseCampaignResult, UpdateReleaseRecipientCommand,
    UpdateReleaseRecipientResult,
};
pub use beacon_signal::{
    BeaconPreferences, BeaconSignalRepository, BeaconSignalRepositoryError, CreateInviteCommand,
    CreateInviteResult, CreatePressRequestCommand, CreatePressRequestResult, EmitNearbyCommand,
    EmitNearbyResult, ExchangeInviteCommand, ExchangeInviteResult, LeaveCommand, LogoutCommand,
    RecordEngagementCommand, RecordEngagementResult, SubmitCoverageCommand, SubmitCoverageResult,
    UpdatePreferencesCommand,
};
pub use cache::{RedirectCache, RedirectCacheError, RedirectSnapshot};
pub use commerce_inventory::{
    CommerceInventoryError, CommerceInventoryRepository, InventoryActivationState,
    MarkInventoryReadyCommand, MarkInventoryReadyResult, StocktakeCommand, StocktakeItemInput,
    StocktakeItemResult, StocktakeResult,
};
pub use concert_qr::{
    CheckinCommand, CheckinResult, ConcertEventInfo, ConcertQrError, ConcertQrRepository,
    CreateCampaignCommand, CreateCampaignResult, RevokeCampaignCommand,
};
/// Re-exported brain types used in API responses.
pub use crowdrelay_brain::ReachMetrics;
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
