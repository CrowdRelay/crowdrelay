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

mod common;

use crowdrelay_domain::gig_plan::{GigRefusal, TenantIntent, plan_gig};
use crowdrelay_infra::gig_planning::{city_opportunities, stated_intent};
use crowdrelay_infra::organization_settings::{
    KEY_ROSTER_PACKAGES_PER_PERIOD, OrganizationSettingsRepository,
};
use crowdrelay_infra::tenant_settings::{KEY_TENANT_INTENT, TenantSettingsRepository};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

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
    city_in(pool, slug, "PL", 51.1, 17.0).await
}

async fn city_in(
    pool: &PgPool,
    slug: &str,
    country_code: &str,
    latitude: f64,
    longitude: f64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    // Coordinates matter: the reachability gate excludes a city without them,
    // and a fixture without coordinates would silently measure zero and pass a
    // test that proved nothing.
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $1, $2, $3, $4)
         ON CONFLICT (country_code, slug)
         DO UPDATE SET latitude = $3, longitude = $4",
    )
    .bind(slug)
    .bind(country_code)
    .bind(latitude)
    .bind(longitude)
    .execute(pool)
    .await?;
    Ok(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM cities WHERE country_code = $2 AND slug = $1",
        )
        .bind(slug)
        .bind(country_code)
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
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database).await
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
        wro.reachable_fans,
        Some(60),
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
        "no peer act has billed this room, and the tenant declares no genre — \
         zero is the honest count, not a missing join"
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
    assert_eq!(
        plan.contact
            .iter()
            .map(|contact| contact.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Anna"]
    );
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
    a_roster_capacity_is_stated_or_absent(pool).await?;
    shared_fans_measure_overlap_without_naming_anybody(pool).await?;
    an_overlap_is_measured_in_each_city_separately(pool).await?;
    a_declared_open_slot_reaches_the_roster_planner(pool).await?;
    a_citys_coordinates_reach_the_roster_planner(pool).await?;
    an_act_style_is_declared_or_absent(pool).await?;

    Ok(())
}

/// §4G.3b: two acts in one organisation share part of an audience, and the
/// roster planner prices a co-bill by it.
///
/// The only identity that crosses a workspace is `normalized_email`, and the
/// only thing that comes back is a count — the test asserts the shares, and
/// the shape of the query is what keeps a list from ever being readable.
async fn shared_fans_measure_overlap_without_naming_anybody(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::gig_planning::roster_opportunity;
    use crowdrelay_infra::place_reach::audience_overlaps_by_city;

    let label = organization(pool, "label-overlap").await?;
    let head = org_workspace(pool, label, "headliner").await?;
    let support = org_workspace(pool, label, "support").await?;
    let city_id = city(pool, "poznan").await?;

    // The headliner reaches four people; two of them also follow the support.
    for index in 0..4 {
        reachable_fan(pool, head, city_id, &format!("head{index}@example.com")).await?;
    }
    for index in 0..2 {
        reachable_fan(pool, support, city_id, &format!("head{index}@example.com")).await?;
    }
    // And two the headliner does not reach at all.
    for index in 0..2 {
        reachable_fan(pool, support, city_id, &format!("own{index}@example.com")).await?;
    }
    // A fan who withdrew consent is not shared, whatever the email says.
    let withdrawn = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1, 'gone@example.com', 'active') RETURNING id",
    )
    .bind(head)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents
            (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
         VALUES ($1, $2, 'marketing', false, 'v1', 'signup', now())",
    )
    .bind(head)
    .bind(withdrawn)
    .execute(pool)
    .await?;
    reachable_fan(pool, support, city_id, "gone@example.com").await?;

    let overlaps = audience_overlaps_by_city(pool, &[head, support], &[city_id]).await?;
    let pair = overlaps
        .iter()
        .find(|pair| {
            (pair.workspace_a == head && pair.workspace_b == support)
                || (pair.workspace_a == support && pair.workspace_b == head)
        })
        .ok_or("two acts sharing two fans produced no overlap row")?;
    // Two of the headliner's four are shared: 5000bp. Two of the support's
    // five are shared: 4000bp — the share is of the asked side's audience,
    // and the two directions are different answers to different questions.
    assert_eq!(
        pair.shared, 2,
        "a consent-withdrawn address counted as shared"
    );
    assert_eq!(pair.share_of(head), Some(5_000));
    assert_eq!(pair.share_of(support), Some(4_000));

    // The roster planner sees the same number the gate measured — keyed by the
    // other act's name, which is the currency `ActOverlap` trades in.
    let opportunity = roster_opportunity(pool, label, 2, OffsetDateTime::now_utc()).await?;
    let headliner = opportunity
        .acts
        .iter()
        .find(|act| act.name == "headliner")
        .ok_or("the headliner was absent from its own roster read")?;
    assert_eq!(
        headliner.overlap_with_act("support", crowdrelay_domain::CityId::from_uuid(city_id)),
        Some(5_000),
        "the measured share did not reach the planner"
    );

    // Two acts that share *nobody* measure zero — a measured zero is the
    // co-bill the planner should propose, because the audiences do not
    // cannibalise. Only an act with no reachable audience at all is absent,
    // and absent reads as unmeasured rather than as separate.
    let stranger = org_workspace(pool, label, "stranger").await?;
    let own = city(pool, "gdansk").await?;
    reachable_fan(pool, stranger, own, "solo@example.com").await?;
    let opportunity = roster_opportunity(pool, label, 2, OffsetDateTime::now_utc()).await?;
    let stranger_act = opportunity
        .acts
        .iter()
        .find(|act| act.name == "stranger")
        .ok_or("a third act vanished from the roster read")?;
    assert_eq!(
        stranger_act.overlap_with_act("headliner", crowdrelay_domain::CityId::from_uuid(own)),
        Some(0),
        "two acts that were measured and share nobody did not report zero"
    );

    Ok(())
}

async fn org_workspace(
    pool: &PgPool,
    organization_id: Uuid,
    name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind(name)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(id)
}

/// §4G.2b: the roster's capacity is a stored organisation setting.
///
/// Driven against the real table because the whole value of the setting is the
/// difference between "never stated" and a number, and that difference is a
/// `NULL` row rather than a branch anybody can unit test.
/// The overlap two acts have is a fact about one city, not about the roster.
///
/// Driven against real rows because the whole point is a query: two acts can
/// share almost nobody in one city and almost everybody in another, and a
/// single national number applied to both is the pairing mistake the ceiling
/// exists to prevent, wearing the ceiling's own clothes.
async fn an_overlap_is_measured_in_each_city_separately(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::place_reach::audience_overlaps_by_city;

    let label = organization(pool, "label-two-cities").await?;
    let left = org_workspace(pool, label, "left-act").await?;
    let right = org_workspace(pool, label, "right-act").await?;
    // Real coordinates, far enough apart that a fan of one is outside the
    // other's radius — every fixture city sharing one point would make every
    // fan reachable everywhere and the per-city claim untestable.
    let home = city_in(pool, "katowice", "PL", 50.26, 19.02).await?;
    let away = city_in(pool, "szczecin", "PL", 53.43, 14.55).await?;

    // In their shared home city both acts reach the same two people.
    for index in 0..2 {
        reachable_fan(pool, left, home, &format!("home{index}@example.com")).await?;
        reachable_fan(pool, right, home, &format!("home{index}@example.com")).await?;
    }
    // Away, each reaches two of their own and nobody in common.
    for index in 0..2 {
        reachable_fan(pool, left, away, &format!("left{index}@example.com")).await?;
        reachable_fan(pool, right, away, &format!("right{index}@example.com")).await?;
    }

    let overlaps = audience_overlaps_by_city(pool, &[left, right], &[home, away]).await?;
    let share_in = |city_id: Uuid| {
        overlaps
            .iter()
            .find(|overlap| {
                overlap.city_id == city_id
                    && (overlap.workspace_a == left || overlap.workspace_b == left)
                    && (overlap.workspace_a == right || overlap.workspace_b == right)
            })
            .and_then(|overlap| overlap.share_of(left))
    };
    assert_eq!(
        share_in(home),
        Some(10_000),
        "two acts reaching the same people at home did not measure as one audience"
    );
    assert_eq!(
        share_in(away),
        Some(0),
        "the home city's share leaked into a city where the acts share nobody"
    );

    Ok(())
}

/// §4h-8 / 5.21 — an act says what it sounds like, or it has not said.
///
/// The package matcher will judge a shared bill on this, and the whole point
/// of the setting is that it is declared: an act mislabelled by a guess gets
/// proposed onto bills it does not fit, and nobody can see why. Absent stays
/// absent.
async fn an_act_style_is_declared_or_absent(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::tenant_settings::{KEY_ACT_STYLE, TenantSettingsRepository};

    let act = workspace(pool).await?;
    let settings = TenantSettingsRepository::new(pool.clone());
    assert_eq!(
        settings.act_style(act).await?,
        None,
        "an act that has never said reported a style"
    );

    settings
        .set_setting(act, KEY_ACT_STYLE, "  doom-leaning post-metal  ")
        .await?;
    assert_eq!(
        settings.act_style(act).await?.as_deref(),
        Some("doom-leaning post-metal"),
        "the declaration did not survive the round trip"
    );

    // A blanked field is a retraction, not an empty style: the act goes back
    // to having said nothing, which is what the pairing reads as unmeasured.
    settings.set_setting(act, KEY_ACT_STYLE, "   ").await?;
    assert_eq!(settings.act_style(act).await?, None);

    Ok(())
}

/// 4V.5 — a promoter's offer of a place on the bill reaches the planner.
///
/// The roster's cheapest move only exists when somebody declares it. This
/// drives the whole path: a published show with `open_support_slots` set is a
/// slot, and everything else — an unstated bill, a bill declared full, a draft
/// show, a night that already happened — is not.
async fn a_declared_open_slot_reaches_the_roster_planner(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::gig_planning::roster_opportunity;

    let label = organization(pool, "label-slots").await?;
    let act = org_workspace(pool, label, "slot-act").await?;
    let lodz = city(pool, "lodz").await?;

    let slot_show = published_show(pool, act, lodz, "with-room", 40).await?;
    let _silent_show = published_show(pool, act, lodz, "nobody-said", 45).await?;
    let full_show = published_show(pool, act, lodz, "bill-is-full", 50).await?;
    sqlx::query("UPDATE events SET open_support_slots = 1 WHERE id = $1")
        .bind(slot_show)
        .execute(pool)
        .await?;
    // A slot on a festival bill is the organiser's stage, not the
    // headliner's room — the planner must be able to say so (5.8).
    sqlx::query("UPDATE events SET festival_name = 'OFF Festival' WHERE id = $1")
        .bind(slot_show)
        .execute(pool)
        .await?;
    // Declared full. A zero is an answer somebody gave, and the planner acts
    // on it exactly as it acts on silence: no slot. The difference matters to
    // the operator reading it, not to the plan.
    sqlx::query("UPDATE events SET open_support_slots = 0 WHERE id = $1")
        .bind(full_show)
        .execute(pool)
        .await?;

    let opportunity = roster_opportunity(pool, label, 2, OffsetDateTime::now_utc()).await?;
    assert_eq!(
        opportunity.open_slots.len(),
        1,
        "exactly the declared slot is a slot, got {:?}",
        opportunity
            .open_slots
            .iter()
            .map(|slot| slot.venue.as_str())
            .collect::<Vec<_>>()
    );
    let slot = &opportunity.open_slots[0];
    assert_eq!(slot.city, "lodz");
    assert_eq!(slot.headliner, "slot-act");
    assert_eq!(
        slot.festival_name.as_deref(),
        Some("OFF Festival"),
        "a festival slot that lost its name reads as a room it is not"
    );
    assert!(
        (39..=41).contains(&slot.days_until_show),
        "the lead time is the show's own, got {}",
        slot.days_until_show
    );
    // The silent show is not a slot and is not an error either — nobody has
    // said, which is the state most shows are in.
    assert!(
        !opportunity
            .open_slots
            .iter()
            .any(|slot| slot.venue.contains("nobody")),
        "a show nobody has spoken about was read as an offer"
    );

    // A draft show is not a commitment anybody made, and a night that already
    // happened cannot offer a place on its bill.
    let draft = published_show(pool, act, lodz, "still-a-draft", 60).await?;
    sqlx::query("UPDATE events SET status = 'draft', open_support_slots = 2 WHERE id = $1")
        .bind(draft)
        .execute(pool)
        .await?;
    let past = published_show(pool, act, lodz, "last-month", -30).await?;
    sqlx::query("UPDATE events SET open_support_slots = 2 WHERE id = $1")
        .bind(past)
        .execute(pool)
        .await?;
    let opportunity = roster_opportunity(pool, label, 2, OffsetDateTime::now_utc()).await?;
    assert_eq!(
        opportunity.open_slots.len(),
        1,
        "a draft or a past show was read as an open slot"
    );

    Ok(())
}

/// 5.8 — a corridor can only be routed if the catalogue's coordinates reach
/// the planner. The pin is the wiring, not the math (the domain pins that):
/// a city the catalogue has pinned must arrive carrying its pin.
async fn a_citys_coordinates_reach_the_roster_planner(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::gig_planning::roster_opportunity;

    let label = organization(pool, "label-corridor").await?;
    let act = org_workspace(pool, label, "corridor-act").await?;
    let krakow = city_in(pool, "krakow", "PL", 50.06, 19.94).await?;
    reachable_fan(pool, act, krakow, "corridor@example.com").await?;

    let opportunity = roster_opportunity(pool, label, 2, OffsetDateTime::now_utc()).await?;
    let krakow_opp = opportunity
        .cities
        .iter()
        .find(|opp| opp.city_id == crowdrelay_domain::CityId::from_uuid(krakow))
        .ok_or("the city the act reaches was absent from its roster read")?;
    assert_eq!(
        (krakow_opp.latitude, krakow_opp.longitude),
        (Some(50.06), Some(19.94)),
        "a corridor cannot be routed over coordinates that never arrived"
    );

    // A city the catalogue never pinned still surfaces as an opportunity —
    // it just cannot be a leg, and `None` must say so rather than a
    // defaulted zero at the equator.
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code) VALUES ('unmapped', 'unmapped', 'PL')
         ON CONFLICT (country_code, slug) DO UPDATE SET latitude = NULL, longitude = NULL",
    )
    .execute(pool)
    .await?;
    let unmapped: Uuid =
        sqlx::query_scalar("SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'unmapped'")
            .fetch_one(pool)
            .await?;
    reachable_fan(pool, act, unmapped, "unmapped@example.com").await?;
    let opportunity = roster_opportunity(pool, label, 2, OffsetDateTime::now_utc()).await?;
    let unmapped_opp = opportunity
        .cities
        .iter()
        .find(|opp| opp.city_id == crowdrelay_domain::CityId::from_uuid(unmapped))
        .ok_or("an unlocated city vanished instead of arriving unlocated")?;
    assert_eq!(
        (unmapped_opp.latitude, unmapped_opp.longitude),
        (None, None),
        "an unlocatable city must read unlocatable, not zeroed"
    );

    Ok(())
}

/// A published show at a fixed distance from now, returning its id.
async fn published_show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    slug: &str,
    days_from_now: i64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, $3, $3, $3, now() + ($4 || ' days')::interval, 'published', now())
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(slug)
    .bind(days_from_now.to_string())
    .fetch_one(pool)
    .await?)
}

async fn a_roster_capacity_is_stated_or_absent(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let settings = OrganizationSettingsRepository::new(pool.clone());
    let label = organization(pool, "label-one").await?;
    let other_label = organization(pool, "label-two").await?;

    assert_eq!(
        settings.packages_this_period(label).await?,
        None,
        "a roster that has never stated a capacity reported one"
    );

    settings
        .set(label, KEY_ROSTER_PACKAGES_PER_PERIOD, "3")
        .await?;
    assert_eq!(settings.packages_this_period(label).await?, Some(3));

    // An upsert replaces rather than duplicating: the primary key is
    // (organization_id, key), and a second row would make the answer depend on
    // which one the reader saw first.
    settings
        .set(label, KEY_ROSTER_PACKAGES_PER_PERIOD, "5")
        .await?;
    assert_eq!(settings.packages_this_period(label).await?, Some(5));
    assert_eq!(settings.list(label).await?.len(), 1);

    // One label's number never sizes another's plan.
    assert_eq!(
        settings.packages_this_period(other_label).await?,
        None,
        "one organisation's capacity leaked into another"
    );

    // A hand-edited row outside the bounds reads as absent rather than being
    // clamped. Clamping would size a plan by a number nobody chose, and the
    // manager would have no way to tell.
    settings
        .set(label, KEY_ROSTER_PACKAGES_PER_PERIOD, "40")
        .await?;
    assert_eq!(
        settings.packages_this_period(label).await?,
        None,
        "an out-of-range stored capacity was clamped instead of refused"
    );
    settings
        .set(label, KEY_ROSTER_PACKAGES_PER_PERIOD, "not a number")
        .await?;
    assert_eq!(settings.packages_this_period(label).await?, None);

    Ok(())
}

async fn organization(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO organizations (slug, name) VALUES ($1, $1) RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await?)
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

/// Two catalogue rows may share a slug — the unique key is
/// `(country_code, slug)`, not slug alone. The planner must keep them as two
/// cities, each with its own evidence: a pipeline keyed on the slug merges
/// them into one phantom opportunity whose reach unions both radii and whose
/// room is whichever country the query happened to order first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn two_cities_sharing_a_slug_do_not_share_their_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let act = workspace(&database).await?;
        let now = OffsetDateTime::now_utc();
        // Wrocław, Poland — and a German namesake hundreds of kilometres away.
        // Same slug, different city, different audience.
        let wroclaw_pl = city_in(&database, "wroclaw", "PL", 51.1, 17.0).await?;
        let wroclaw_de = city_in(&database, "wroclaw", "DE", 48.0, 7.85).await?;
        for index in 0..60 {
            reachable_fan(
                &database,
                act,
                wroclaw_pl,
                &format!("pl{index}@example.com"),
            )
            .await?;
        }
        for index in 0..30 {
            reachable_fan(
                &database,
                act,
                wroclaw_de,
                &format!("de{index}@example.com"),
            )
            .await?;
        }
        played_show(&database, act, wroclaw_pl, "Klub X", "show-pl", 40).await?;
        played_show(&database, act, wroclaw_de, "Forum Y", "show-de", 40).await?;

        let opportunities = city_opportunities(&database, act, now).await?;
        let pl = opportunities
            .iter()
            .find(|city| city.city_id.into_uuid() == wroclaw_pl)
            .ok_or("the PL city did not surface as its own opportunity")?;
        let de = opportunities
            .iter()
            .find(|city| city.city_id.into_uuid() == wroclaw_de)
            .ok_or("the DE namesake did not surface as its own opportunity")?;

        assert_eq!(
            pl.city, de.city,
            "the fixture does not test what it claims unless both rows share the slug"
        );
        assert_eq!(
            pl.reachable_fans,
            Some(60),
            "the PL count picked up the DE radius — the cities merged"
        );
        assert_eq!(
            de.reachable_fans,
            Some(30),
            "the DE count picked up the PL radius — the cities merged"
        );
        assert_eq!(
            pl.venue.as_ref().map(|venue| venue.name.as_str()),
            Some("Klub X"),
            "the PL proposal named the DE room"
        );
        assert_eq!(
            de.venue.as_ref().map(|venue| venue.name.as_str()),
            Some("Forum Y"),
            "the DE proposal named the PL room"
        );
        Ok(())
    }
    .await
}

/// Reachability has three honest answers, not two: a measured count (which
/// may be zero), or unmeasurable. A city without coordinates and a city that
/// is simply unknown both answer `None` — folding either into zero would put
/// "only 0 people asked to hear from you" on a city the system never counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unlocatable_city_is_unmeasurable_not_zero() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let pool = &database;
        let workspace = workspace(pool).await?;

        // Measured, and the measurement is nobody.
        let empty_city = city_in(pool, "pustkow", "PL", 50.0, 20.0).await?;
        assert_eq!(
            crowdrelay_infra::place_reach::reachable_in_city(pool, workspace, empty_city).await?,
            Some(0),
            "a coordinate-carrying city with no fans nearby is a measured zero"
        );

        // Present in the catalogue but unlocatable — nothing to anchor a
        // radius to.
        sqlx::query(
            "INSERT INTO cities (slug, name, country_code, latitude, longitude)
             VALUES ('nowhere', 'Nowhere', 'PL', NULL, NULL)",
        )
        .execute(pool)
        .await?;
        let nowhere: Uuid = sqlx::query_scalar("SELECT id FROM cities WHERE slug = 'nowhere'")
            .fetch_one(pool)
            .await?;
        assert_eq!(
            crowdrelay_infra::place_reach::reachable_in_city(pool, workspace, nowhere).await?,
            None,
            "a city without coordinates cannot be measured"
        );

        // Not in the catalogue at all.
        assert_eq!(
            crowdrelay_infra::place_reach::reachable_in_city(pool, workspace, Uuid::now_v7())
                .await?,
            None,
            "an unknown city cannot be measured"
        );
        Ok(())
    }
    .await
}

/// A peer act with one genre claim — returns the registry id so the same
/// peer can be billed twice (the count is DISTINCT acts, not appearances).
async fn peer_act(pool: &PgPool, genre: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
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
    Ok(peer)
}

/// Bills `peer` on `event_id` — written directly, the way
/// `booking_window_postgres` seeds the same shape: the resolution path that
/// would set `peer_act_id` has its own suite.
async fn bill_peer(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
    peer: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
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

async fn peer_on_bill(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
    genre: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let peer = peer_act(pool, genre).await?;
    bill_peer(pool, workspace_id, event_id, peer).await
}

/// The comparable-acts count reaches the planner (N.2): a billed act whose
/// genre intersects the tenant's declared set counts once per act; an
/// unrelated genre does not, and neither does the tenant's own act — a band
/// is not its own comparable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn comparable_acts_reach_the_planner() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let pool = &database;
        let act = workspace(pool).await?;
        let wroclaw = city(pool, "wroclaw-comparables").await?;
        let now = OffsetDateTime::now_utc();

        // The tenant declares a genre; without it "comparable" has no "mine"
        // side to intersect and every act counts as incomparable.
        sqlx::query(
            "INSERT INTO viryaos_band_listings (workspace_id, act_name, genre_tags)
             VALUES ($1, 'Test Act', '{doom metal}')",
        )
        .bind(act)
        .execute(pool)
        .await?;

        for index in 0..60 {
            reachable_fan(pool, act, wroclaw, &format!("cmp{index}@example.com")).await?;
        }

        // The tenant's own show marks the room — and its own act on the bill
        // must not count as its own comparable.
        let own_show = played_show(pool, act, wroclaw, "Klub X", "own-show", 30).await?;
        sqlx::query(
            "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, act_workspace_id)
             VALUES ($1, $2, 'us', 'Test Act', $1)",
        )
        .bind(act)
        .bind(own_show)
        .execute(pool)
        .await?;

        // The other tenant declares a genre too — 'dream pop'. If the
        // genre read forgot the workspace filter, the tenant would borrow
        // 'dream pop' as its own set and the count below would change.
        let other = workspace(pool).await?;
        sqlx::query(
            "INSERT INTO viryaos_band_listings (workspace_id, act_name, genre_tags)
             VALUES ($1, 'Other Act', '{dream pop}')",
        )
        .bind(other)
        .execute(pool)
        .await?;

        // No fixture insert: the 0316 seed map itself is what makes "doom"
        // the same genre as "doom metal" — if the seed row ever goes missing
        // this test fails, which is the point.

        // Another tenant's bill at the same room carries two peers: one doom,
        // one not. Only the doom one intersects the tenant's genres — and the
        // same peer billed twice still counts once (acts, not appearances).
        let doom_peer = peer_act(pool, "doom").await?;
        let their_show = played_show(pool, other, wroclaw, "Klub X", "their-show", 20).await?;
        bill_peer(pool, other, their_show, doom_peer).await?;
        let encore = played_show(pool, other, wroclaw, "Klub X", "their-encore", 15).await?;
        bill_peer(pool, other, encore, doom_peer).await?;
        peer_on_bill(pool, other, their_show, "dream pop").await?;

        // A second room gets its own matching peer — the count is per venue,
        // and Klub Y's bill must not leak into Klub X's. One mark keeps Klub
        // X the winner either way.
        let klub_y_night = played_show(pool, other, wroclaw, "Klub Y", "other-room", 10).await?;
        peer_on_bill(pool, other, klub_y_night, "doom metal").await?;

        let wro = city_opportunities(pool, act, now)
            .await?
            .into_iter()
            .find(|city| city.city == "wroclaw-comparables")
            .ok_or("a city with sixty fans and marked room was not considered")?;
        let venue = wro
            .venue
            .as_ref()
            .ok_or("a marked room produced no venue evidence")?;
        assert_eq!(
            venue.comparable_acts, 1,
            "the doom peer counts once across two bills; the dream-pop peer, \
             the other tenant's genre set and Klub Y's bill do not count"
        );

        // The count reaches the letter: with a route on file the plan
        // proposes, and the comparable-acts reason — strongest first — is
        // what the opening line renders.
        sqlx::query(
            "INSERT INTO viryaos_booking_targets
                (workspace_id, city_id, target_kind, display_name, contact_email,
                 relationship_score)
             VALUES ($1, $2, 'promoter', 'Anna', 'anna@example.com', 70)",
        )
        .bind(act)
        .bind(wroclaw)
        .execute(pool)
        .await?;
        let wro = city_opportunities(pool, act, now)
            .await?
            .into_iter()
            .find(|city| city.city == "wroclaw-comparables")
            .ok_or("city vanished after adding a promoter")?;
        let plan = plan_gig(&wro, TenantIntent::BookingShows).expect("proposes with a route");
        assert!(
            matches!(
                plan.reasons.first(),
                Some(
                    crowdrelay_domain::gig_plan::Reason::ComparableActsPlayedHere {
                        count: 1,
                        of_shows: 3
                    }
                )
            ),
            "the comparable-acts reason did not lead the proposal: {:?}",
            plan.reasons
        );
        assert_eq!(
            plan.opening_line(),
            "One act from our genre has played Klub X on record.",
            "the opening line must render the count the graph measured"
        );
        Ok(())
    }
    .await
}
