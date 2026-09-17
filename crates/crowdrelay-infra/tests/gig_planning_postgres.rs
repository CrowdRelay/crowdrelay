//! The planners are pure and well tested. This checks the half that is not:
//! that the evidence handed to them is the evidence the database holds.
//!
//! Every distinction the domain depends on is a `NULL` here — a city never
//! played, a room with no ticketed show, a venue with no booking target — and
//! every one of them is a column that could be folded to zero by a `COALESCE`
//! somebody adds to make a query tidier. Folding any of them makes the planner
//! confident about something nobody measured, and a confident wrong proposal
//! costs a band a week.
//!
//! SQLx runs these queries at runtime by design, so a column that does not
//! exist is a production failure with no compile-time warning. Four such
//! mistakes were caught by driving the attestation queries rather than reading
//! them; these are driven for the same reason.

use crowdrelay_domain::gig_plan::{GigRefusal, TenantIntent, plan_gig};
use crowdrelay_infra::gig_planning::{city_opportunities, stated_intent};
use crowdrelay_infra::tenant_settings::{KEY_TENANT_INTENT, TenantSettingsRepository};
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
        let name = format!("crowdrelay_gigplan_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{head}/{name}"))
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
    // Coordinates matter: the reachability gate excludes a city without them,
    // and a fixture without coordinates would silently measure zero and pass a
    // test that proved nothing.
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $1, 'PL', 51.1, 17.0)
         ON CONFLICT (country_code, slug)
         DO UPDATE SET latitude = 51.1, longitude = 17.0",
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

/// A fan who is active, consented, opted into nearby gigs and inside the
/// radius. All four are required, which is the point: a fixture that satisfies
/// three of them measures zero and would make a green test out of a broken
/// gate.
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
        // `policy_version` and `source` are NOT NULL with CHECKs against blank.
        // Spelled
        // out rather than defaulted: a fixture that sidesteps a real
        // constraint proves nothing about the real table.
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

/// A completed show, which marks the room in the shared registry through the
/// trigger from migration 0290.
async fn played_show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    venue: &str,
    slug: &str,
    days_ago: i64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
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
    .await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_evidence_handed_to_the_planner_is_what_the_database_holds()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(pool).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    let now = OffsetDateTime::now_utc();

    // Sixty reachable people, above the planner's floor of fifty.
    for index in 0..60 {
        reachable_fan(pool, act, wroclaw, &format!("fan{index}@example.com")).await?;
    }
    played_show(pool, act, wroclaw, "Klub X", "show-1", 400).await?;
    played_show(pool, act, wroclaw, "Klub X", "show-2", 40).await?;

    let opportunities = city_opportunities(pool, act, now).await?;
    let wro = opportunities
        .iter()
        .find(|city| city.city == "wroclaw")
        .ok_or("the city with sixty fans and two shows was not considered")?;

    assert_eq!(
        wro.reachable_fans, 60,
        "the reachability gate did not count fans who satisfy all four conditions"
    );
    assert!(!wro.has_upcoming_show, "a completed show read as upcoming");

    let venue = wro
        .venue
        .as_ref()
        .ok_or("a room marked by two completed shows produced no venue evidence")?;
    assert_eq!(venue.name, "Klub X");
    // One of the two shows is inside twelve months; the 400-day-old one is not.
    // A window that counted both would overstate how active the room is.
    assert_eq!(
        venue.shows_last_12_months, 1,
        "the twelve-month window counted a show from over a year ago"
    );
    assert!(
        venue.days_since_last_event.is_some_and(|days| days <= 41),
        "days since the last event did not come from the most recent show: {:?}",
        venue.days_since_last_event
    );

    // ── The NULLs the planner's whole value rests on ────────────────────────
    assert!(
        venue.typical_draw.is_none(),
        "a room with no ticketed show reported a draw instead of an absence"
    );
    assert!(
        venue.capacity.is_none(),
        "a room with no booking target reported a capacity from nowhere"
    );
    assert!(
        !venue.has_booking_route,
        "a room nobody has a booking target for claimed a route"
    );
    assert!(
        venue.contact_verified_days_ago.is_none(),
        "a contact nobody has ever written to reported a verification age"
    );
    assert_eq!(
        venue.comparable_acts, 0,
        "comparable acts must stay zero until the peer-act graph exists"
    );

    // The planner refuses this city — no route to anybody — and that refusal is
    // the correct, useful answer rather than a failure.
    assert_eq!(
        plan_gig(wro, TenantIntent::BookingShows),
        Err(GigRefusal::NoContactableRoute {
            venue: "Klub X".to_owned()
        }),
        "a room with no contactable route was proposed anyway"
    );

    // ── Give it a promoter, and it proposes ─────────────────────────────────
    sqlx::query(
        "INSERT INTO viryaos_booking_targets
            (workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score, capacity)
         VALUES ($1, $2, 'promoter', 'Anna', 'anna@example.com', 70, 300)",
    )
    .bind(act)
    .bind(wroclaw)
    .execute(pool)
    .await?;

    let with_promoter = city_opportunities(pool, act, now).await?;
    let wro = with_promoter
        .iter()
        .find(|city| city.city == "wroclaw")
        .ok_or("city vanished after adding a promoter")?;
    assert_eq!(wro.promoters.len(), 1);
    assert_eq!(wro.promoters[0].name, "Anna");
    assert!(
        !wro.promoters[0].answered_last_time,
        "a promoter who has never replied was recorded as having answered"
    );

    let plan = plan_gig(wro, TenantIntent::BookingShows).expect("proposes with a route");
    assert_eq!(plan.city, "wroclaw");
    assert_eq!(plan.venue, "Klub X");
    assert_eq!(plan.contact, vec!["Anna"]);
    // The unmeasured facts travel as caveats rather than vanishing.
    assert!(
        plan.caveats.iter().any(|note| note.contains("unmeasured")),
        "the unmeasured draw did not reach the band: {:?}",
        plan.caveats
    );

    // ── A city the band has never played stays absent, not zero ─────────────
    let praha = city(pool, "praha").await?;
    for index in 0..60 {
        reachable_fan(pool, act, praha, &format!("praha{index}@example.com")).await?;
    }
    let never_played = city_opportunities(pool, act, now).await?;
    let pra = never_played
        .iter()
        .find(|city| city.city == "praha")
        .ok_or("a city with sixty fans and no history was not considered")?;
    assert!(
        pra.months_since_show.is_none(),
        "a city never played reported a month count instead of an absence"
    );
    assert!(
        pra.venue.is_none(),
        "a city with no marked room produced venue evidence from nowhere"
    );
    assert_eq!(
        plan_gig(pra, TenantIntent::BookingShows),
        Err(GigRefusal::NoRoomOnRecord)
    );

    // ── A booked show closes the gap ────────────────────────────────────────
    sqlx::query(
        // `published_at` is required by CHECK whenever the status is
        // published — a published event with no publication time is a state
        // the schema refuses, and the fixture has to respect that or it is
        // testing a row production could never hold.
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, 'upcoming', 'Upcoming', 'Klub X', now() + interval '30 days',
                 'published', now())",
    )
    .bind(act)
    .bind(wroclaw)
    .execute(pool)
    .await?;
    let after_booking = city_opportunities(pool, act, now).await?;
    let wro = after_booking
        .iter()
        .find(|city| city.city == "wroclaw")
        .ok_or("city vanished after a booking")?;
    assert!(wro.has_upcoming_show);
    assert_eq!(
        plan_gig(wro, TenantIntent::BookingShows),
        Err(GigRefusal::AlreadyBooked),
        "a city with a show on the calendar was proposed as a gap"
    );

    stated_intent_comes_from_the_act_that_stated_it(pool, act).await?;

    Ok(())
}

/// §4G.2: the intent is a stored setting, and it is read per workspace.
///
/// The roster planner asks each act's own workspace, never the label's, because
/// an act that says it is recording must not be proposed by somebody else's
/// preference. This drives the read against a real row rather than trusting the
/// key string, which is the half a unit test cannot check: a typo in
/// `KEY_TENANT_INTENT` would read `None` forever and every band would silently
/// look unstated.
async fn stated_intent_comes_from_the_act_that_stated_it(
    pool: &PgPool,
    act: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    let settings = TenantSettingsRepository::new(pool.clone());

    // Never asked is not the same as asked and declined to say. Both plan as
    // `Unstated`, and the console shows the difference.
    assert_eq!(
        settings.tenant_intent(act).await?,
        None,
        "an act that has never stated an intent reported a stored one"
    );
    assert_eq!(
        stated_intent(&settings, act).await?,
        TenantIntent::Unstated,
        "an unstated act did not resolve to Unstated"
    );

    for intent in TenantIntent::all() {
        settings
            .set_setting(act, KEY_TENANT_INTENT, intent.as_str())
            .await?;
        assert_eq!(
            stated_intent(&settings, act).await?,
            intent,
            "the stored intent did not survive the round trip through the settings row"
        );
    }

    // A hand-edited row the vocabulary does not recognise plans as `Unstated`:
    // weaker, and it says the timing is unverified. It must never read as
    // `HeadsDown`, because withholding every proposal on the strength of a typo
    // is the failure nobody would ever report.
    settings
        .set_setting(act, KEY_TENANT_INTENT, "touring")
        .await?;
    assert_eq!(
        stated_intent(&settings, act).await?,
        TenantIntent::Unstated,
        "an unreadable stored value did not fall back to Unstated"
    );

    // A second act in the same database keeps its own answer. One act's
    // heads-down must not silence a labelmate.
    let other = workspace(pool).await?;
    settings
        .set_setting(act, KEY_TENANT_INTENT, TenantIntent::HeadsDown.as_str())
        .await?;
    assert_eq!(
        stated_intent(&settings, act).await?,
        TenantIntent::HeadsDown
    );
    assert_eq!(
        stated_intent(&settings, other).await?,
        TenantIntent::Unstated,
        "one act's stated intent leaked into another workspace"
    );

    Ok(())
}
