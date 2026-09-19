//! §12-6 end to end: a venue-linked booking target carries the room's
//! evidence row and the window loader reads the room's own history; the
//! outreach action then reserves every recipient it names, or none.
//!
//! Three isolation properties are what the tests exist to prove:
//!
//! * an unlinked target carries `venue_evidence: None` — never a row of
//!   fabricated zeroes;
//! * `booking_contact_days` is the requesting tenant's own freshness only —
//!   a second tenant's fresher private fact must never appear;
//! * a two-recipient letter locks and reserves both contacts inside one
//!   transaction, so a stale version on the second leaves the first
//!   unreserved — all of them, or none of them.

use std::time::Duration;

use crowdrelay_application::autopilot::{AutopilotActionRepository, AutopilotDecisionRepository};
use crowdrelay_domain::booking::BookingOutreachPhase;
use crowdrelay_domain::{BookingTargetId, CityId, WorkspaceId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_4v7_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

fn repository(pool: &PgPool) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: "postgres://unused".to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

async fn workspace(pool: &PgPool, name: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

/// A city with real coordinates — the window loader reads them for
/// `venue_coords` and own-show adjacency.
async fn city_in(
    pool: &PgPool,
    slug: &str,
    latitude: f64,
    longitude: f64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $1, 'PL', $2, $3)
         ON CONFLICT (country_code, slug)
         DO UPDATE SET latitude = $2, longitude = $3",
    )
    .bind(slug)
    .bind(latitude)
    .bind(longitude)
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = $1",
    )
    .bind(slug)
    .fetch_one(pool)
    .await?)
}

/// A completed show `days_ago` at a named room — the marks trigger turns it
/// into the shared registry's `place_venues` + `place_venue_marks` rows.
async fn played_show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    venue: &str,
    slug: &str,
    days_ago: i64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status)
         VALUES ($1, $2, $3, $3, $4, now() - ($5 || ' days')::interval, 'completed')
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(slug)
    .bind(venue)
    .bind(days_ago.to_string())
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn seed_target(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    kind: &str,
    display_name: &str,
    contact_email: &str,
    priority: i32,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_booking_targets
             (workspace_id, city_id, target_kind, display_name, contact_email, priority)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(contact_email)
    .bind(priority)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// The tenant's own listing genres — the "mine" half of comparable acts.
async fn band_listing(
    pool: &PgPool,
    workspace_id: Uuid,
    genres: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_band_listings (workspace_id, act_name, genre_tags)
         VALUES ($1, 'Test Act', $2)
         ON CONFLICT (workspace_id) DO UPDATE SET genre_tags = $2",
    )
    .bind(workspace_id)
    .bind(genres)
    .execute(pool)
    .await?;
    Ok(())
}

/// A peer act with one genre claim, billed on `event_id`.
async fn peer_on_bill(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
    genre: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let peer = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_peer_acts (name_key, display_name)
         VALUES (place_venue_key($1), $1) RETURNING id",
    )
    .bind(format!("Peer {genre} {}", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO place_peer_act_genres (peer_act_id, genre_tag, provenance, source_ref)
         VALUES ($1, $2, 'researched', 'test')",
    )
    .bind(peer)
    .bind(genre)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, peer_act_id)
         VALUES ($1, $2, $3, $3, $4)",
    )
    .bind(workspace_id)
    .bind(event_id)
    .bind(format!("peer-{}", Uuid::now_v7().simple()))
    .bind(peer)
    .execute(pool)
    .await?;
    Ok(())
}

/// An executor advertising one capability, the way the worker heartbeat does.
async fn advertise(
    pool: &PgPool,
    workspace_id: Uuid,
    capability: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_executor_instances
            (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
         VALUES ($1,'n8n-booking-test','1','sha',$2,$3)
         ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + Duration::from_secs(1800))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities
            (workspace_id, executor_id, capability, capability_version, observed_at, expires_at)
         VALUES ($1,'n8n-booking-test',$2,'1',$3,$4)",
    )
    .bind(workspace_id)
    .bind(capability)
    .bind(now)
    .bind(now + Duration::from_secs(1800))
    .execute(pool)
    .await?;
    Ok(())
}

/// A queued booking outreach action, seeded the way the evaluator's persist
/// path would write it — decision row first, then the action with the typed
/// payload serialized to JSONB.
async fn seed_booking_action(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    payload: serde_json::Value,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO viryaos_autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'booking_opportunity','city',$4,
                 'request_booking_outreach',9000,'require_approval','seeded booking proposal',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("booking-decision-{}", Uuid::now_v7()))
    .bind(city_id)
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    sqlx::query_scalar(
        "INSERT INTO viryaos_autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, action_class)
         VALUES ($1,$2,$3,'booking_opportunity','booking.outreach.request','city',
                 $4,$5,$6,'queued','third_party') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(city_id)
    .bind(format!("booking-action-{}", Uuid::now_v7()))
    .bind(payload)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_venue_linked_target_carries_its_evidence_and_window_inputs()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let pool = &database.pool;
    let result = run_evidence_case(pool).await;
    database.drop_database().await;
    result
}

async fn run_evidence_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool, "Act One").await?;
    let other = workspace(pool, "Act Two").await?;
    // Wrocław with real coordinates — the venue city carries them.
    let wroclaw = city_in(pool, "wroclaw", 51.11, 17.03).await?;
    let krakow = city_in(pool, "krakow", 50.06, 19.94).await?;

    // The room's own history is the shared registry's: one show of ours and
    // one of another tenant's both count.
    let our_show = played_show(pool, act, wroclaw, "Klub X", "ours-at-x", 30).await?;
    let their_show = played_show(pool, other, wroclaw, "Klub X", "theirs-at-x", 20).await?;
    let venue_id =
        sqlx::query_scalar::<_, Uuid>("SELECT venue_id FROM place_venue_marks WHERE event_id = $1")
            .bind(our_show)
            .fetch_one(pool)
            .await?;

    // The tenant's genres plus one comparable peer on the other tenant's
    // bill — and the requesting tenant's own act on its own bill, which must
    // never count as comparable to itself.
    band_listing(pool, act, &["metal"]).await?;
    peer_on_bill(pool, other, their_show, "Metal").await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, act_workspace_id)
         VALUES ($1, $2, 'self', 'Self Act', $1)",
    )
    .bind(act)
    .bind(our_show)
    .execute(pool)
    .await?;

    // Global facts resolve in trust order; the private fact exposes its age
    // only, and only the requesting tenant's.
    for (ws, days_ago) in [(None, 40_i64), (Some(act), 10), (Some(other), 2)] {
        match ws {
            None => {
                sqlx::query(
                    "INSERT INTO place_venue_facts
                         (venue_id, attribute, value, provenance, source_ref, observed_at)
                     VALUES ($1, 'capacity', '300', 'researched', 'directory',
                             now() - ($2 || ' days')::interval)",
                )
                .bind(venue_id)
                .bind(days_ago.to_string())
                .execute(pool)
                .await?;
                sqlx::query(
                    "INSERT INTO place_venue_facts
                         (venue_id, attribute, value, provenance, source_ref, observed_at)
                     VALUES ($1, 'genres', 'metal, stoner', 'researched', 'directory',
                             now() - ($2 || ' days')::interval)",
                )
                .bind(venue_id)
                .bind(days_ago.to_string())
                .execute(pool)
                .await?;
            }
            Some(workspace_id) => {
                sqlx::query(
                    "INSERT INTO place_venue_facts
                         (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
                     VALUES ($1, 'booking_email', $3, 'researched', 'test',
                             now() - ($4 || ' days')::interval, $2)",
                )
                .bind(venue_id)
                .bind(workspace_id)
                .bind(format!("booking+{}@example.com", workspace_id.simple()))
                .bind(days_ago.to_string())
                .execute(pool)
                .await?;
            }
        }
    }

    // The anchor resolves to the room by name; the promoter never links.
    let anchor = seed_target(
        pool,
        act,
        wroclaw,
        "venue",
        "Klub X",
        "booking@klub-x.example",
        90,
    )
    .await?;
    let promoter = seed_target(
        pool,
        act,
        wroclaw,
        "promoter",
        "Promoter Jan",
        "jan@example.com",
        60,
    )
    .await?;

    // The tenant's own calendar for the window loader: one confirmed show in
    // Kraków (~250 km away) and one draft — a cancelled event must not block.
    for (slug, status, days_ahead) in [
        ("krakow-night", "published", 25_i64),
        ("draft-night", "draft", 40),
        ("cancelled-night", "cancelled", 30),
    ] {
        sqlx::query(
            "INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
             VALUES ($1, $2, $3, $3, 'Other Room', now() + ($4 || ' days')::interval, $5,
                     CASE WHEN $5 = 'published' THEN now() ELSE NULL END)",
        )
        .bind(act)
        .bind(krakow)
        .bind(slug)
        .bind(days_ahead.to_string())
        .bind(status)
        .execute(pool)
        .await?;
    }

    let repo = repository(pool);
    let snapshots = repo
        .load_booking_target_snapshots(WorkspaceId::from_uuid(act), now)
        .await?;

    let linked = snapshots
        .iter()
        .find(|target| target.target_id == BookingTargetId::from_uuid(anchor))
        .expect("the anchor target is in the snapshot set");
    let evidence = linked
        .venue_evidence
        .as_ref()
        .expect("a venue-linked target carries its evidence row");
    // Both tenants' marks count — the registry is shared.
    assert_eq!(evidence.shows_last_12m, 2, "both tenants' shows count");
    assert_eq!(
        evidence.days_since_last_event,
        Some(20),
        "the newest marked show was the other tenant's, 20 days back"
    );
    // The peer on the other tenant's bill matches on genre; the tenant's own
    // act never counts against itself.
    assert_eq!(evidence.comparable_acts, 1);
    assert_eq!(evidence.genres.as_deref(), Some("metal, stoner"));
    assert_eq!(evidence.capacity.as_deref(), Some("300"));
    // The freshest private booking_email is this tenant's 10-day-old fact —
    // the other tenant's 2-day-old fact must not appear.
    assert_eq!(evidence.booking_contact_days, Some(10));

    let unlinked = snapshots
        .iter()
        .find(|target| target.target_id == BookingTargetId::from_uuid(promoter))
        .expect("the promoter is in the snapshot set");
    assert_eq!(
        unlinked.venue_evidence, None,
        "a promoter never fabricates a venue evidence row"
    );

    // The window inputs: the room's marks for the linked target only, plus
    // the tenant's calendar with coordinates where known.
    let inputs = repo
        .load_booking_window_inputs(WorkspaceId::from_uuid(act), now)
        .await?;
    let anchor_inputs = inputs
        .targets
        .iter()
        .find(|input| input.target_id == BookingTargetId::from_uuid(anchor))
        .expect("the linked target has window inputs");
    assert_eq!(
        anchor_inputs.room_shows.len(),
        2,
        "the room's marks are the shared registry's, not just ours"
    );
    assert!(
        anchor_inputs.venue_coords.is_some(),
        "the venue city carries coordinates"
    );
    assert!(
        inputs
            .targets
            .iter()
            .all(|input| input.target_id != BookingTargetId::from_uuid(promoter)),
        "an unlinked target has no window inputs"
    );
    assert_eq!(
        inputs.own_shows.len(),
        2,
        "published and draft shows ride; cancelled does not"
    );
    assert!(
        inputs
            .own_shows
            .iter()
            .find(|show| show.slug == "krakow-night")
            .is_some_and(|show| show.confirmed && show.coords.is_some()),
        "the confirmed Kraków show carries its city coordinates"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn booking_outreach_reserves_every_recipient_or_none()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let pool = &database.pool;
    let result = run_execution_case(pool).await;
    database.drop_database().await;
    result
}

async fn run_execution_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool, "Act One").await?;
    let wroclaw = city_in(pool, "wroclaw", 51.11, 17.03).await?;
    let anchor = seed_target(
        pool,
        act,
        wroclaw,
        "venue",
        "Klub X",
        "klubx@example.com",
        90,
    )
    .await?;
    let extra = seed_target(
        pool,
        act,
        wroclaw,
        "promoter",
        "Promoter Jan",
        "jan@example.com",
        60,
    )
    .await?;
    advertise(pool, act, "booking.outreach", now).await?;

    let day = (now + time::Duration::days(21)).date();
    let payload = serde_json::to_value(
        crowdrelay_application::autopilot::AutopilotActionPayload::RequestBookingOutreach {
            city_id: CityId::from_uuid(wroclaw),
            target_id: BookingTargetId::from_uuid(anchor),
            target_version: 1,
            target_name: "Klub X".to_owned(),
            score: 71,
            phase: BookingOutreachPhase::Initial,
            proposed_window: Some(crowdrelay_domain::booking_window::ProposedWindow {
                start: day,
                end: day + time::Duration::days(28),
                basis: vec![crowdrelay_domain::booking_window::WindowBasis::LeadTime {
                    median_days: 42,
                }],
            }),
            additional_recipients: vec![(BookingTargetId::from_uuid(extra), 1)],
            draft: crowdrelay_domain::booking_letter::BookingLetter {
                subject: "Act One — booking in Wrocław".to_owned(),
                body: "Cześć,\n\nJesteśmy Act One.".to_owned(),
            },
            venue_evidence: Some(crowdrelay_domain::booking::BookingVenueEvidence {
                shows_last_12m: 9,
                comparable_acts: 3,
                genres: Some("metal".to_owned()),
                capacity: None,
                days_since_last_event: Some(11),
                booking_contact_days: None,
            }),
        },
    )?;
    let action_id = seed_booking_action(pool, act, wroclaw, payload).await?;

    let repo = repository(pool);
    // A fresh now — `available_at` defaulted to the insert's `now()`, which is
    // later than the `now` this case opened with.
    let claim_now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(act), 8, claim_now)
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .expect("the queued outreach is claimable");
    repo.execute_action(WorkspaceId::from_uuid(act), action, claim_now)
        .await?;

    // Both contacts are reserved under this action — the anchor's and the
    // copied recipient's.
    let reserved = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_contact_governor
         WHERE workspace_id = $1 AND last_action_id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(reserved, 2, "both recipients are reserved under one action");

    // The emitted letter carries the whole recipient set, the proposed
    // window verbatim, and the first-line fact rendered from the approved
    // evidence row.
    let emitted: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.booking.outreach_requested'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        emitted["recipients"].as_array().map(Vec::len),
        Some(2),
        "the letter names the anchor and the extra recipient"
    );
    assert_eq!(
        emitted["proposed_window"]["basis"][0]["lead_time"]["median_days"],
        serde_json::json!(42),
        "the approved window travels verbatim"
    );
    assert_eq!(
        emitted["first_line_fact"].as_str(),
        Some("9 shows in the last year, 3 comparable acts on its bills, programmes metal."),
    );
    // The letter travels approved — the executor sends these words verbatim.
    assert_eq!(
        emitted["draft"]["subject"].as_str(),
        Some("Act One — booking in Wrocław")
    );

    // And every recipient's clock moved — one touch per person.
    let touched = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_booking_targets
         WHERE workspace_id = $1 AND last_outreach_at IS NOT NULL",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(touched, 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_stale_second_recipient_aborts_and_reserves_nothing()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let pool = &database.pool;
    let result = run_stale_recipient_case(pool).await;
    database.drop_database().await;
    result
}

async fn run_stale_recipient_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool, "Act One").await?;
    let wroclaw = city_in(pool, "wroclaw", 51.11, 17.03).await?;
    let anchor = seed_target(
        pool,
        act,
        wroclaw,
        "venue",
        "Klub X",
        "klubx@example.com",
        90,
    )
    .await?;
    let extra = seed_target(
        pool,
        act,
        wroclaw,
        "promoter",
        "Promoter Jan",
        "jan@example.com",
        60,
    )
    .await?;
    advertise(pool, act, "booking.outreach", now).await?;

    // The payload claims the second recipient at a version the row no longer
    // holds — the edit between approval and send must abort the whole letter.
    let payload = serde_json::to_value(
        crowdrelay_application::autopilot::AutopilotActionPayload::RequestBookingOutreach {
            city_id: CityId::from_uuid(wroclaw),
            target_id: BookingTargetId::from_uuid(anchor),
            target_version: 1,
            target_name: "Klub X".to_owned(),
            score: 71,
            phase: BookingOutreachPhase::Initial,
            proposed_window: None,
            additional_recipients: vec![(BookingTargetId::from_uuid(extra), 9_999)],
            draft: crowdrelay_domain::booking_letter::BookingLetter {
                subject: "Act One — booking in Wrocław".to_owned(),
                body: "Cześć,\n\nJesteśmy Act One.".to_owned(),
            },
            venue_evidence: None,
        },
    )?;
    let action_id = seed_booking_action(pool, act, wroclaw, payload).await?;

    let repo = repository(pool);
    let claim_now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(act), 8, claim_now)
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .expect("the queued outreach is claimable");
    let outcome = repo
        .execute_action(WorkspaceId::from_uuid(act), action, claim_now)
        .await;
    assert!(
        matches!(
            outcome,
            Err(crowdrelay_application::RepositoryError::Conflict)
        ),
        "a stale recipient version must abort the action, got {outcome:?}"
    );

    // Nothing reserved, nothing emitted, nobody's clock touched — the
    // transaction took the anchor's reservation down with the refusal.
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
        "no contact stays reserved for a letter that never left"
    );
    let emitted = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.booking.outreach_requested'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(emitted, 0, "a partial letter reached the outbox");
    let touched = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_booking_targets
         WHERE workspace_id = $1 AND last_outreach_at IS NOT NULL",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(touched, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_reply_after_approval_retires_the_letter_before_it_sends()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let pool = &database.pool;
    let result = run_reply_after_approval_case(pool).await;
    database.drop_database().await;
    result
}

async fn run_reply_after_approval_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool, "Act One").await?;
    let wroclaw = city_in(pool, "wroclaw", 51.11, 17.03).await?;
    let anchor = seed_target(
        pool,
        act,
        wroclaw,
        "venue",
        "Klub X",
        "klubx@example.com",
        90,
    )
    .await?;
    let extra = seed_target(
        pool,
        act,
        wroclaw,
        "promoter",
        "Promoter Jan",
        "jan@example.com",
        60,
    )
    .await?;
    advertise(pool, act, "booking.outreach", now).await?;

    // The operator approves the letter — then the promoter writes back before
    // the send window opens. A reply is the lead becoming served, not a fresher
    // prospect: the dispatch lock must refuse the wave the way the evaluator's
    // `last_reply` hold already refused the proposal.
    sqlx::query(
        "INSERT INTO viryaos_booking_interactions
             (workspace_id, target_id, direction, phase, disposition, source_key, occurred_at)
         VALUES ($1, $2, 'inbound', 'reply', 'positive', $3, now())",
    )
    .bind(act)
    .bind(extra)
    .bind(format!("reply-{}", Uuid::now_v7()))
    .execute(pool)
    .await?;

    let payload = serde_json::to_value(
        crowdrelay_application::autopilot::AutopilotActionPayload::RequestBookingOutreach {
            city_id: CityId::from_uuid(wroclaw),
            target_id: BookingTargetId::from_uuid(anchor),
            target_version: 1,
            target_name: "Klub X".to_owned(),
            score: 71,
            phase: BookingOutreachPhase::Initial,
            proposed_window: None,
            additional_recipients: vec![(BookingTargetId::from_uuid(extra), 1)],
            draft: crowdrelay_domain::booking_letter::BookingLetter {
                subject: "Act One — booking in Wrocław".to_owned(),
                body: "Cześć,\n\nJesteśmy Act One.".to_owned(),
            },
            venue_evidence: None,
        },
    )?;
    let action_id = seed_booking_action(pool, act, wroclaw, payload).await?;

    let repo = repository(pool);
    let claim_now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(act), 8, claim_now)
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .expect("the queued outreach is claimable");
    let outcome = repo
        .execute_action(WorkspaceId::from_uuid(act), action, claim_now)
        .await;
    assert!(
        matches!(
            outcome,
            Err(crowdrelay_application::RepositoryError::Conflict)
        ),
        "a target that replied after approval must refuse the wave, got {outcome:?}"
    );

    // The whole wave dies together — no reservation on the anchor, no outward
    // intent in the outbox, nobody's outreach clock moved.
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
        "a replied wave may not leave half-reserved contacts"
    );
    let emitted = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM outbox_events
         WHERE workspace_id = $1 AND event_type = 'crowdrelay.booking.outreach_requested'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        emitted, 0,
        "a replied lead may not receive the approved pitch"
    );
    let touched = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_booking_targets
         WHERE workspace_id = $1 AND last_outreach_at IS NOT NULL",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(touched, 0);
    Ok(())
}
