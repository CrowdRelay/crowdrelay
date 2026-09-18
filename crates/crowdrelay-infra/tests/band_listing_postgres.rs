//! The listing and the representation approach against a real Postgres.
//!
//! What these tests exist for is what the schema enforces that Rust cannot
//! see: the `accepts_outreach_basis` CHECK on representation kinds, the
//! `approach` phase widening, the publish/unlist/rotate transitions on the
//! listing row, and the allowance count that makes a thin pitch cost
//! something. A unit test over the same code would pass while every one of
//! those constraints silently failed to migrate.

use std::time::Duration;

use crowdrelay_application::autopilot::{AutopilotOutreachStateRepository, RecordOutreachReply};
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_domain::listing::{BandListing, ListedClaim, ListingVisibility};
use crowdrelay_domain::outreach::OutreachReplyDisposition;
use crowdrelay_domain::representation::MONTHLY_APPROACH_ALLOWANCE;
use crowdrelay_domain::value_tier::MetricValueTier;
use crowdrelay_domain::{OutreachTargetId, WorkspaceId};
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
use crowdrelay_infra::band_listing::{BandListingError, PostgresBandListingRepository};
use crowdrelay_infra::config::DatabaseConfig;
use crowdrelay_infra::database::MIGRATOR;
use crowdrelay_infra::gdrive::{DriveContactRow, PostgresGDriveRepository};
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

/// The draw ledger an agent pitch cites: one completed show with one paid
/// buyer and one interested fan — the minimum real evidence the gate asks
/// for. A workspace without it has nothing to pitch an agent on.
async fn seed_draw(pool: &PgPool, workspace_id: Uuid) -> Result<(), sqlx::Error> {
    let city_slug = format!("testowice-{}", Uuid::now_v7().simple());
    sqlx::query("INSERT INTO cities (slug, name, country_code) VALUES ($1, 'Testowice', 'PL')")
        .bind(&city_slug)
        .execute(pool)
        .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = $1",
    )
    .bind(&city_slug)
    .fetch_one(pool)
    .await?;
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events \
            (id, workspace_id, city_id, slug, title, venue, starts_at, status, published_at) \
         VALUES ($1, $2, $3, $4, 'Draw Show', 'Klub Testowice', \
                 now() - interval '10 days', 'completed', now() - interval '40 days')",
    )
    .bind(event_id)
    .bind(workspace_id)
    .bind(city_id)
    .bind(format!("draw-show-{}", Uuid::now_v7().simple()))
    .execute(pool)
    .await?;
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) \
         VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace_id)
    .bind(format!("draw-fan-{}@example.test", Uuid::now_v7().simple()))
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO event_interests (workspace_id, event_id, fan_id) VALUES ($1,$2,$3)")
        .bind(workspace_id)
        .bind(event_id)
        .bind(fan_id)
        .execute(pool)
        .await?;
    let pool_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO admission_pools (id, workspace_id, event_id, slug, name, capacity) \
         VALUES ($1,$2,$3,$4,'ga',100)",
    )
    .bind(pool_id)
    .bind(workspace_id)
    .bind(event_id)
    .bind(format!("ga-{}", Uuid::now_v7().simple()))
    .execute(pool)
    .await?;
    let sale_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO ticket_sales \
            (id, workspace_id, event_id, admission_pool_id, capacity, \
             sales_open_at, sales_close_at) \
         VALUES ($1,$2,$3,$4,100, now() - interval '30 days', now() - interval '9 days')",
    )
    .bind(sale_id)
    .bind(workspace_id)
    .bind(event_id)
    .bind(pool_id)
    .execute(pool)
    .await?;
    let suffix = Uuid::now_v7().simple().to_string();
    sqlx::query(
        r#"
        INSERT INTO ticket_orders (
            workspace_id, ticket_sale_id, public_reference, status, buyer_email,
            currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
            vat_rate_basis_points, reservation_key, request_hash, checkout_token_hash,
            expires_at, paid_at
        ) VALUES (
            $1,$2,$3,'paid',$4,'PLN',10800,10000,800,800,$5,
            sha256($6::bytea), sha256($7::bytea), now() + interval '1 day',
            now() - interval '12 days'
        )
        "#,
    )
    .bind(workspace_id)
    .bind(sale_id)
    .bind(format!("VRY-ORD-{}", suffix[..16].to_uppercase()))
    .bind(format!("buyer-{}@example.test", &suffix[..8]))
    .bind(format!("reservation-{suffix}"))
    .bind(format!("request-{suffix}").into_bytes())
    .bind(format!("checkout-{suffix}").into_bytes())
    .execute(pool)
    .await?;
    Ok(())
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
    seed_draw(&pool, workspace_id).await?;

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

/// Approaches waiting on approval spend the month's allowance before any
/// send happens — otherwise the band could queue more than can ever send
/// and the surplus would die in the queue as failed actions.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn pending_approaches_spend_the_monthly_allowance() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let listings = PostgresBandListingRepository::new(pool.clone());
    let repo = PostgresRepresentationRepository::new(pool.clone());
    let key = || IdempotencyKey::parse(format!("itest-{}", Uuid::now_v7())).unwrap();
    listings.save(workspace_id, &listing("Virya")).await?;
    listings.publish(workspace_id).await?;
    seed_draw(&pool, workspace_id).await?;

    for index in 0..MONTHLY_APPROACH_ALLOWANCE {
        let agent = insert_target(
            &pool,
            workspace_id,
            "agent",
            &format!("pending{index}@example.com"),
            true,
            Some("asked for bands"),
        )
        .await?;
        match repo
            .request_approach(workspace_id, agent, None, &key())
            .await?
        {
            ApproachOutcome::Queued { .. } => {}
            other => panic!("expected queued, got {other:?}"),
        }
    }

    // Zero approaches have sent — every one waits on approval — and the
    // allowance is still spent.
    let spare = insert_target(
        &pool,
        workspace_id,
        "agent",
        "spare@example.com",
        true,
        Some("asked for bands"),
    )
    .await?;
    match repo
        .request_approach(workspace_id, spare, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => {
            assert!(reason.contains("month"), "unexpected refusal: {reason}")
        }
        other => panic!("expected allowance refusal, got {other:?}"),
    }
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
    seed_draw(&pool, workspace_id).await?;
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
    // The retry finds the queued action under the key before any gate runs
    // — a resubmitted form gets the action it already queued, not a refusal.
    match (first, second) {
        (
            ApproachOutcome::Queued {
                action_id: first_id,
            },
            Ok(ApproachOutcome::Replayed { action_id, status }),
        ) => {
            assert_eq!(action_id, first_id);
            assert_eq!(status, "awaiting_approval");
        }
        other => panic!("expected queued-then-replayed, got {other:?}"),
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

/// §4h-10: an agent pitch is an application — one knock per season, a closed
/// door until `refused_until`, and no approach without real draw behind it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_agent_approach_rides_the_season_and_the_draw() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let listings = PostgresBandListingRepository::new(pool.clone());
    let repo = PostgresRepresentationRepository::new(pool.clone());
    let key = || IdempotencyKey::parse(format!("itest-{}", Uuid::now_v7())).unwrap();
    listings.save(workspace_id, &listing("Virya")).await?;
    listings.publish(workspace_id).await?;

    // No draw on the books — the application is refused with the honest
    // reason, and nothing queues.
    let thin = insert_target(
        &pool,
        workspace_id,
        "agent",
        "thin@example.com",
        true,
        Some("roster page lists submissions"),
    )
    .await?;
    match repo
        .request_approach(workspace_id, thin, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => assert!(
            reason.contains("numbers") || reason.contains("paid tickets"),
            "unexpected refusal: {reason}"
        ),
        other => panic!("expected a draw refusal, got {other:?}"),
    }
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(queued, 0, "a thin pitch queued an approach anyway");

    seed_draw(&pool, workspace_id).await?;

    // A label faces no draw gate — the listing is its pitch.
    let label = insert_target(
        &pool,
        workspace_id,
        "label",
        "label@example.com",
        true,
        Some("demo inbox is open"),
    )
    .await?;
    assert!(matches!(
        repo.request_approach(workspace_id, label, None, &key())
            .await?,
        ApproachOutcome::Queued { .. }
    ));

    // A registry row at the same address is the same agent — the door and
    // the season bind by contact, not by which table listed them.
    let email = format!("agent-{}@example.test", Uuid::now_v7().simple());
    let registry_agent = insert_target(
        &pool,
        workspace_id,
        "agent",
        &email,
        true,
        Some("met after the Testowice show"),
    )
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_booking_agents (workspace_id, name, contact_email) \
         VALUES ($1, 'Registry Agent', $2)",
    )
    .bind(workspace_id)
    .bind(&email)
    .execute(&pool)
    .await?;
    assert!(
        matches!(
            repo.request_approach(workspace_id, registry_agent, None, &key())
                .await?,
            ApproachOutcome::Queued { .. }
        ),
        "an open door and real draw must queue"
    );

    // The pending approach for that agent counts against a second request —
    // but the season door is its own gate, so retire the queue row and knock
    // again as if the first send had landed: a ledger `approach` row inside
    // the season window. `cancelled`, not `DELETE` — the action ledger is
    // append-only and a delete cascades into a wall.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'cancelled', finished_at = now() \
         WHERE workspace_id = $1 AND subject_id = $2",
    )
    .bind(workspace_id)
    .bind(registry_agent)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_outreach_interactions \
            (workspace_id, target_id, direction, phase, source_key, occurred_at) \
         VALUES ($1,$2,'outbound','approach',$3, now() - interval '30 days')",
    )
    .bind(workspace_id)
    .bind(registry_agent)
    .bind(format!("season-knock-{}", Uuid::now_v7().simple()))
    .execute(&pool)
    .await?;
    match repo
        .request_approach(workspace_id, registry_agent, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => {
            assert!(reason.contains("season"), "unexpected refusal: {reason}")
        }
        other => panic!("expected a season refusal, got {other:?}"),
    }

    // The registry's own approached_at binds identically — a knock recorded
    // on the booking graph without an outreach ledger row is still a knock.
    let past_email = format!("agent-{}@example.test", Uuid::now_v7().simple());
    let past_agent = insert_target(
        &pool,
        workspace_id,
        "agent",
        &past_email,
        true,
        Some("met after the Testowice show"),
    )
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_booking_agents \
            (workspace_id, name, contact_email, approached_at) \
         VALUES ($1, 'Recent Agent', $2, now() - interval '60 days')",
    )
    .bind(workspace_id)
    .bind(&past_email)
    .execute(&pool)
    .await?;
    match repo
        .request_approach(workspace_id, past_agent, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => {
            assert!(reason.contains("season"), "unexpected refusal: {reason}")
        }
        other => panic!("expected a season refusal, got {other:?}"),
    }

    // An old approach is outside the window — the season reopened.
    sqlx::query(
        "UPDATE viryaos_booking_agents SET approached_at = now() - interval '200 days' \
         WHERE workspace_id = $1 AND contact_email = $2",
    )
    .bind(workspace_id)
    .bind(&past_email)
    .execute(&pool)
    .await?;
    assert!(
        matches!(
            repo.request_approach(workspace_id, past_agent, None, &key())
                .await?,
            ApproachOutcome::Queued { .. }
        ),
        "a knock older than the season must not still bind"
    );

    // A refusal on the registry row closes the door outright.
    let refused_email = format!("agent-{}@example.test", Uuid::now_v7().simple());
    let refused_agent = insert_target(
        &pool,
        workspace_id,
        "agent",
        &refused_email,
        true,
        Some("met after the Testowice show"),
    )
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_booking_agents \
            (workspace_id, name, contact_email, refused_until) \
         VALUES ($1, 'Refused Agent', $2, CURRENT_DATE + 60)",
    )
    .bind(workspace_id)
    .bind(&refused_email)
    .execute(&pool)
    .await?;
    match repo
        .request_approach(workspace_id, refused_agent, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => assert!(
            reason.contains("door") || reason.contains("declined"),
            "unexpected refusal: {reason}"
        ),
        other => panic!("expected a closed-door refusal, got {other:?}"),
    }

    // A declined reply on an agent target writes the door on the registry.
    let decline_email = format!("agent-{}@example.test", Uuid::now_v7().simple());
    let decline_agent = insert_target(
        &pool,
        workspace_id,
        "agent",
        &decline_email,
        true,
        Some("met after the Testowice show"),
    )
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_booking_agents (workspace_id, name, contact_email) \
         VALUES ($1, 'Declining Agent', $2)",
    )
    .bind(workspace_id)
    .bind(&decline_email)
    .execute(&pool)
    .await?;
    let autopilot = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    autopilot
        .record_outreach_reply(
            WorkspaceId::from_uuid(workspace_id),
            RecordOutreachReply {
                target_id: OutreachTargetId::from_uuid(decline_agent),
                opportunity_id: None,
                disposition: OutreachReplyDisposition::Declined,
                reply_text: None,
                occurred_at: time::OffsetDateTime::now_utc(),
            },
            &IdempotencyKey::parse(format!("itest-{}", Uuid::now_v7()))?,
            None::<&RequestId>,
        )
        .await?;
    let days: i32 = sqlx::query_scalar(
        "SELECT (refused_until - CURRENT_DATE)::int FROM viryaos_booking_agents \
         WHERE workspace_id = $1 AND contact_email = $2",
    )
    .bind(workspace_id)
    .bind(&decline_email)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        days,
        crowdrelay_domain::representation::AGENT_APPROACH_SEASON_DAYS as i32,
        "a decline must close the door for exactly one season"
    );
    match repo
        .request_approach(workspace_id, decline_agent, None, &key())
        .await
    {
        Err(RepresentationError::Refused(reason)) => assert!(
            reason.contains("door") || reason.contains("declined"),
            "unexpected refusal: {reason}"
        ),
        other => panic!("expected a closed-door refusal, got {other:?}"),
    }

    // The door state is visible where the band decides — the read carries
    // approached_at / refused_until off the registry row.
    let targets = repo.list_targets(workspace_id).await?;
    let refused = targets
        .iter()
        .find(|t| t.target_id == refused_agent)
        .expect("the refused agent lists");
    assert!(refused.refused_until.is_some(), "the door did not surface");
    let past = targets
        .iter()
        .find(|t| t.target_id == past_agent)
        .expect("the approached agent lists");
    assert!(
        past.approached_at.is_some(),
        "the season stamp did not surface"
    );
    Ok(())
}

/// A beacon agent promotes onto the registry AND onto the approach list —
/// one filing makes the agent both recorded and reachable.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_beacon_agent_lands_on_the_registry_and_the_approach_list()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let workspace_id = insert_workspace(&pool).await?;
    let gdrive = PostgresGDriveRepository::new(pool.clone());
    let email = format!("beacon-agent-{}@example.test", Uuid::now_v7().simple());
    let contact = DriveContactRow {
        id: Uuid::now_v7(),
        normalized_email: email.clone(),
        display_name: Some("Beacon Agent".to_owned()),
        organization: Some("Agency X".to_owned()),
        phone: None,
        suggested_kind: Some("booking_agent".to_owned()),
        city: None,
        notes: None,
        source_file_id: "file-1".to_owned(),
        source_file_name: "contacts.csv".to_owned(),
        sources: vec![],
        last_seen_at: time::OffsetDateTime::now_utc(),
        disappeared_at: None,
        fan_outcome: "none".to_owned(),
        beacon_outcome: "staged".to_owned(),
        matched_venue: None,
        venue_played_here: false,
        matched_counterparty: None,
        counterparty_worked_with: false,
    };
    // The promote marks the drive contact promoted — the staging row must
    // exist for that update to land.
    sqlx::query(
        "INSERT INTO viryaos_drive_contacts \
            (id, workspace_id, normalized_email, suggested_kind, source_file_id, \
             source_file_name, fan_outcome, beacon_outcome) \
         VALUES ($1,$2,$3,'booking_agent','file-1','contacts.csv','staged','staged')",
    )
    .bind(contact.id)
    .bind(workspace_id)
    .bind(&email)
    .execute(&pool)
    .await?;
    gdrive.promote_beacon_agent(workspace_id, &contact).await?;

    let (registry, list): (i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM viryaos_booking_agents \
              WHERE workspace_id = $1 AND contact_email = $2), \
            (SELECT count(*) FROM viryaos_outreach_targets \
              WHERE workspace_id = $1 AND contact_email = $2 AND target_kind = 'agent')",
    )
    .bind(workspace_id)
    .bind(&email)
    .fetch_one(&pool)
    .await?;
    assert_eq!((registry, list), (1, 1), "the promote must file both rows");

    // A re-import refreshes rather than duplicating — and never reopens a
    // door the agent closed.
    sqlx::query(
        "UPDATE viryaos_booking_agents SET refused_until = CURRENT_DATE + 90 \
         WHERE workspace_id = $1 AND contact_email = $2",
    )
    .bind(workspace_id)
    .bind(&email)
    .execute(&pool)
    .await?;
    gdrive.promote_beacon_agent(workspace_id, &contact).await?;
    let kept: Option<time::Date> = sqlx::query_scalar(
        "SELECT refused_until FROM viryaos_booking_agents \
         WHERE workspace_id = $1 AND contact_email = $2",
    )
    .bind(workspace_id)
    .bind(&email)
    .fetch_one(&pool)
    .await?;
    assert!(kept.is_some(), "a re-import erased a closed door");
    Ok(())
}
