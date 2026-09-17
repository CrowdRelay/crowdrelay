//! 4G.4 — approving a gig proposal writes one letter to everybody who books
//! the room, or writes none of it.
//!
//! The property worth a database test is the one a unit test cannot reach: the
//! contact governor runs inside the dispatch transaction, so a promoter who is
//! blocked has to roll back the whole outreach rather than leaving the band
//! having written to two of three people about one night. Those three book the
//! same city and talk to each other, and a partial send is worse than none.
//!
//! Also driven here because SQLx checks nothing at compile time: the approval
//! writes a decision and an action by hand, and a column that does not exist
//! would be a production failure with no warning anywhere earlier.

use std::time::Duration;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::AutopilotActionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::gig_outreach::{GigOutreachError, GigOutreachOutcome, approve_gig_proposal};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    url: String,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_gigout_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            url,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
            ..
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn one_approval_writes_to_the_whole_room_or_to_nobody()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool, &database.url).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    for index in 0..60 {
        reachable_fan(pool, act, wroclaw, &format!("fan{index}@example.com")).await?;
    }
    played_show(pool, act, wroclaw, "Klub X", "show-1", 40).await?;
    // Two promoters who book the same room. The whole point of the recipient
    // set: they talk to each other.
    promoter(pool, act, wroclaw, "Anna", "anna@example.com", 70).await?;
    promoter(pool, act, wroclaw, "Bogdan", "bogdan@example.com", 60).await?;

    // ── The approval ────────────────────────────────────────────────────────
    let key = IdempotencyKey::parse("gig-approve-1").expect("valid key");
    let outcome = approve_gig_proposal(pool, act, "wroclaw", &key, now).await?;
    let (action_id, recipients, opening_line) = match outcome {
        GigOutreachOutcome::Queued {
            action_id,
            recipients,
            opening_line,
            ..
        } => (action_id, recipients, opening_line),
        other => return Err(format!("expected a queued outreach, got {other:?}").into()),
    };
    assert_eq!(
        recipients.len(),
        2,
        "both promoters who book the room must be on one letter, got {recipients:?}"
    );
    assert!(
        !opening_line.is_empty() && opening_line.ends_with('.'),
        "the letter opens with the proposal's reason: {opening_line:?}"
    );

    // One action, not one per promoter. This is the §12-6 rule, and it is the
    // difference between everybody hearing and two of three hearing.
    let (kind, status, payload) = sqlx::query_as::<_, (String, String, serde_json::Value)>(
        "SELECT action_kind, status, payload FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(kind, "gig.outreach.request");
    // Approving the proposal is the approval. A second approve screen is how a
    // person stops reading approvals.
    assert_eq!(status, "queued");
    assert_eq!(
        payload["recipients"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default(),
        2
    );
    assert!(
        !payload["reasons"]
            .as_array()
            .expect("reasons travel with the action")
            .is_empty(),
        "the reasons the band approved on did not reach the letter"
    );
    let total_actions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(total_actions, 1, "one room, one action");

    // Every action carries a decision, or the trace cannot explain the send.
    let decision_kind = sqlx::query_scalar::<_, String>(
        "SELECT decision.decision_kind FROM viryaos_autopilot_actions AS action
         JOIN viryaos_autopilot_decisions AS decision ON decision.id = action.decision_id
         WHERE action.workspace_id = $1 AND action.id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(decision_kind, "gig.proposal.approved");

    // ── The same click twice ────────────────────────────────────────────────
    let replay = approve_gig_proposal(pool, act, "wroclaw", &key, now).await?;
    match replay {
        GigOutreachOutcome::Replayed {
            action_id: replayed,
            ..
        } => assert_eq!(replayed, action_id, "a retried click made a second letter"),
        other => return Err(format!("expected a replay, got {other:?}").into()),
    }

    // ── One blocked promoter stops the whole letter ─────────────────────────
    // Bogdan is under a do-not-contact. The governor refuses him, and the
    // transaction takes Anna's reservation and the outbox row down with it.
    block_contact(pool, act, "bogdan@example.com", now).await?;
    // A registry that advertises something else. Gating only applies once an
    // executor exists at all — with an empty registry nothing is gated, which
    // is deliberate and is why this seeds one before asserting the park.
    advertise(pool, act, "booking.outreach", now).await?;
    let parked = repository(pool, url)
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(act), 8, now)
        .await?;
    assert!(
        !parked
            .iter()
            .any(|candidate| candidate.id.into_uuid() == action_id),
        "the outreach was claimed with no executor advertising gig.outreach"
    );
    advertise(pool, act, "gig.outreach", now).await?;
    // A parked action returns to the queue five minutes later rather than
    // burning an attempt, so the claim that follows the switch-on has to be
    // past that window — the park is a delay, not a rejection.
    let after_park = now + time::Duration::minutes(6);
    let repository = repository(pool, url);
    let claimed = repository
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(act), 8, after_park)
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .ok_or("the queued outreach was not claimable")?;
    let executed = repository
        .execute_action(WorkspaceId::from_uuid(act), action, after_park)
        .await;
    assert!(
        executed.is_err(),
        "a blocked promoter did not stop the outreach"
    );
    let reserved = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_contact_governor
         WHERE workspace_id = $1 AND last_action_id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        reserved, 0,
        "the promoter the governor admitted kept a reservation from a letter nobody received"
    );
    let emitted = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_action_emissions
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(emitted, 0, "a partial letter reached the outbox");

    // ── A city that no longer proposes refuses rather than sending ──────────
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, 'already-booked', 'Booked', 'Klub X',
                 now() + interval '30 days', 'published', now())",
    )
    .bind(act)
    .bind(wroclaw)
    .execute(pool)
    .await?;
    let second_key = IdempotencyKey::parse("gig-approve-2").expect("valid key");
    match approve_gig_proposal(pool, act, "wroclaw", &second_key, now).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("already a show"),
            "the refusal did not say what changed: {sentence}"
        ),
        other => {
            return Err(format!("a booked city was approved anyway: {other:?}").into());
        }
    }

    // A city nobody has an audience in is not on the board at all.
    match approve_gig_proposal(pool, act, "lisboa", &second_key, now).await {
        Err(GigOutreachError::NotFound) => {}
        other => return Err(format!("an unknown city was not refused: {other:?}").into()),
    }

    Ok(())
}

/// 4G.5 — a settled proposal has its reasons scored.
///
/// The whole reason `Reason` is structured is that "which kind of evidence
/// predicts a booking" is a question the system can answer about itself. This
/// drives the chain the measurements travel: approve → the letter goes out →
/// a promoter answers inside the window → a show lands on the calendar — and
/// then asks the tally whether it saw what happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_settled_proposal_has_its_reasons_scored() -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::gig_planning::proposal_track_record;

    let database = DisposableDatabase::create().await?;
    let pool = &database.pool;
    let result = async {
        let now = OffsetDateTime::now_utc();
        let act = workspace(pool).await?;
        let wroclaw = city(pool, "wroclaw").await?;
        for index in 0..60 {
            reachable_fan(pool, act, wroclaw, &format!("fan{index}@example.com")).await?;
        }
        played_show(pool, act, wroclaw, "Klub X", "show-1", 40).await?;
        promoter(pool, act, wroclaw, "Anna", "anna@example.com", 70).await?;
        let anna = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM viryaos_booking_targets
             WHERE workspace_id = $1 AND contact_email = 'anna@example.com'",
        )
        .bind(act)
        .fetch_one(pool)
        .await?;

        let key = IdempotencyKey::parse("gig-score-1").expect("valid key");
        let GigOutreachOutcome::Queued { action_id, .. } =
            approve_gig_proposal(pool, act, "wroclaw", &key, now).await?
        else {
            return Err("the proposal did not queue".into());
        };

        // Before the letter runs there is no track record to read — an
        // unexecuted approval is a promise, not an outcome.
        let record = proposal_track_record(pool, act).await?;
        let proposal = record
            .proposals
            .iter()
            .find(|entry| entry.city == "wroclaw")
            .ok_or("the approved proposal is absent from its own track record")?;
        assert!(
            !proposal.is_settled(),
            "a letter that never left must not score"
        );
        assert!(
            record.by_reason.is_empty(),
            "an unsettled proposal must not move the tallies"
        );

        // The letter leaves and Anna answers inside the week — the rows the
        // measurement pipeline itself writes, driven the way it writes them.
        // The ledger enforces the real transition chain, so the fixture walks
        // it rather than teleporting the action to succeeded.
        for status in ["processing", "succeeded"] {
            sqlx::query(
                "UPDATE viryaos_autopilot_actions
                 SET status = $3, finished_at = $4
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(act)
            .bind(action_id)
            .bind(status)
            .bind(now)
            .execute(pool)
            .await?;
        }
        let measurement_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO viryaos_autopilot_measurements
                (id, workspace_id, action_id, measurement_kind, subject_id,
                 action_finished_at, baseline_value, due_at, status,
                 available_at, started_at, finished_at)
             VALUES ($1, $2, $3, 'booking_reply_7d', $4, $5, 0,
                     $5 + interval '7 days', 'succeeded', $5, $5, $6)",
        )
        .bind(measurement_id)
        .bind(act)
        .bind(action_id)
        .bind(anna)
        .bind(now)
        .bind(now + time::Duration::days(7))
        .execute(pool)
        .await?;
        let decision_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT decision_id FROM viryaos_autopilot_actions
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(act)
        .bind(action_id)
        .fetch_one(pool)
        .await?;
        sqlx::query(
            "INSERT INTO viryaos_autopilot_outcomes
                (workspace_id, decision_id, action_id, measurement_id,
                 metric_key, observed_value, baseline_value,
                 effect_assessment, delta_basis_points, observed_at)
             VALUES ($1, $2, $3, $4, 'effect.booking_reply_7d', 1.0, 0,
                     'improved', 10000, $5)",
        )
        .bind(act)
        .bind(decision_id)
        .bind(action_id)
        .bind(measurement_id)
        .bind(now + time::Duration::days(3))
        .execute(pool)
        .await?;

        // And the band gets the show — the outcome the proposal was for.
        sqlx::query(
            "INSERT INTO events
                (workspace_id, city_id, slug, title, venue, starts_at,
                 status, published_at)
             VALUES ($1, $2, 'the-gig', 'The Gig', 'Klub X',
                     now() + interval '45 days', 'published', now())",
        )
        .bind(act)
        .bind(wroclaw)
        .execute(pool)
        .await?;

        let record = proposal_track_record(pool, act).await?;
        let proposal = record
            .proposals
            .iter()
            .find(|entry| entry.city == "wroclaw")
            .ok_or("the settled proposal vanished from the track record")?;
        assert!(proposal.is_settled(), "a finished window did not settle");
        assert_eq!(proposal.replies, 1, "Anna's reply did not score");
        assert!(
            proposal.show_booked,
            "the show the letter produced did not score"
        );
        assert!(
            !proposal.reasons.is_empty(),
            "the scored proposal lost the reasons it was approved on"
        );

        // Every reason the proposal carried is now a tally with a reply and a
        // show against it — which is the answer "did the evidence hold".
        for reason in &proposal.reasons {
            let kind = serde_json::to_value(reason)
                .expect("reason encodes")
                .get("kind")
                .and_then(|value| value.as_str())
                .expect("a reason always has a kind")
                .to_owned();
            let score = record
                .by_reason
                .iter()
                .find(|score| score.kind == kind)
                .unwrap_or_else(|| panic!("reason {kind} was carried but never scored"));
            assert_eq!(score.proposals, 1);
            assert_eq!(score.replies, 1, "reason {kind} did not see the reply");
            assert_eq!(score.shows, 1, "reason {kind} did not see the show");
        }

        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}

fn repository(pool: &PgPool, url: &str) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: url.to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Test Act')")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn city(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $1, 'PL', 51.1, 17.0)
         ON CONFLICT (country_code, slug) DO UPDATE SET latitude = 51.1",
    )
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE country_code='PL' AND slug=$1")
            .bind(slug)
            .fetch_one(pool)
            .await?,
    )
}

/// Active, consented, opted into nearby gigs, inside the radius they chose.
/// All four, or the reachability gate counts nothing and the test proves
/// nothing.
async fn reachable_fan(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    email: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1, $2, 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(email)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents
            (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
         VALUES ($1, $2, 'marketing', true, 'v1', 'signup', now())",
    )
    .bind(workspace_id)
    .bind(fan)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_location_preferences
            (workspace_id, fan_id, city_id, radius_km, nearby_gigs_enabled)
         VALUES ($1, $2, $3, 50, true)",
    )
    .bind(workspace_id)
    .bind(fan)
    .bind(city_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(fan)
    .bind(city_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn played_show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    venue: &str,
    slug: &str,
    days_ago: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status)
         VALUES ($1, $2, $3, $3, $4, now() - ($5 || ' days')::interval, 'completed')",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(slug)
    .bind(venue)
    .bind(days_ago.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn promoter(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    name: &str,
    email: &str,
    score: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_booking_targets
            (workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score, capacity)
         VALUES ($1, $2, 'promoter', $3, $4, $5, 300)",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(name)
    .bind(email)
    .bind(score)
    .execute(pool)
    .await?;
    Ok(())
}

/// An executor advertising one capability, the way the worker's heartbeat does.
async fn advertise(
    pool: &PgPool,
    workspace_id: Uuid,
    capability: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_executor_instances
            (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
         VALUES ($1,'n8n-gig-test','1','sha',$2,$3)
         ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities
            (workspace_id, executor_id, capability, capability_version, observed_at, expires_at)
         VALUES ($1,'n8n-gig-test',$2,'1',$3,$4)",
    )
    .bind(workspace_id)
    .bind(capability)
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    Ok(())
}

/// A do-not-contact on one address, the way an operator sets one.
async fn block_contact(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_contact_governor
            (workspace_id, normalized_contact, last_context, last_outbound_at,
             next_contact_after, do_not_contact)
         VALUES ($1, $2, 'manual', $3, $3, true)",
    )
    .bind(workspace_id)
    .bind(email)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}
