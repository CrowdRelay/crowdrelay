//! The peer-act half of the gig-planning evidence checks — split from
//! `gig_planning.rs` when the file outgrew the source-size ratchet. Same
//! contract: every distinction the planner reads is a `NULL` or a count the
//! database must actually produce — billed peers, genre intersections, a
//! resolved room or act status — and each is driven against real rows so a
//! folded `COALESCE` fails here rather than in a proposal a band paid for.
//!
//! Every test here runs on a database of its own. Peer acts are one registry
//! across every tenant, unique on the normalized name, and the assertions
//! name them ("Lead Act", "Dead Act") and count them per room — on the
//! shared suite database a second run finds the first run's peers.

use crate::common;
use crate::gig_planning::{city, played_show, reachable_fan, workspace};

use crowdrelay_domain::gig_plan::{TenantIntent, plan_gig};
use crowdrelay_infra::gig_planning::city_opportunities;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

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
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("create a migrated database of this test's own");
    let outcome = comparable_acts_reach_the_planner_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn comparable_acts_reach_the_planner_on(
    database: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    async {
        let pool = &database;
        let act = workspace(pool).await?;
        let wroclaw = city(pool, "wroclaw-comparables").await?;
        let now = OffsetDateTime::now_utc();

        // The tenant declares a genre; without it "comparable" has no "mine"
        // side to intersect and every act counts as incomparable.
        sqlx::query(
            "INSERT INTO band_listings (workspace_id, act_name, genre_tags)
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
            "INSERT INTO band_listings (workspace_id, act_name, genre_tags)
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
            "INSERT INTO booking_targets
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

/// The support-slot list names acts the tenant could actually ask. A peer
/// qualifies on a city tie — researched home town or a tracked billing in
/// the city's rooms — but only earns the slot when a reach-out route
/// exists: this workspace's contact lead, a billing in any tracked room
/// (the venue is the intro), or a public page on record. A genre-matching
/// name with none of the three is a directory entry, not a suggestion —
/// that row is the Rammstein case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn local_acts_only_surface_when_they_can_be_asked() -> Result<(), Box<dyn std::error::Error>>
{
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("create a migrated database of this test's own");
    let outcome = local_acts_only_surface_when_they_can_be_asked_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn local_acts_only_surface_when_they_can_be_asked_on(
    database: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool: &PgPool = &database;
    {
        let act = workspace(pool).await?;
        let wroclaw = city(pool, "wroclaw-askable").await?;
        let now = OffsetDateTime::now_utc();

        sqlx::query(
            "INSERT INTO band_listings (workspace_id, act_name, genre_tags)
             VALUES ($1, 'Test Act', '{doom metal}')",
        )
        .bind(act)
        .execute(pool)
        .await?;
        for index in 0..60 {
            reachable_fan(pool, act, wroclaw, &format!("ask{index}@example.com")).await?;
        }
        // The city's venue evidence — and one billing route for the
        // circuit act below.
        let other = workspace(pool).await?;
        let night = played_show(pool, other, wroclaw, "Klub Ask", "ask-night", 20).await?;

        // Reachable through this tenant's own lead.
        let lead = named_peer_act(pool, "Lead Act", wroclaw, "doom metal").await?;
        peer_fact(pool, lead, "contact_email", "lead@example.com", Some(act)).await?;
        // Reachable through the circuit: billed at the city's tracked room.
        let circuit = named_peer_act(pool, "Circuit Act", wroclaw, "doom metal").await?;
        bill_peer(pool, other, night, circuit).await?;
        // Reachable through a public page — a global link fact.
        let page = named_peer_act(pool, "Page Act", wroclaw, "doom metal").await?;
        peer_fact(
            pool,
            page,
            "link:social",
            "https://example.com/pageact",
            None,
        )
        .await?;
        // The directory names that must NOT surface.
        named_peer_act(pool, "Arena Act", wroclaw, "doom metal").await?;
        named_peer_act(
            pool,
            "Far Act",
            city(pool, "far-askable").await?,
            "doom metal",
        )
        .await?;
        let foreign = named_peer_act(pool, "Foreign Lead Act", wroclaw, "doom metal").await?;
        peer_fact(
            pool,
            foreign,
            "contact_email",
            "theirs@example.com",
            Some(other),
        )
        .await?;

        let wro = city_opportunities(pool, act, now)
            .await?
            .into_iter()
            .find(|city| city.city == "wroclaw-askable")
            .ok_or("a city with sixty fans was not considered")?;
        let mut names: Vec<String> = wro.local_acts.iter().map(|act| act.name.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Circuit Act".to_owned(),
                "Lead Act".to_owned(),
                "Page Act".to_owned()
            ],
            "a name with no reach-out route — Arena Act — and a lead that is \
             not this tenant's stay out of the bill suggestions"
        );
        let via = |name: &str| {
            wro.local_acts
                .iter()
                .find(|act| act.name == name)
                .map(|act| act.reachable_via.clone())
        };
        assert_eq!(via("Lead Act").as_deref(), Some("email on file"));
        assert_eq!(
            via("Circuit Act").as_deref(),
            Some("billed in tracked rooms")
        );
        assert_eq!(via("Page Act").as_deref(), Some("public page"));
        for act in &wro.local_acts {
            assert_eq!(
                act.shared_genres,
                vec!["doom metal".to_owned()],
                "the genre tie still reports"
            );
        }
        Ok(())
    }
}

/// The Łykend case, driven: a room marked played and then reported closed
/// must leave the proposal — a dead room is not a booking lead, however many
/// shows it hosted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_closed_room_is_not_proposed() -> Result<(), Box<dyn std::error::Error>> {
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("create a migrated database of this test's own");
    let outcome = a_closed_room_is_not_proposed_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn a_closed_room_is_not_proposed_on(pool: PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(&pool).await?;
    // A private city: `place_venues` is global, so a shared city would let a
    // sibling test's rooms outrank this one's and the assertion would measure
    // somebody else's fixture.
    let wroclaw = city(&pool, "shutville").await?;
    let now = OffsetDateTime::now_utc();
    for index in 0..60 {
        reachable_fan(
            &pool,
            act,
            wroclaw,
            &format!("closed-fan{index}@example.com"),
        )
        .await?;
    }
    played_show(&pool, act, wroclaw, "Klub Shut", "closed-show-1", 40).await?;
    played_show(&pool, act, wroclaw, "Klub Shut", "closed-show-2", 45).await?;
    played_show(&pool, act, wroclaw, "Klub Open", "open-show-1", 41).await?;

    // Klub Shut leads on the room record (two shows to Open's one — the
    // count decides before the id tiebreak, so the pick is deterministic);
    // the report that it closed must remove it.
    // The source_ref carries a fresh id so a re-run on a reused database is
    // a new claim, not a unique-index collision with the last run's.
    sqlx::query(
        "INSERT INTO place_venue_facts
            (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
         SELECT venue.id, 'status', 'closed', 'researched',
                'https://example.com/closed/' || gen_random_uuid(), now(), NULL
         FROM place_venues AS venue
         JOIN cities ON cities.id = venue.city_id
         WHERE cities.slug = 'shutville' AND venue.name_key = 'klub shut'",
    )
    .execute(&pool)
    .await?;

    let opportunities = city_opportunities(&pool, act, now).await?;
    let wro = opportunities
        .iter()
        .find(|city| city.city == "shutville")
        .ok_or("the city was not considered")?;
    let venue = wro
        .venue
        .as_ref()
        .ok_or("a city holding an open room produced no venue evidence")?;
    assert_eq!(
        venue.name, "Klub Open",
        "the closed room still anchored the proposal"
    );

    // And when the only room in town is closed, the answer is "no room on
    // record" — the refusal that asks for research, not a dead lead.
    let empty = city(&pool, "emptytown").await?;
    played_show(&pool, act, empty, "Dead Room", "dead-show-1", 30).await?;
    sqlx::query(
        "INSERT INTO place_venue_facts
            (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
         SELECT venue.id, 'status', 'closed', 'researched',
                'https://example.com/closed/' || gen_random_uuid(), now(), NULL
         FROM place_venues AS venue
         JOIN cities ON cities.id = venue.city_id
         WHERE cities.slug = 'emptytown' AND venue.name_key = 'dead room'",
    )
    .execute(&pool)
    .await?;
    let opportunities = city_opportunities(&pool, act, now).await?;
    let empty_city = opportunities
        .iter()
        .find(|city| city.city == "emptytown")
        .ok_or("the city was not considered")?;
    assert!(
        empty_city.venue.is_none(),
        "a closed-only city still proposed a room: {:?}",
        empty_city.venue
    );

    // The reopen path: a fresh `active` claim outranks the stale `closed` on
    // recency within the same provenance — the resolved status, not the mere
    // existence of a closed row, decides. Klub Shut comes back, and with the
    // deeper record (two shows to Open's one) it is the proposal again.
    sqlx::query(
        "INSERT INTO place_venue_facts
            (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
         SELECT venue.id, 'status', 'active', 'researched',
                'https://example.com/reopen/' || gen_random_uuid(), now() + interval '1 second', NULL
         FROM place_venues AS venue
         JOIN cities ON cities.id = venue.city_id
         WHERE cities.slug = 'shutville' AND venue.name_key = 'klub shut'",
    )
    .execute(&pool)
    .await?;
    let opportunities = city_opportunities(&pool, act, now).await?;
    let reopened = opportunities
        .iter()
        .find(|city| city.city == "shutville")
        .and_then(|city| city.venue.as_ref())
        .ok_or("a reopened room produced no venue evidence")?;
    assert_eq!(
        reopened.name, "Klub Shut",
        "a fresh 'active' fact did not lift the closure"
    );
    Ok(())
}

/// A peer act whose researched home town resolves to a catalogue city —
/// the fixture shape the seed importer produces.
async fn named_peer_act(
    pool: &PgPool,
    display_name: &str,
    home_city_id: Uuid,
    genre: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let peer = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_peer_acts (name_key, display_name, home_city_id)
         VALUES (place_venue_key($1), $1, $2) RETURNING id",
    )
    .bind(display_name)
    .bind(home_city_id)
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

/// One attributed fact about a peer — `workspace` NULL is the global half,
/// a concrete id is that tenant's private lead.
async fn peer_fact(
    pool: &PgPool,
    peer_act_id: Uuid,
    attribute: &str,
    value: &str,
    workspace: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO place_peer_act_facts
            (peer_act_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
         VALUES ($1, $2, $3, 'researched', 'test', now(), $4)",
    )
    .bind(peer_act_id)
    .bind(attribute)
    .bind(value)
    .bind(workspace)
    .execute(pool)
    .await?;
    Ok(())
}

/// The band-side mirror of the closed-room rule: a peer act whose resolved
/// status is `inactive` is not a support suggestion, even with a lead on
/// file — and a fresher `active` claim brings it back, the same ladder
/// rooms follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_dead_band_is_not_a_local_suggestion() -> Result<(), Box<dyn std::error::Error>> {
    let isolated = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("create a migrated database of this test's own");
    let outcome = a_dead_band_is_not_a_local_suggestion_on(isolated.pool.clone()).await;
    isolated.drop().await?;
    outcome
}

async fn a_dead_band_is_not_a_local_suggestion_on(
    pool: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(&pool).await?;
    let wroclaw = city(&pool, "wroclaw-dead-act").await?;
    let now = OffsetDateTime::now_utc();

    sqlx::query(
        "INSERT INTO band_listings (workspace_id, act_name, genre_tags)
         VALUES ($1, 'Test Act', '{doom metal}')",
    )
    .bind(act)
    .execute(&pool)
    .await?;
    for index in 0..60 {
        reachable_fan(&pool, act, wroclaw, &format!("dead{index}@example.com")).await?;
    }

    // Two otherwise-identical hometown acts with leads on file — one the
    // sheet calls dead, one it confirms live.
    let dead = named_peer_act(&pool, "Dead Act", wroclaw, "doom metal").await?;
    peer_fact(&pool, dead, "contact_email", "dead@example.com", Some(act)).await?;
    peer_fact(&pool, dead, "status", "inactive", None).await?;
    let live = named_peer_act(&pool, "Live Act", wroclaw, "doom metal").await?;
    peer_fact(&pool, live, "contact_email", "live@example.com", Some(act)).await?;
    peer_fact(&pool, live, "status", "active", None).await?;

    let names = |rows: &[crowdrelay_domain::gig_plan::LocalAct]| {
        rows.iter().map(|act| act.name.clone()).collect::<Vec<_>>()
    };
    let wro = city_opportunities(&pool, act, now)
        .await?
        .into_iter()
        .find(|city| city.city == "wroclaw-dead-act")
        .ok_or("a city with sixty fans was not considered")?;
    let found = names(&wro.local_acts);
    assert!(
        found.iter().any(|name| name == "Live Act"),
        "the live act did not surface: {found:?}"
    );
    assert!(
        !found.iter().any(|name| name == "Dead Act"),
        "a band marked inactive still proposed: {found:?}"
    );

    // The reopen: a newer `active` claim wins the ladder and the band is a
    // suggestion again.
    sqlx::query(
        "INSERT INTO place_peer_act_facts
            (peer_act_id, attribute, value, provenance, source_ref, observed_at)
         VALUES ($1, 'status', 'active', 'researched', 'reunion-post', now() + interval '1 minute')",
    )
    .bind(dead)
    .execute(&pool)
    .await?;
    let wro = city_opportunities(&pool, act, now)
        .await?
        .into_iter()
        .find(|city| city.city == "wroclaw-dead-act")
        .ok_or("the city dropped out")?;
    let found = names(&wro.local_acts);
    assert!(
        found.iter().any(|name| name == "Dead Act"),
        "a re-formed act stayed excluded: {found:?}"
    );
    Ok(())
}
