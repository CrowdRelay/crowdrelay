//! §4h-11 — the staging queue read against a date, and the city a press
//! contact keeps when it is promoted (3.9, 3.10).
//!
//! Both halves are database shape rather than policy: a column that must carry
//! through a promote, and a read that must place candidates in a city while
//! scoping each tenant to its own — plus the one deliberate global read, the
//! cold rooms any tenant may see. SQLx checks none of that at compile time,
//! so it is driven here.

use crate::common;

use crowdrelay_infra::show_helpers::who_can_help;
use sqlx::PgPool;
use uuid::Uuid;

include!("show_helpers_yield.rs");

/// 3.10 — two workspaces seed different candidates for the same city; each
/// sees only its own plus the shared cold rooms; and an event with no city
/// degrades rather than erroring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn each_workspace_sees_its_own_candidates_and_the_shared_cold_rooms()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database).await
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(pool).await?;
    let other = workspace(pool).await?;
    let wroclaw = city(pool, "wroclaw", "Wrocław", "PL", 51.1, 17.03).await?;
    let krakow = city(pool, "krakow", "Kraków", "PL", 50.06, 19.94).await?;
    let berlin = city(pool, "berlin", "Berlin", "DE", 52.52, 13.4).await?;
    // The show's own venue mark lands via the event trigger — "Klub A" is a
    // room this tenant has played, so it must never list as cold.
    show(pool, act, Some(wroclaw), "friday", "Klub A").await?;
    show(pool, other, Some(wroclaw), "friday-too", "Klub B").await?;
    show(pool, act, None, "no-city-yet", "TBA").await?;
    show(pool, act, Some(wroclaw), "draft-show", "Klub A").await?;
    sqlx::query("UPDATE events SET status = 'draft' WHERE slug = 'draft-show'")
        .execute(pool)
        .await?;

    // ── press: proposed only, this workspace, this city, media kinds ──
    press(pool, act, Some(wroclaw), "Gazeta", "press", "proposed").await?;
    // Promoted contacts are already past the candidate stage — the promote
    // flow owns them, so the shortlist does not repeat them.
    press(pool, act, Some(wroclaw), "Old Paper", "press", "promoted").await?;
    // Another city's paper must not appear on this night.
    press(pool, act, Some(krakow), "Kraków Daily", "press", "proposed").await?;
    // An endorsement target is not press — the kind list is the media set.
    press(
        pool,
        act,
        Some(wroclaw),
        "Sponsor Lady",
        "endorsement",
        "proposed",
    )
    .await?;
    // The other workspace's candidate for the same city stays its own.
    press(
        pool,
        other,
        Some(wroclaw),
        "Rival Radio",
        "radio",
        "proposed",
    )
    .await?;

    // ── rooms_and_promoters: active only, this workspace, this city ──
    booking_target(pool, act, wroclaw, "Anna", "promoter", true).await?;
    booking_target(pool, act, wroclaw, "Dead Club", "venue", false).await?;
    booking_target(pool, other, wroclaw, "Their Booker", "promoter", true).await?;
    // P.6 prior seed: this tenant mailed Anna once with no reply, and the
    // other workspace's outreach ledger holds a positive thread on the same
    // address — the record reads "2 wrote, 1 answered, 1 won" without ever
    // naming the other tenant.
    sqlx::query(
        "UPDATE booking_targets SET last_outreach_at = now() - interval '30 days'
         WHERE workspace_id = $1 AND contact_email = 'anna@example.com'",
    )
    .bind(act)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO outreach_targets
            (workspace_id, target_kind, display_name, contact_email, last_outreach_at,
             last_reply_at, last_reply_disposition)
         VALUES ($1, 'support_slot', 'Anna (theirs)', 'anna@example.com',
                 now() - interval '40 days', now() - interval '39 days', 'positive')",
    )
    .bind(other)
    .execute(pool)
    .await?;
    // The prior reads replies from the interaction ledger, not the
    // denormalized disposition — seed the inbound row too.
    sqlx::query(
        "INSERT INTO outreach_interactions
            (workspace_id, target_id, direction, phase, disposition, source_key, occurred_at)
         SELECT workspace_id, id, 'inbound', 'reply', 'positive',
                'seed-anna-reply', now() - interval '39 days'
         FROM outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'anna@example.com'",
    )
    .bind(other)
    .execute(pool)
    .await?;

    // ── communities: this workspace, the city's country, active ──
    let yielding_community = community(pool, act, "PL", "r/wroclaw", true).await?;
    // Alphabetically earlier but unmeasured: fan yield, not name, should put
    // the proven community first in the helper shortlist.
    community(pool, act, "PL", "r/aaa-unmeasured", true).await?;
    community(pool, act, "PL", "r/inactive", false).await?;
    community(pool, act, "DE", "r/berlin", true).await?;
    community(pool, other, "PL", "r/theirs", true).await?;

    // One measurable community journey: post -> tracked click -> owned fan.
    // The helper read should surface this as decision evidence for the booker.
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions
            (id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'growth_intelligence','target_community',$4,
                 'seed.community',9000,'auto_execute','seed helper yield',
                 '{}','{}','{}',now(),$5)",
    )
    .bind(decision_id)
    .bind(act)
    .bind(format!("helper-yield-{decision_id}"))
    .bind(yielding_community)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions
            (id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, finished_at)
         VALUES ($1,$2,$3,'growth_intelligence','community.engage.request',
                 'target_community',$4,$5,'{}','succeeded',now())",
    )
    .bind(action_id)
    .bind(act)
    .bind(decision_id)
    .bind(yielding_community)
    .bind(format!("helper-yield-action-{action_id}"))
    .execute(pool)
    .await?;
    let link_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO smart_links (id, workspace_id, slug, destination_url)
         VALUES ($1,$2,$3,'https://example.test/fan')",
    )
    .bind(link_id)
    .bind(act)
    .bind(format!("helper-yield-{}", link_id.simple()))
    .execute(pool)
    .await?;
    let link_slug: String =
        sqlx::query_scalar("SELECT slug FROM smart_links WHERE workspace_id=$1 AND id=$2")
            .bind(act)
            .bind(link_id)
            .fetch_one(pool)
            .await?;
    sqlx::query(
        "INSERT INTO community_posts
            (workspace_id, action_id, target_id, subreddit, title, body,
             smart_link, status, posted_at)
         VALUES ($1,$2,$3,'r/wroclaw','show','body',$4,'posted',
                 now() - interval '50 days')",
    )
    .bind(act)
    .bind(action_id)
    .bind(yielding_community)
    .bind(format!("/l/{link_slug}"))
    .execute(pool)
    .await?;
    let visitor_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO click_events
            (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
         VALUES ($1,$2,$3,now() - interval '49 days')",
    )
    .bind(act)
    .bind(link_id)
    .bind(visitor_id)
    .execute(pool)
    .await?;
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status, created_at)
         VALUES ($1,$2,'active',now() - interval '48 days') RETURNING id",
    )
    .bind(act)
    .bind(format!("helper-yield-{}@example.test", visitor_id.simple()))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_acquisition_events
            (workspace_id, fan_id, source, request_id, anonymous_visitor_id, occurred_at)
         VALUES ($1,$2,'public_signup',$3,$4,now() - interval '48 days')",
    )
    .bind(act)
    .bind(fan_id)
    .bind(Uuid::now_v7())
    .bind(visitor_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO fan_provenance_events
            (workspace_id, fan_id, event_kind, channel, source_target, community,
             action_id, attribution_method, occurred_at)
         VALUES ($1,$2,'conversion','reddit',$3,'r/wroclaw',$4,
                 'last_tracked_click',now() - interval '48 days')",
    )
    .bind(act)
    .bind(fan_id)
    .bind(&link_slug)
    .bind(action_id)
    .execute(pool)
    .await?;
    seed_earlier_community_touch(pool, act, decision_id, visitor_id).await?;

    // ── cold_rooms: the registry's rooms minus this tenant's marks ──
    // "Klub A" (marked by the show above) and "Klub B" (marked by the other
    // workspace) already exist in place_venues through the trigger. Two more
    // are rooms nobody has marked — one with a public capacity fact, one
    // with none, and a private fact on the first that must never leak.
    let cold = venue(pool, wroclaw, "Cold Klub").await?;
    let no_capacity = venue(pool, wroclaw, "No Cap Klub").await?;
    venue(pool, berlin, "Berlin Cold").await?;
    venue_fact(pool, cold, "capacity", "350", None).await?;
    venue_fact(pool, no_capacity, "capacity", "999", Some(other)).await?;

    // ── P.3 sections ──
    // bill_mates: a peer act on friday's bill (the kind nobody contacts), a
    // second bill they shared before, a tenant act the crossbill edge already
    // covers, and an act of our own — the last two must not list.
    let friday_id = event_id(pool, act, "friday").await?;
    sqlx::query("INSERT INTO fan_consents (workspace_id,fan_id,purpose,granted,policy_version,source) VALUES ($1,$2,'marketing',true,'v1','helper-proof')")
        .bind(act).bind(fan_id).execute(pool).await?;
    sqlx::query("INSERT INTO event_interests (workspace_id,event_id,fan_id) VALUES ($1,$2,$3)")
        .bind(act)
        .bind(friday_id)
        .bind(fan_id)
        .execute(pool)
        .await?;
    let peer = peer_act(pool, "The Openers").await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position, peer_act_id)
         VALUES ($1, $2, 'the-openers', 'The Openers', 1, $3)",
    )
    .bind(act)
    .bind(friday_id)
    .bind(peer)
    .execute(pool)
    .await?;
    // A past shared bill is the warm fact the row carries.
    show(pool, act, Some(wroclaw), "last-month", "Klub A").await?;
    let last_month = event_id(pool, act, "last-month").await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position, peer_act_id)
         VALUES ($1, $2, 'the-openers', 'The Openers', 1, $3)",
    )
    .bind(act)
    .bind(last_month)
    .bind(peer)
    .execute(pool)
    .await?;
    // A cancelled bill they shared does not count toward shared_bills.
    sqlx::query(
        "INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, 'cancelled-night', 'cancelled-night', 'Klub A', now() + interval '20 days', 'cancelled', now())",
    )
    .bind(act).bind(wroclaw).execute(pool).await?;
    let cancelled = event_id(pool, act, "cancelled-night").await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position, peer_act_id)
         VALUES ($1, $2, 'the-openers', 'The Openers', 1, $3)",
    )
    .bind(act)
    .bind(cancelled)
    .bind(peer)
    .execute(pool)
    .await?;
    // An unclaimed act — resolver never minted it — still lists.
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position)
         VALUES ($1, $2, 'mystery-band', 'Mystery Band', 3)",
    )
    .bind(act)
    .bind(friday_id)
    .execute(pool)
    .await?;
    // The band's own listing name on the bill stays out — an unresolved
    // collision must not offer "add yourself to your roster".
    sqlx::query("INSERT INTO band_listings (workspace_id, act_name) VALUES ($1, 'Virya')")
        .bind(act)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position)
         VALUES ($1, $2, 'virya', 'Virya', 4)",
    )
    .bind(act)
    .bind(friday_id)
    .execute(pool)
    .await?;
    // A same-named beacon in the wrong city must not flip on_roster.
    beacon(pool, act, krakow, "The Openers", "scene_partner", None).await?;
    // A tenant act — the crossbill edge is its channel, not this section.
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position, act_workspace_id)
         VALUES ($1, $2, 'tenant-band', 'Tenant Band', 2, $3)",
    )
    .bind(act).bind(friday_id).bind(other).execute(pool).await?;
    // Our own act — never a candidate.
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position, act_workspace_id)
         VALUES ($1, $2, 'us', 'Us', 0, $3)",
    )
    .bind(act).bind(friday_id).bind(act).execute(pool).await?;

    // venue_channel: "Klub A" was marked by the event trigger; a venue-kind
    // beacon of the same name means the channel is already on the roster.
    beacon(pool, act, wroclaw, "Klub A", "venue", None).await?;
    // photographers: one in the city the band has written to, one in the
    // wrong city, one that is the other workspace's.
    beacon(
        pool,
        act,
        wroclaw,
        "Basia Lens",
        "photographer",
        Some("basia@example.com"),
    )
    .await?;
    beacon(pool, act, krakow, "Krakow Lens", "photographer", None).await?;
    let suppressed = beacon(
        pool,
        act,
        wroclaw,
        "Suppressed Lens",
        "photographer",
        Some("gone@example.com"),
    )
    .await?;
    sqlx::query("UPDATE beacons SET do_not_contact = true WHERE id = $1")
        .bind(suppressed)
        .execute(pool)
        .await?;
    beacon(pool, other, wroclaw, "Their Lens", "photographer", None).await?;
    sqlx::query(
        "INSERT INTO contact_governor
            (workspace_id, normalized_contact, last_outbound_at, next_contact_after, last_context)
         VALUES ($1, 'basia@example.com', now() - interval '10 days', now() - interval '3 days', 'beacon.outreach')",
    )
    .bind(act).execute(pool).await?;

    let helpers = who_can_help(pool, act, "friday")
        .await?
        .ok_or("the show produced no helper read")?;
    assert_eq!(helpers.event.slug, "friday");
    assert_eq!(helpers.event.city.as_deref(), Some("Wrocław"));
    assert_eq!(helpers.event.country_code.as_deref(), Some("PL"));
    assert_eq!(
        helpers.event.city_id,
        Some(wroclaw),
        "the id is what an admit action needs to place the beacon"
    );
    assert!(
        helpers.truncated.is_empty(),
        "no section hit the cap: {:?}",
        helpers.truncated
    );
    assert!(
        helpers.degraded.is_empty(),
        "degraded: {:?}",
        helpers.degraded
    );
    assert_eq!(helpers.notes, ["staged_contacts_have_no_city"]);

    // press — only the city's proposed media contact, nothing promoted,
    // nothing another city's, nothing that is not a media kind, nothing the
    // other workspace holds.
    let names: Vec<&str> = helpers
        .press
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(names, ["Gazeta"], "press section: {names:?}");
    assert_eq!(helpers.press[0].target_kind, "press");
    // No contact_email field exists on the row at all — the type carries no
    // address, which is the assertion that cannot drift.

    let rooms: Vec<&str> = helpers
        .rooms_and_promoters
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(
        rooms,
        ["Anna"],
        "rooms_and_promoters must hold only this tenant's active targets: {rooms:?}"
    );
    // P.6 — the cross-tenant record rides the row: two tenants' sends, one
    // reply, one win. Only counts ever leave the read.
    let anna_prior = helpers.rooms_and_promoters[0]
        .counterparty_prior
        .expect("the prior read ran");
    assert_eq!(
        (
            anna_prior.tenants_contacted,
            anna_prior.tenants_replied,
            anna_prior.tenants_won
        ),
        (2, 1, 1),
        "anna@example.com: this tenant's send + the other workspace's won thread"
    );
    assert!(
        helpers.rooms_and_promoters[0].venue_prior.is_none(),
        "a promoter-kind target has no linked room record"
    );

    let communities: Vec<(&str, &str)> = helpers
        .communities
        .iter()
        .map(|row| (row.community_name.as_str(), row.country.as_str()))
        .collect();
    assert_eq!(
        communities,
        [
            ("r/wroclaw", "PL"),
            ("r/earlier-touch", "PL"),
            ("r/aaa-unmeasured", "PL"),
        ],
        "fan-producing community sorts ahead of an alphabetically earlier unmeasured one: {communities:?}"
    );
    assert_eq!(helpers.communities[0].measured_posts_90d, 1);
    assert_eq!(helpers.communities[0].tracked_clicks_90d, Some(1));
    assert_eq!(helpers.communities[0].fans_acquired_90d, Some(1));
    assert_eq!(helpers.communities[0].fans_retained_30d, Some(1));
    assert_eq!(helpers.communities[1].measured_posts_90d, 1);
    assert_eq!(helpers.communities[1].tracked_clicks_90d, Some(1));
    assert_eq!(helpers.communities[1].fans_acquired_90d, Some(0));
    assert_eq!(helpers.communities[1].fans_retained_30d, Some(0));
    assert_eq!(helpers.communities[2].measured_posts_90d, 0);
    assert_eq!(helpers.communities[2].tracked_clicks_90d, None);
    assert_eq!(helpers.communities[2].fans_acquired_90d, None);
    assert_eq!(helpers.communities[2].fans_retained_30d, None);
    sqlx::query("INSERT INTO fan_consents (workspace_id,fan_id,purpose,granted,policy_version,source) VALUES ($1,$2,'marketing',false,'v1','helper-proof')")
        .bind(act).bind(fan_id).execute(pool).await?;
    let withdrawn = who_can_help(pool, act, "friday")
        .await?
        .ok_or("withdrawn helper read missing")?;
    assert_eq!(withdrawn.communities[0].fans_retained_30d, Some(0));

    // cold_rooms — the shared registry minus this tenant's marks. "Klub A"
    // is out because the tenant played it; "Klub B" is IN because the other
    // workspace's mark is not this tenant's knowledge. The capacity fact
    // surfaces globally; the private fact on "No Cap Klub" must not.
    let cold_names: Vec<&str> = helpers
        .cold_rooms
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(
        cold_names.first(),
        Some(&"Cold Klub"),
        "a room with a known capacity sorts first: {cold_names:?}"
    );
    assert_eq!(helpers.cold_rooms[0].capacity.as_deref(), Some("350"));
    assert!(
        cold_names.contains(&"Klub B"),
        "a room only the other workspace played is cold to this one: {cold_names:?}"
    );
    assert!(
        cold_names.contains(&"No Cap Klub"),
        "an unmarked room with no capacity fact still lists: {cold_names:?}"
    );
    assert!(
        !cold_names.contains(&"Klub A"),
        "a room this tenant played is not cold: {cold_names:?}"
    );
    // P.6 — "Klub B" is cold to this tenant but the registry knows the other
    // workspace's night there; "Cold Klub" carries a measured zero.
    let klub_b = helpers
        .cold_rooms
        .iter()
        .find(|room| room.display_name == "Klub B")
        .expect("Klub B lists as cold to this tenant");
    assert_eq!(
        klub_b.venue_prior.map(|p| (p.tenants_played, p.shows)),
        Some((1, 1)),
        "the other workspace's mark is the prior this tenant sees"
    );
    let cold_klub = helpers
        .cold_rooms
        .iter()
        .find(|room| room.display_name == "Cold Klub")
        .expect("Cold Klub lists");
    assert_eq!(
        cold_klub.venue_prior.map(|p| (p.tenants_played, p.shows)),
        Some((0, 0)),
        "a room nobody marked reports a measured zero, not a missing prior"
    );
    assert!(
        !cold_names.contains(&"Berlin Cold"),
        "another city's room is not on this night: {cold_names:?}"
    );
    let no_cap = helpers
        .cold_rooms
        .iter()
        .find(|row| row.display_name == "No Cap Klub")
        .ok_or("No Cap Klub is missing")?;
    assert_eq!(
        no_cap.capacity, None,
        "the other workspace's private fact leaked into a global read"
    );

    // bill_mates — the peer act lists with its shared-bill count; the tenant
    // act and our own do not (their channels are the crossbill edge and
    // nothing, respectively).
    let mates: Vec<(&str, &str, i64)> = helpers
        .bill_mates
        .iter()
        .map(|m| (m.act_name.as_str(), m.resolution.as_str(), m.shared_bills))
        .collect();
    assert_eq!(
        mates,
        // Only the real prior bill counts — this show must not count itself.
        [("The Openers", "peer", 1), ("Mystery Band", "unclaimed", 0)],
        "bill_mates must hold the peer and unclaimed acts only: {mates:?}"
    );
    assert!(
        !helpers.bill_mates[0].on_roster,
        "no beacon carries the name yet"
    );

    // venue_channel — the show's own room, resolved to the registry and
    // matched by the venue-kind beacon of the same name.
    let channel = helpers
        .venue_channel
        .as_ref()
        .ok_or("the room the show is in produced no channel row")?;
    assert_eq!(channel.display_name, "Klub A");
    assert!(channel.venue_id.is_some(), "the registry must resolve it");
    assert!(channel.on_roster, "the venue beacon is the channel");

    // photographers — this city's only, warmest facts surfaced.
    let lenses: Vec<(&str, bool)> = helpers
        .photographers
        .iter()
        .map(|p| (p.display_name.as_str(), p.contacted_before))
        .collect();
    assert_eq!(
        lenses,
        [("Basia Lens", true)],
        "photographers must be this city's, this tenant's: {lenses:?}"
    );

    // The other workspace's read of the same city: its own press, its own
    // booker, its own community — and the cold rooms flip: "Klub A" is cold
    // to it, "Klub B" is not.
    let theirs = who_can_help(pool, other, "friday-too")
        .await?
        .ok_or("the second show produced no helper read")?;
    let their_press: Vec<&str> = theirs
        .press
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(their_press, ["Rival Radio"], "their press: {their_press:?}");
    let their_cold: Vec<&str> = theirs
        .cold_rooms
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert!(
        their_cold.contains(&"Klub A") && !their_cold.contains(&"Klub B"),
        "cold rooms did not flip per workspace: {their_cold:?}"
    );

    // A show with no city degrades: every section empty, "city" named, never
    // an error — a band needs to see the missing city, not a 404.
    let no_city = who_can_help(pool, act, "no-city-yet")
        .await?
        .ok_or("a city-less show produced no read")?;
    assert_eq!(no_city.degraded, ["city"]);
    assert_eq!(no_city.event.city, None);
    assert_eq!(no_city.event.city_id, None);
    assert!(no_city.press.is_empty());
    assert!(no_city.rooms_and_promoters.is_empty());
    assert!(no_city.communities.is_empty());
    assert!(no_city.cold_rooms.is_empty());
    assert!(no_city.photographers.is_empty());
    // The bill and the room are the event's own answers — a missing city
    // does not suppress them. "TBA" resolves to nothing, but still names
    // itself.
    let channel = no_city
        .venue_channel
        .as_ref()
        .ok_or("a city-less show with a venue name must still answer")?;
    assert_eq!(channel.display_name, "TBA");
    assert_eq!(channel.venue_id, None);
    assert_eq!(no_city.notes, ["staged_contacts_have_no_city"]);

    // An event nobody has is not an empty list — and neither is a draft,
    // matching the timeline's resolution exactly.
    assert!(who_can_help(pool, act, "no-such-show").await?.is_none());
    assert!(who_can_help(pool, act, "draft-show").await?.is_none());

    a_promoted_press_contact_keeps_its_city(pool).await?;

    Ok(())
}

/// 3.9 — the city the sheet placed a contact in survives the promote.
///
/// Before this, `agent_outreach_targets` had no city at all: a band playing a
/// city could not ask which of its own press contacts were in it, and the
/// staged city was dropped on the way through.
async fn a_promoted_press_contact_keeps_its_city(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(pool).await?;
    let gdansk = city(pool, "gdansk", "Gdańsk", "PL", 54.35, 18.65).await?;
    let repository = crowdrelay_infra::gdrive::PostgresGDriveRepository::new(pool.clone());

    // The sheet wrote the city in its own words, with its own diacritics.
    let staged = staged_contact(pool, act, "editor@example.com", Some("Gdańsk")).await?;
    let contact = repository.get_contact(act, staged).await?;
    repository.promote_beacon(act, &contact, "press").await?;
    let city_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT city_id FROM agent_outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'editor@example.com'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        city_id,
        Some(gdansk),
        "the staged city did not survive the promote"
    );

    // A contact the sheet placed nowhere promotes with no city rather than
    // being refused — a national title is legitimately placeless, and the
    // booking half's city requirement does not apply here.
    let placeless = staged_contact(pool, act, "national@example.com", None).await?;
    let contact = repository.get_contact(act, placeless).await?;
    repository.promote_beacon(act, &contact, "press").await?;
    let city_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT city_id FROM agent_outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'national@example.com'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(city_id, None, "a placeless contact was given a city");

    // A city name two catalogue rows share resolves to nothing rather than to
    // whichever came first — the booking half's rule, for the same reason:
    // a contact filed in the wrong city is suggested for the wrong shows.
    city(pool, "springfield", "Springfield", "US", 39.79, -89.64).await?;
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ('springfield-ma', 'Springfield', 'US', 42.1, -72.59)",
    )
    .execute(pool)
    .await?;
    let ambiguous = staged_contact(pool, act, "ambiguous@example.com", Some("Springfield")).await?;
    let contact = repository.get_contact(act, ambiguous).await?;
    repository.promote_beacon(act, &contact, "press").await?;
    let city_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT city_id FROM agent_outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'ambiguous@example.com'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        city_id, None,
        "an ambiguous city name was resolved to one of the candidates anyway"
    );

    Ok(())
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Helpers Test')")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn city(
    pool: &PgPool,
    slug: &str,
    name: &str,
    country_code: &str,
    latitude: f64,
    longitude: f64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .bind(slug)
    .bind(name)
    .bind(country_code)
    .bind(latitude)
    .bind(longitude)
    .fetch_one(pool)
    .await?)
}

/// A show row. `venue` names the room so the registry trigger marks it —
/// a published or completed event with a venue and a city is what feeds
/// `place_venues`/`place_venue_marks`, and that is exactly what makes the
/// room warm rather than cold for this tenant.
async fn show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Option<Uuid>,
    slug: &str,
    venue: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, $3, $3, $4, now() + interval '20 days', 'published', now())",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(slug)
    .bind(venue)
    .execute(pool)
    .await?;
    Ok(())
}

async fn press(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Option<Uuid>,
    display_name: &str,
    kind: &str,
    status: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO agent_outreach_targets
            (workspace_id, target_kind, display_name, contact_email, status, city_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(workspace_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!(
        "{}@example.com",
        display_name.to_lowercase().replace(' ', "-")
    ))
    .bind(status)
    .bind(city_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A community outreach target — the communities section's source table.
/// `country_code` is all the placement it carries; there is no city.
async fn community(
    pool: &PgPool,
    workspace_id: Uuid,
    country_code: &str,
    name: &str,
    active: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO community_outreach_targets
            (workspace_id, symbol_slug, community_name, platform, url,
             country_code, active)
         VALUES ($1, $2, $3, 'reddit', $4, $5, $6)
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(name.replace('/', "-"))
    .bind(name)
    .bind(format!("https://www.reddit.com/{name}"))
    .bind(country_code)
    .bind(active)
    .fetch_one(pool)
    .await?)
}

async fn booking_target(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    display_name: &str,
    kind: &str,
    active: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO booking_targets
            (workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score, active)
         VALUES ($1, $2, $3, $4, $5, 60, $6)",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!(
        "{}@example.com",
        display_name.to_lowercase().replace(' ', "-")
    ))
    .bind(active)
    .execute(pool)
    .await?;
    Ok(())
}

/// A registry row directly — the rooms nobody has marked, which is what
/// makes them cold to every workspace rather than just this one.
async fn venue(
    pool: &PgPool,
    city_id: Uuid,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_venues (city_id, name_key, display_name)
         VALUES ($1, place_venue_key($2), $2)
         RETURNING id",
    )
    .bind(city_id)
    .bind(display_name)
    .fetch_one(pool)
    .await?)
}

/// A fact about a room — `None` workspace writes the global record any
/// tenant may read, `Some` writes a private one the global read must skip.
async fn venue_fact(
    pool: &PgPool,
    venue_id: Uuid,
    attribute: &str,
    value: &str,
    workspace_id: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO place_venue_facts
            (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
         VALUES ($1, $2, $3, 'researched', 'test', now(), $4)",
    )
    .bind(venue_id)
    .bind(attribute)
    .bind(value)
    .bind(workspace_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn event_id(
    pool: &PgPool,
    workspace_id: Uuid,
    slug: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM events WHERE workspace_id = $1 AND slug = $2",
        )
        .bind(workspace_id)
        .bind(slug)
        .fetch_one(pool)
        .await?,
    )
}

async fn peer_act(pool: &PgPool, name: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_peer_acts (name_key, display_name)
         VALUES (place_venue_key($1), $1)
         ON CONFLICT (name_key) DO UPDATE SET display_name = EXCLUDED.display_name
         RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await?)
}

/// A beacon row — kind, city and optional email are the axes the P.3
/// sections filter on.
async fn beacon(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    display_name: &str,
    kind: &str,
    email: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO beacons
            (workspace_id, city_id, beacon_kind, display_name, contact_email,
             accepts_outreach)
         VALUES ($1, $2, $3, $4, $5, true)
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(email)
    .fetch_one(pool)
    .await?)
}

async fn staged_contact(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    city: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO drive_contacts
            (workspace_id, normalized_email, display_name, city,
             source_file_id, source_file_name, suggested_kind)
         VALUES ($1, $2, $3, $4, 'file-1', 'contacts.xlsx', 'press')
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(email)
    .bind(email.split('@').next().unwrap_or("contact"))
    .bind(city)
    .fetch_one(pool)
    .await?)
}
