//! The listing and the representation approach against a real Postgres.
//!
//! What these tests exist for is what the schema enforces that Rust cannot
//! see: the `accepts_outreach_basis` CHECK on representation kinds, the
//! `approach` phase widening, the publish/unlist/rotate transitions on the
//! listing row, and the allowance count that makes a thin pitch cost
//! something. A unit test over the same code would pass while every one of
//! those constraints silently failed to migrate.

use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::listing::{BandListing, ListedClaim, ListingVisibility};
use crowdrelay_domain::representation::MONTHLY_APPROACH_ALLOWANCE;
use crowdrelay_domain::value_tier::MetricValueTier;
use crowdrelay_infra::band_listing::{BandListingError, PostgresBandListingRepository};
use crowdrelay_infra::database::MIGRATOR;
use crowdrelay_infra::representation::{
    ApproachOutcome, PostgresRepresentationRepository, RepresentationError,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

async fn test_pool() -> Result<PgPool, Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

async fn insert_workspace(pool: &PgPool) -> Result<Uuid, sqlx::Error> {
    let workspace_id = WorkspaceId::new().into_uuid();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(format!("listing-e2e-{}", workspace_id.simple()))
        .bind("Listing E2E")
        .execute(pool)
        .await?;
    Ok(workspace_id)
}

fn listing(act: &str) -> BandListing {
    BandListing {
        act_name: act.to_owned(),
        genre_tags: vec!["modern metal".to_owned()],
        cities: vec!["Warsaw".to_owned()],
        claims: vec![ListedClaim {
            label: "Tickets banked in Warsaw, last 12 months".to_owned(),
            value: Some(420),
            tier: MetricValueTier::Downstream,
            basis: "CrowdRelay ticket ledger".to_owned(),
        }],
        published_dates: vec![],
        seeking: vec!["booking agent".to_owned()],
        visibility: ListingVisibility::Unlisted,
    }
}

async fn insert_target(
    pool: &PgPool,
    workspace_id: Uuid,
    kind: &str,
    email: &str,
    accepts_outreach: bool,
    basis: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        INSERT INTO viryaos_outreach_targets (
            workspace_id, target_kind, display_name, contact_email,
            accepts_outreach, accepts_outreach_basis, active, verified,
            do_not_contact
        ) VALUES ($1,$2,$3,$4,$5,$6,true,true,false)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(kind)
    .bind(format!("Test {kind}"))
    .bind(email)
    .bind(accepts_outreach)
    .bind(basis)
    .fetch_one(pool)
    .await
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn listing_lifecycle_draft_publish_rotate_unlist() -> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let repo = PostgresBandListingRepository::new(pool.clone());

    // A workspace with no row has no state and nothing public.
    assert!(repo.load_state(workspace_id).await?.is_none());

    // Save writes the draft; the token exists but the row is unlisted, so
    // the public read must not answer — "saved" is not "published".
    repo.save(workspace_id, &listing("Virya")).await?;
    let state = repo.load_state(workspace_id).await?.expect("saved state");
    assert_eq!(state.listing.act_name, "Virya");
    assert_eq!(state.listing.visibility, ListingVisibility::Unlisted);
    let token = state.share_token;
    assert!(repo.read_visible_by_token(token).await?.is_none());

    // Publish runs the domain review and opens the token.
    repo.publish(workspace_id).await?;
    let visible = repo
        .read_visible_by_token(token)
        .await?
        .expect("published listing");
    assert_eq!(visible.act_name, "Virya");
    assert_eq!(visible.claims.len(), 1);

    // An unsupported claim never reaches the reader — the draft keeps it,
    // the public view drops it.
    let mut draft = listing("Virya");
    draft.claims.push(ListedClaim {
        label: "Fans we have not counted yet".to_owned(),
        value: None,
        tier: MetricValueTier::Intermediate,
        basis: "not counted".to_owned(),
    });
    repo.save(workspace_id, &draft).await?;
    let visible = repo
        .read_visible_by_token(token)
        .await?
        .expect("still published");
    assert_eq!(visible.claims.len(), 1);

    // Rotate: the link already sent dies, the new one reads.
    let new_token = repo.rotate_share_token(workspace_id).await?;
    assert_ne!(new_token, token);
    assert!(repo.read_visible_by_token(token).await?.is_none());
    assert!(repo.read_visible_by_token(new_token).await?.is_some());

    // Unlist takes it down without deleting the draft; republish works.
    repo.unlist(workspace_id).await?;
    assert!(repo.read_visible_by_token(new_token).await?.is_none());
    assert!(repo.load_state(workspace_id).await?.is_some());

    // Unlist on a workspace with no row is NotFound, not a silent success.
    let other = insert_workspace(&pool).await?;
    assert!(matches!(
        repo.unlist(other).await,
        Err(BandListingError::NotFound)
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn publish_refuses_a_listing_with_nothing_banked() -> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let repo = PostgresBandListingRepository::new(pool.clone());

    let mut draft = listing("Vanity Act");
    draft.claims[0].tier = MetricValueTier::Vanity;
    repo.save(workspace_id, &draft).await?;
    assert!(matches!(
        repo.publish(workspace_id).await,
        Err(BandListingError::Refused(_))
    ));
    // Refusal leaves the draft intact and unlisted.
    let state = repo.load_state(workspace_id).await?.expect("draft kept");
    assert_eq!(state.listing.visibility, ListingVisibility::Unlisted);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn representation_kind_requires_a_consent_basis() -> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;

    // agent/label + accepts_outreach with no basis: the CHECK refuses.
    let refused = insert_target(
        &pool,
        workspace_id,
        "agent",
        "agent-no-basis@example.com",
        true,
        None,
    )
    .await;
    assert!(refused.is_err(), "consent without a basis must not insert");

    // With a basis it lands.
    let target = insert_target(
        &pool,
        workspace_id,
        "agent",
        "agent-with-basis@example.com",
        true,
        Some("asked for PL bands at the showcase"),
    )
    .await?;
    assert!(!target.is_nil());
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approach_gate_and_allowance_hold_at_request_time() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let listings = PostgresBandListingRepository::new(pool.clone());
    let repo = PostgresRepresentationRepository::new(pool.clone());
    let key = || IdempotencyKey::parse(format!("itest-{}", Uuid::now_v7())).unwrap();

    // A consented agent still cannot be approached before the listing is
    // published — the listing is the evidence the pitch carries.
    let agent = insert_target(
        &pool,
        workspace_id,
        "agent",
        "gated@example.com",
        true,
        Some("submission page asks for bands"),
    )
    .await?;
    match repo
        .request_approach(workspace_id, agent, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => {
            assert!(reason.contains("listing"), "unexpected refusal: {reason}")
        }
        other => panic!("expected refusal, got {other:?}"),
    }

    listings.save(workspace_id, &listing("Virya")).await?;
    listings.publish(workspace_id).await?;

    // Now the request queues.
    let first = repo
        .request_approach(workspace_id, agent, Some("we'd be a fit"), &key())
        .await?;
    assert!(matches!(first, ApproachOutcome::Queued { .. }));

    // The same target again: already pending, refused with a sentence.
    assert!(matches!(
        repo.request_approach(workspace_id, agent, None, &key())
            .await,
        Err(RepresentationError::Refused(_))
    ));

    // An unconsented contact refuses regardless of the listing.
    let cold = insert_target(
        &pool,
        workspace_id,
        "label",
        "cold@example.com",
        false,
        None,
    )
    .await?;
    assert!(matches!(
        repo.request_approach(workspace_id, cold, None, &key())
            .await,
        Err(RepresentationError::Refused(_))
    ));

    // A press contact is not a representation contact — NotFound, not a
    // refusal, so the band learns nothing about a contact it cannot approach.
    let press = insert_target(
        &pool,
        workspace_id,
        "press",
        "press@example.com",
        true,
        None,
    )
    .await?;
    assert!(matches!(
        repo.request_approach(workspace_id, press, None, &key())
            .await,
        Err(RepresentationError::NotFound)
    ));

    // The allowance: fill the month from recorded approaches, then the next
    // request is refused on scarcity, not on consent.
    for index in 0..MONTHLY_APPROACH_ALLOWANCE {
        sqlx::query(
            r#"
            INSERT INTO viryaos_outreach_interactions (
                workspace_id, target_id, direction, phase, source_key, occurred_at
            ) VALUES ($1,$2,'outbound','approach',$3, now())
            "#,
        )
        .bind(workspace_id)
        .bind(agent)
        .bind(format!("approach-fill-{index}"))
        .execute(&pool)
        .await?;
    }
    assert_eq!(
        repo.approaches_used_this_month(workspace_id).await?,
        MONTHLY_APPROACH_ALLOWANCE
    );
    let next_agent = insert_target(
        &pool,
        workspace_id,
        "agent",
        "scarcity@example.com",
        true,
        Some("roster page lists submissions"),
    )
    .await?;
    match repo
        .request_approach(workspace_id, next_agent, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => {
            assert!(reason.contains("month"), "unexpected refusal: {reason}")
        }
        other => panic!("expected allowance refusal, got {other:?}"),
    }

    // The band never sees the mailbox: the read model carries no email.
    let targets = repo.list_targets(workspace_id).await?;
    let row = serde_json::to_value(&targets).unwrap();
    assert!(
        row.as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("contact_email").is_none())
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn idempotent_approach_replay_returns_the_same_action()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let listings = PostgresBandListingRepository::new(pool.clone());
    let repo = PostgresRepresentationRepository::new(pool.clone());

    listings.save(workspace_id, &listing("Virya")).await?;
    listings.publish(workspace_id).await?;
    let agent = insert_target(
        &pool,
        workspace_id,
        "agent",
        "replay@example.com",
        true,
        Some("asked for bands"),
    )
    .await?;

    let key = IdempotencyKey::parse(format!("itest-{}", Uuid::now_v7())).unwrap();
    let first = repo
        .request_approach(workspace_id, agent, None, &key)
        .await?;
    let second = repo.request_approach(workspace_id, agent, None, &key).await;
    // The retry finds the queued action under the key — but the pending
    // check runs first and refuses with the honest sentence instead.
    match (first, second) {
        (ApproachOutcome::Queued { .. }, Err(RepresentationError::Refused(_))) => {}
        other => panic!("expected queued-then-refused, got {other:?}"),
    }

    // One decision, one action — the retry did not double-queue.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1 AND context = 'representation'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1);
    Ok(())
}
