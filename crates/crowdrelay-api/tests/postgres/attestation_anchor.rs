//! An attestation's anchor is provable against a public root, not our HMAC.
//!
//! The batch builder is the claim under test: an issued attestation must show
//! up as an `external_proof_items` leaf exactly once, and `anchor_for_digest`
//! must be able to say so — and to say "not yet" as a stated state, because
//! "we cannot find this" reads to a sceptical reader like "this was forged".
//! Driving the real router matters: the candidates CTE, the idempotency
//! replay and the public inclusion route are the feature, and none of it is
//! reachable without HTTP.
//!
//! Runs under `just test-postgres`, which provisions and migrates the
//! disposable database this file points at via `CROWDRELAY_TEST_DATABASE_URL`.

use crate::common;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{
        Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
};
use crowdrelay_api::{
    AcquisitionState, AcquisitionStateArgs, AdmissionState, AdmissionStateArgs, AppState,
    ClickMetricsSnapshot, ConcertQrState, EventActionMetricsSnapshot, EventState,
    FanLifecycleState, HttpConfig, OpsState, PushPublicState, ReferralState, TicketingState,
    tenant::{
        RegionalSource, TenantPalette, TenantProducts, TenantProfile, TenantRegionalProfile,
        TenantRegionalProvenance,
    },
};
use crowdrelay_application::{
    AcquisitionRepository, AdmissionRepository, ClaimAdmissionPass, ConfirmFan, EventCache,
    EventRepository, FanLifecycleRepository, IssueAdmissionPass, ListCities, ListFanEventInterests,
    LoadAdmissionPass, LoadReferralProgress, RedeemAdmissionPass, RedeemCoupon, RedirectCache,
    ReferralRepository, RegisterEventInterest, ReplaceEventActs, RepositoryError,
    ResolveReferralCode, RevokeAdmissionPass, SetEventCounterparty, SignupFan, SignupFanCommand,
    UnsubscribeFan, UpsertSmartLinkCommand, UpsertedSmartLink,
};
use crowdrelay_domain::{
    CitySignal, ClickEvent, EventAction, EventInterestResult, FanEventInterest, FanSignupResult,
    PublicEvent, ReferralCode, ReferralProgress, ResolvedSmartLink, WorkspaceId, WorkspaceSlug,
};
use crowdrelay_infra::{
    attestation::{AttestationError, AttestationSigningKey, PostgresAttestationRepository},
    autopilot::PostgresAutopilotRepository,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

const ADMIN_KEY: &str = "test-admin-api-key-123456789012";
const SIGNING_SECRET: &[u8] = b"attestation-anchor-test-signing-secret";

// ── Disposable database, same pattern as the infra postgres suites. ──────────

// ── The smallest AppState that serves the real routes. ───────────────────────
//
// The attestation anchor path touches only the pool and the admin key, but the
// router is built whole — a half-built state would test a server that cannot
// exist.

struct StubAcquisition;

#[async_trait]
impl AcquisitionRepository for StubAcquisition {
    async fn resolve_workspace(
        &self,
        _slug: &WorkspaceSlug,
    ) -> Result<Option<WorkspaceId>, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn persist_click_batch(&self, _clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn persist_fan_signup(
        &self,
        _command: &SignupFanCommand,
    ) -> Result<FanSignupResult, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn list_city_signals(
        &self,
        _workspace_id: WorkspaceId,
        _limit: u32,
    ) -> Result<Vec<CitySignal>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn upsert_smart_link<'a>(
        &self,
        _command: &UpsertSmartLinkCommand<'a>,
    ) -> Result<UpsertedSmartLink, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn list_smart_links(
        &self,
        _workspace_id: WorkspaceId,
    ) -> Result<Vec<UpsertedSmartLink>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn load_or_create_fan_referral_code(
        &self,
        _workspace_id: WorkspaceId,
        _fan_id: crowdrelay_domain::FanId,
    ) -> Result<ReferralCode, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
}

struct StubReferrals;

#[async_trait]
impl ReferralRepository for StubReferrals {
    async fn referral_code_is_active(
        &self,
        _workspace_id: WorkspaceId,
        _code: &ReferralCode,
    ) -> Result<bool, RepositoryError> {
        Ok(true)
    }
    async fn load_referral_progress(
        &self,
        _workspace_id: WorkspaceId,
        _session_token: &crowdrelay_domain::FanSessionToken,
    ) -> Result<ReferralProgress, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn redeem_coupon(
        &self,
        _command: &crowdrelay_application::RedeemCouponCommand,
    ) -> Result<crowdrelay_domain::CouponRedemptionResult, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
}

struct StubEvents;

#[async_trait]
impl EventRepository for StubEvents {
    async fn load_published_events(&self) -> Result<Vec<PublicEvent>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn persist_event_action(&self, _actions: &[EventAction]) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn register_interest(
        &self,
        _command: &crowdrelay_application::RegisterEventInterestCommand,
    ) -> Result<EventInterestResult, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn list_fan_interests(
        &self,
        _workspace_id: WorkspaceId,
        _session_token: &crowdrelay_domain::FanSessionToken,
        _limit: u32,
    ) -> Result<Vec<FanEventInterest>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn replace_event_acts(
        &self,
        _command: &crowdrelay_application::ReplaceEventActsCommand,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn set_event_counterparty(
        &self,
        _command: &crowdrelay_application::SetEventCounterpartyCommand,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn set_event_support_slots(
        &self,
        _command: &crowdrelay_application::SetEventSupportSlotsCommand,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn set_event_festival(
        &self,
        _command: &crowdrelay_application::SetEventFestivalCommand,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

struct StubAdmission;

#[async_trait]
impl AdmissionRepository for StubAdmission {
    async fn issue_pass(
        &self,
        _command: &crowdrelay_application::IssueAdmissionPassCommand,
    ) -> Result<crowdrelay_domain::AdmissionPassIssued, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn claim_pass(
        &self,
        _command: &crowdrelay_application::ClaimAdmissionPassCommand,
    ) -> Result<crowdrelay_domain::AdmissionPassClaimed, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn load_pass(
        &self,
        _workspace_id: WorkspaceId,
        _session: &crowdrelay_domain::PassSessionToken,
    ) -> Result<crowdrelay_domain::AdmissionPassView, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn redeem_pass(
        &self,
        _command: &crowdrelay_application::RedeemAdmissionPassCommand,
    ) -> Result<crowdrelay_domain::AdmissionRedemptionResult, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn revoke_pass(
        &self,
        _command: &crowdrelay_application::RevokeAdmissionPassCommand,
    ) -> Result<crowdrelay_domain::AdmissionPassView, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
}

struct StubFanLifecycle;

#[async_trait]
impl FanLifecycleRepository for StubFanLifecycle {
    async fn confirm(
        &self,
        _command: &crowdrelay_application::ConfirmFanCommand,
    ) -> Result<crowdrelay_domain::FanConfirmationResult, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
    async fn unsubscribe(
        &self,
        _workspace_id: WorkspaceId,
        _token: &crowdrelay_domain::FanActionToken,
    ) -> Result<crowdrelay_domain::FanUnsubscribeResult, RepositoryError> {
        Err(RepositoryError::Unavailable)
    }
}

fn app_state(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let database = pool.clone();
    let timeout = Duration::from_secs(5);
    let acquisition_repository: Arc<dyn AcquisitionRepository> = Arc::new(StubAcquisition);
    let referral_repository: Arc<dyn ReferralRepository> = Arc::new(StubReferrals);
    let event_repository: Arc<dyn EventRepository> = Arc::new(StubEvents);
    let admission_repository: Arc<dyn AdmissionRepository> = Arc::new(StubAdmission);
    let lifecycle_repository: Arc<dyn FanLifecycleRepository> = Arc::new(StubFanLifecycle);

    let acquisition = AcquisitionState::new(AcquisitionStateArgs {
        workspace_id,
        redirect_cache: Arc::new(RedirectCache::new()),
        signup_fan: SignupFan::new(Arc::clone(&acquisition_repository)),
        list_cities: ListCities::new(Arc::clone(&acquisition_repository)),
        click_submitter: Arc::new(|_event| {}),
        click_metrics_reader: Arc::new(ClickMetricsSnapshot::default),
        public_site_base_url: Url::parse("http://localhost:4321")?,
        secure_cookies: false,
        acquisition_repository,
    });
    let referrals = ReferralState::new(
        workspace_id,
        ResolveReferralCode::new(Arc::clone(&referral_repository)),
        LoadReferralProgress::new(Arc::clone(&referral_repository)),
        RedeemCoupon::new(referral_repository),
        Url::parse("http://localhost:4321")?,
        false,
    );
    let events = EventState::new(
        workspace_id,
        Arc::new(EventCache::new()),
        RegisterEventInterest::new(Arc::clone(&event_repository)),
        ListFanEventInterests::new(Arc::clone(&event_repository)),
        ReplaceEventActs::new(Arc::clone(&event_repository)),
        SetEventCounterparty::new(Arc::clone(&event_repository)),
        crowdrelay_application::SetEventSupportSlots::new(Arc::clone(&event_repository)),
        crowdrelay_application::SetEventFestival::new(event_repository),
        Arc::new(|_action| {}),
        Arc::new(EventActionMetricsSnapshot::default),
    );
    let admission = AdmissionState::new(AdmissionStateArgs {
        workspace_id,
        issue_pass: IssueAdmissionPass::new(Arc::clone(&admission_repository)),
        claim_pass: ClaimAdmissionPass::new(Arc::clone(&admission_repository)),
        load_pass: LoadAdmissionPass::new(Arc::clone(&admission_repository)),
        redeem_pass: RedeemAdmissionPass::new(Arc::clone(&admission_repository)),
        revoke_pass: RevokeAdmissionPass::new(admission_repository),
        qr_signing_key: None,
        qr_ttl: Duration::from_secs(30),
        secure_cookies: false,
    });
    let fan_lifecycle = FanLifecycleState::new(
        workspace_id,
        ConfirmFan::new(Arc::clone(&lifecycle_repository)),
        UnsubscribeFan::new(lifecycle_repository),
        Url::parse("http://localhost:4321")?,
        false,
    );
    let ticketing = TicketingState::new(
        workspace_id,
        database.clone(),
        timeout,
        Some(Sha256::digest(ADMIN_KEY.as_bytes()).into()),
        Some(Sha256::digest(b"test-staff-api-key-123456789012".as_slice()).into()),
        None,
        None,
        None,
        None,
        Some([7_u8; 32]),
    );
    let ops = OpsState::new(workspace_id, database.clone(), timeout);
    let autopilot = PostgresAutopilotRepository::new_with_timeouts(database.clone(), timeout);
    Ok(AppState::new(
        database,
        timeout,
        acquisition,
        referrals,
        events,
        admission,
        ConcertQrState::new(workspace_id, pool.clone(), None),
        fan_lifecycle,
        ticketing,
        None,
        Some(Sha256::digest(b"test-control-plane-key-123456789012".as_slice()).into()),
        None,
        None,
        ops,
        autopilot,
        false,
        PushPublicState {
            runtime_enabled: false,
            web_push_vapid_public_key: None,
            fcm_project_id: None,
        },
        TenantProfile {
            slug: "test".to_owned(),
            display_name: "Test Act".to_owned(),
            palette: TenantPalette::default(),
            products: TenantProducts {
                crowdrelay: true,
                signal: true,
                synesthesia: false,
            },
            regional: TenantRegionalProfile {
                country_code: "US".to_owned(),
                region: "us".to_owned(),
                locale: "en-US".to_owned(),
                timezone: "America/New_York".to_owned(),
                currency: "USD".to_owned(),
                date_format: "mdy".to_owned(),
                number_format: "dot_decimal".to_owned(),
                data_region: Some("us".to_owned()),
            },
            regional_provenance: TenantRegionalProvenance {
                country_code: RegionalSource::TenantProfile,
                region: RegionalSource::TenantProfile,
                locale: RegionalSource::TenantProfile,
                timezone: RegionalSource::TenantProfile,
                currency: RegionalSource::TenantProfile,
                date_format: RegionalSource::TenantProfile,
                number_format: RegionalSource::TenantProfile,
                data_region: RegionalSource::TenantProfile,
            },
            parked: false,
        },
        crowdrelay_infra::sensitive_response::SensitiveResponseKey::derive_from_secret(
            b"test-encryption-key",
        ),
        AttestationSigningKey::derive_from_secret(SIGNING_SECRET),
        crowdrelay_infra::provider_verification::ProviderVerifiers::new(
            None,
            None,
            None,
            reqwest::Client::new(),
        ),
    ))
}

// ── The suite ────────────────────────────────────────────────────────────────

async fn seed_workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind("Anchor Test Workspace")
        .execute(pool)
        .await?;
    Ok(id)
}

/// A stored attestation row with a real signature. Issuing through the
/// repository would drag the whole ticket-history fixture in to satisfy the
/// measurement floor, and what is under test here is the anchor, not the
/// measurement — the insert is the forge path the infra suite already proves
/// the schema accepts.
async fn seed_attestation(
    pool: &PgPool,
    workspace_id: Uuid,
    digest: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let signature =
        PostgresAttestationRepository::new(pool.clone(), signing_key()).sign_digest(digest);
    sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_attestations
            (workspace_id, act_name, figures, issued_at, valid_until, digest, signature)
        VALUES ($1, 'Anchor Test Act', '[]'::jsonb, now() - interval '1 hour',
                now() + interval '30 days', $2, $3)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(digest)
    .bind(signature)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

fn signing_key() -> AttestationSigningKey {
    AttestationSigningKey::derive_from_secret(SIGNING_SECRET)
}

async fn get(
    app: &axum::Router,
    uri: &str,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty())?)
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)?
    };
    Ok((status, json))
}

async fn create_audit_batch(
    app: &axum::Router,
    idempotency_key: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/admin/proofs/audit-batches")
                .header(AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                .header("idempotency-key", idempotency_key)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"limit": 1024}).to_string()))?,
        )
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    assert_eq!(status, StatusCode::OK, "audit batch failed: {body:?}");
    Ok(serde_json::from_slice(&body)?)
}

/// The leaf hash the batch builder stores: SHA-256 over a 0x00 domain byte,
/// the canonical length as big-endian u64, and the canonical JSON text —
/// recomputed here so the test does not take the stored hash's word for it.
fn leaf_hash(canonical: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0_u8]);
    hasher.update((canonical.len() as u64).to_be_bytes());
    hasher.update(canonical);
    hasher.finalize().into()
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([1_u8]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_attestation_is_anchored_once_and_the_anchor_says_so()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_anchor_suite(&database).await
}

async fn run_anchor_suite(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_uuid = seed_workspace(pool).await?;
    let workspace_id = WorkspaceId::from_uuid(workspace_uuid);
    let app = crowdrelay_api::router(
        app_state(pool, workspace_id)?,
        HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );
    let repository = PostgresAttestationRepository::new(pool.clone(), signing_key());
    let digest = "a".repeat(64);
    let attestation_id = seed_attestation(pool, workspace_uuid, &digest).await?;

    // ── Before any batch: "not yet" is a stated state, unknown is a 404. ────
    let (status, body) = get(
        &app,
        &format!("/v1/public/attestations/digest/{digest}/anchor"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["anchored"], false,
        "unbatched attestation must not claim an anchor"
    );
    assert!(
        matches!(repository.anchor_for_digest(&digest).await, Ok(None)),
        "an unbatched attestation must answer Ok(None), not an error"
    );

    let (status, _) = get(
        &app,
        "/v1/public/attestations/digest/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee/anchor",
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a digest nothing carries must 404"
    );
    assert!(
        matches!(
            repository
                .anchor_for_digest(
                    "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                )
                .await,
            Err(AttestationError::NotFound)
        ),
        "a digest nothing carries must be NotFound, not Ok(None)"
    );

    let (status, _) = get(&app, "/v1/public/attestations/digest/not-a-digest/anchor").await?;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a malformed digest must 400"
    );
    let (status, _) = get(
        &app,
        "/v1/public/attestations/digest/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/anchor",
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "uppercase hex is not a stored digest"
    );

    // ── Build a batch: the attestation becomes a leaf. ──────────────────────
    let first = create_audit_batch(&app, "anchor-test-batch-one").await?;
    let batch = first["batch"].as_object().ok_or("first batch was empty")?;
    let batch_id = batch["id"].as_str().ok_or("batch had no id")?;

    let (stored_kind, stored_leaf): (String, Vec<u8>) = sqlx::query_as(
        r#"
        SELECT source_kind, leaf_sha256
        FROM external_proof_items
        WHERE workspace_id = $1 AND source_id = $2
        "#,
    )
    .bind(workspace_uuid)
    .bind(attestation_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(stored_kind, "attestation");

    // The stored leaf must be derivable: the same canonical form the CTE
    // builds, hashed the same way.
    let canonical: String = sqlx::query_scalar(
        r#"
        SELECT jsonb_build_array(
            'crowdrelay/attestation/v1', att.id, att.digest, att.issued_at
        )::text
        FROM viryaos_attestations AS att
        WHERE att.id = $1
        "#,
    )
    .bind(attestation_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        hex::encode(leaf_hash(canonical.as_bytes())),
        hex::encode(&stored_leaf),
        "the stored leaf is not the documented canonical form"
    );

    // anchor_for_digest now resolves the batch and root.
    let anchor = repository
        .anchor_for_digest(&digest)
        .await?
        .ok_or("a batched attestation answered Ok(None)")?;
    assert_eq!(anchor.attestation_id, attestation_id);
    assert_eq!(anchor.batch_id.to_string(), batch_id);
    assert_eq!(anchor.leaf_sha256, hex::encode(&stored_leaf));
    assert_eq!(anchor.batch_status, "queued");

    // ── The public anchor route reports the same fact. ──────────────────────
    let (status, body) = get(
        &app,
        &format!("/v1/public/attestations/digest/{digest}/anchor"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["anchored"], true);
    assert_eq!(body["anchor"]["batch_id"].as_str(), Some(batch_id));
    assert_eq!(
        body["anchor"]["attestation_id"].as_str(),
        Some(attestation_id.to_string().as_str())
    );
    let proof_path = body["inclusion_proof_path"]
        .as_str()
        .ok_or("anchored response carried no inclusion_proof_path")?;
    assert_eq!(
        proof_path,
        format!("/v1/public/proofs/batches/{batch_id}/attestation/{attestation_id}")
    );

    // ── The path it hands out serves a Merkle proof that verifies to root. ──
    let (status, proof) = get(&app, proof_path).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "inclusion route rejected the anchor's path"
    );
    assert_eq!(proof["verified"], true);
    assert_eq!(
        proof["leaf_sha256"].as_str(),
        Some(anchor.leaf_sha256.as_str())
    );

    // Verify independently rather than trusting the `verified` flag: fold the
    // returned steps up from the leaf and land on the batch's root.
    let mut current: [u8; 32] = hex::decode(&anchor.leaf_sha256)?
        .try_into()
        .map_err(|_| "leaf was not 32 bytes")?;
    for step in proof["proof"].as_array().ok_or("proof had no steps")? {
        let sibling: [u8; 32] = hex::decode(step["sha256"].as_str().ok_or("step had no sha256")?)?
            .try_into()
            .map_err(|_| "step was not 32 bytes")?;
        current = if step["side"].as_str() == Some("left") {
            node_hash(&sibling, &current)
        } else {
            node_hash(&current, &sibling)
        };
    }
    assert_eq!(
        hex::encode(current),
        anchor.root_sha256,
        "the Merkle path does not fold up to the batch root"
    );

    // ── A second batch must not take the same leaf twice. ───────────────────
    let second = create_audit_batch(&app, "anchor-test-batch-two").await?;
    assert!(
        second["batch"].is_null(),
        "a second batch found new candidates — the attestation was anchored twice: {second}"
    );
    let item_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM external_proof_items
         WHERE workspace_id = $1 AND source_kind = 'attestation' AND source_id = $2",
    )
    .bind(workspace_uuid)
    .bind(attestation_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(item_count, 1, "the attestation was included more than once");

    Ok(())
}
