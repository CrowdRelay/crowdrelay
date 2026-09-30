//! Who could help with one particular night (§4h-11, 3.10).
//!
//! # The queue read against a date instead of as an inventory
//!
//! The system already holds hundreds of contacts: press the archive scan
//! staged, rooms a sheet seeded, promoters who book the city, communities that
//! admitted us. As a list nobody opens it — four hundred rows with no reason to
//! read any one of them is how a queue becomes a graveyard.
//!
//! Against a date it is a different object. Friday's show in Wrocław makes a
//! Wrocław paper, a Wrocław promoter and a Wrocław room worth looking at this
//! week specifically, and everything else is correctly absent.
//!
//! # Candidates, never instructions
//!
//! Proximity to a date is a reason to *look*, never a reason to write. Every
//! row here is a suggestion to a person: promotion still stands between a
//! staged contact and any outreach, the contact governor still binds, and the
//! admission wall still binds. That is also why no row carries an email — a
//! candidate list names who, it does not hand out an address; the promote
//! flow is the only door.
//!
//! # Each section degrades on its own
//!
//! Four independent queries answer four independent questions, and one of
//! them failing is not a reason to hide the other three. A section whose
//! query fails comes back empty with its key named in `degraded`, and an
//! event with no city resolves `degraded: ["city"]` with every section empty
//! — without a city there is no local anybody, and answering with the whole
//! contact list is the inventory this read exists to replace.
//!
//! # The honest gap
//!
//! `drive_contacts` rows carry at most a free-text `city` (migration
//! 0295) — not a joinable `city_id`, and "Wroclaw" typed without its
//! diacritic does not equal `cities.name`. The staging queue's raw contacts
//! therefore cannot be city-filtered without pattern-matching prose, which
//! the read refuses to do. The response names the gap instead:
//! `notes: ["staged_contacts_have_no_city"]`.
//!
//! # The cold_rooms read is deliberately global
//!
//! `place_venues` is the shared registry — one row per room no matter which
//! tenant marked it, and it carries no `workspace_id` to scope by. The read
//! returns only what any tenant could see (a name and a public capacity
//! fact): the tenant's own marks exclude the rooms it already knows, and the
//! capacity join filters `workspace_id IS NULL` so a contributor-private fact
//! never surfaces. The global table holds nothing worth isolating — the
//! workspace-scope ratchet records the statement as one of the few that
//! legitimately names no workspace, the same rule venue_fact_expiry's global
//! sweep carries (4V.8).

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// One section's worth of rows is bounded — a shortlist is short by
/// definition, and each query is per-city so the bound is never the
/// interesting part of the answer.
const MAX_HELPERS_PER_SECTION: i64 = 40;

/// The show being asked about — enough for the page to say whose shortlist
/// this is, in the same shape the timeline's event block uses.
#[derive(Debug, Serialize)]
pub struct HelperEvent {
    pub slug: String,
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    /// The city's display name. `null` when the event carries no city —
    /// `degraded` says so plainly rather than implying "nowhere".
    pub city: Option<String>,
    /// The city's country — communities are country-scoped, so the field is
    /// `country_code`, never anything that reads as city-local.
    pub country_code: Option<String>,
    /// The city's id — what an operator action (admit a candidate to the
    /// roster) needs to place a beacon in this city. `null` with `city`.
    /// The id, not the slug: slug resolution reads a top-100 fan-signal
    /// snapshot that low-signal and foreign cities never reach.
    pub city_id: Option<Uuid>,
}

/// An unpromoted press, radio or playlist contact in the show's city.
///
/// No `contact_email` on purpose: a candidate list does not hand out
/// addresses. The promote flow is the only door between this row and a send.
#[derive(Debug, Serialize)]
pub struct PressCandidate {
    pub id: Uuid,
    pub target_kind: String,
    pub display_name: String,
    pub contact_domain: Option<String>,
    pub why_fit: String,
    pub verified: bool,
}

/// An active room or promoter the tenant already targets in this city.
#[derive(Debug, Serialize)]
pub struct RoomCandidate {
    pub id: Uuid,
    pub target_kind: String,
    pub display_name: String,
    pub accepts_booking: bool,
    pub relationship_score: i32,
    /// Whether the target is joined to the shared venue registry — the
    /// difference between "a name on a list" and "a room with a record".
    pub venue_linked: bool,
    /// P.6: the target address's reply record across every tenant —
    /// anonymous counts. `null` means the prior read did not run.
    pub counterparty_prior: Option<crate::cross_tenant_priors::CounterpartyPrior>,
    /// P.6: the linked room's play record — `null` when the target is not
    /// venue-linked or the read did not run.
    pub venue_prior: Option<crate::cross_tenant_priors::VenuePrior>,
}

/// A community the tenant posts in, in the show's country.
#[derive(Debug, Serialize)]
pub struct CommunityCandidate {
    pub id: Uuid,
    pub community_name: String,
    pub platform: String,
    pub url: String,
    pub self_promo_policy: String,
    /// The country the match was made on — named `country`, never `city`,
    /// because communities carry no city and pretending they do would
    /// misreport the granularity.
    pub country: String,
    /// Posts in this community during the last 90 days that carried a tracked
    /// CrowdRelay link. Zero means unmeasured here, so the yield fields below
    /// are `None` rather than fabricated zeroes.
    pub measured_posts_90d: u32,
    /// First-hop tracked clicks from those posts. `None` when the tenant has
    /// not run a measurable post here in the window.
    pub tracked_clicks_90d: Option<u64>,
    /// Distinct owned fans who joined within seven days of those tracked
    /// clicks. This is the North-Star evidence the booker can use without the
    /// system taking the relationship decision away from them.
    pub fans_acquired_90d: Option<u64>,
}

/// A room the shared registry knows in this city that this tenant has never
/// played — "cold" in the literal sense.
#[derive(Debug, Serialize)]
pub struct ColdRoom {
    pub id: Uuid,
    pub display_name: String,
    /// P.6: the room's play record across every tenant — cold for this
    /// band is not cold for the registry. `null` when the read did not run.
    pub venue_prior: Option<crate::cross_tenant_priors::VenuePrior>,
    /// The global capacity fact's raw value, when one exists. Text rather
    /// than a number because the fact itself is text — "350" and "350
    /// standing" are both honest answers, and parsing either into an integer
    /// would invent precision the registry never claimed.
    pub capacity: Option<String>,
}

/// A band on the same bill who is not a tenant — the peer whose crowd is
/// already in the room on our date (P.3). The crossbill edge carries a
/// tenant bill-mate; a peer or unclaimed act is a name only, and the honest
/// candidate row says so — the roster is the only door to contact.
#[derive(Debug, Serialize)]
pub struct BillMate {
    pub act_slug: String,
    pub act_name: String,
    /// Slot order on the bill — running order, headliner last. 0 opens.
    pub position: i32,
    /// `peer` when the name resolved to a `place_peer_acts` row, `unclaimed`
    /// when the resolver has not minted one yet. Tenant acts are omitted
    /// entirely: the crossbill edge is already their channel.
    pub resolution: String,
    /// A beacon already carries this act's name — the difference between a
    /// name on a poster and a contact the pipeline can carry.
    pub on_roster: bool,
    /// Bills this act has shared with this band, tonight included. "We have
    /// shared a stage before" is the warmest fact a candidate row can hold.
    pub shared_bills: i64,
}

/// The room the show is in — the venue's own channel (P.3). `venue_id` is
/// `None` when the registry has not resolved the name; the row still carries
/// the event's own venue text, because a room we cannot name in the registry
/// is still a room we are playing.
#[derive(Debug, Serialize)]
pub struct VenueChannel {
    pub venue_id: Option<Uuid>,
    pub display_name: String,
    /// A `venue`-kind beacon in this city already names this room — the
    /// channel exists on the roster rather than needing to be found.
    pub on_roster: bool,
    /// The room's platform-wide play record — the acts on record that
    /// played it. `None` when the room never matched the registry or the
    /// read failed; `Some` zero is a measured zero.
    pub venue_prior: Option<crate::cross_tenant_priors::VenuePrior>,
}

/// A photographer beacon in the show's city (P.3). The recap needs the lens
/// before the show happens — a photographer nobody contacted is a recap that
/// never exists.
#[derive(Debug, Serialize)]
pub struct PhotographerCandidate {
    pub id: Uuid,
    pub display_name: String,
    pub verified: bool,
    pub relationship_score: i32,
    /// The contact governor remembers a touch — the band has written to them
    /// before, in any context. Warm, not cold.
    pub contacted_before: bool,
}

/// Everybody worth looking at for one night, per the spec's four sections.
#[derive(Debug, Serialize)]
pub struct ShowHelpers {
    pub event: HelperEvent,
    /// What this read could not answer: `"city"` when the event carries no
    /// city, and one key per section whose query failed. Absence of a key
    /// means the section's empty array is a measured answer, not a failure.
    pub degraded: Vec<&'static str>,
    /// The gaps in the read itself, named rather than pattern-matched around.
    /// `staged_contacts_have_no_city` is constant: drive contacts carry a
    /// free-text city at best, which is not a city the read can filter on.
    pub notes: Vec<&'static str>,
    pub press: Vec<PressCandidate>,
    pub rooms_and_promoters: Vec<RoomCandidate>,
    pub communities: Vec<CommunityCandidate>,
    pub cold_rooms: Vec<ColdRoom>,
    /// The other bands on this bill who are not tenants — P.3's bill-mate
    /// side. Empty when the bill was never entered (`event_acts` is operator
    /// data) — that is a real empty, not a missing read.
    pub bill_mates: Vec<BillMate>,
    /// The room itself, when the event names one — the venue's own channel.
    pub venue_channel: Option<VenueChannel>,
    /// Photographer beacons in the show's city.
    pub photographers: Vec<PhotographerCandidate>,
    /// Sections that hit the shortlist cap — a festival bill can carry 500
    /// acts, so "40 shown" must not read as "40 exist".
    pub truncated: Vec<&'static str>,
}

/// Who could help with the show identified by this slug.
///
/// `Ok(None)` when the workspace has no such event — the same resolution the
/// event timeline uses, published and completed shows alike. An event with
/// no city is still `Some`: `degraded` carries `"city"` and every section
/// answers empty, never an error.
///
/// # Errors
///
/// Propagates the database error from the event lookup itself. A section
/// query's error degrades that section instead — the read is a shortlist,
/// and one empty source is not a reason to answer 503 for the other three.
pub async fn who_can_help(
    pool: &PgPool,
    workspace_id: Uuid,
    event_slug: &str,
) -> Result<Option<ShowHelpers>, sqlx::Error> {
    let Some(event) = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            OffsetDateTime,
            Option<Uuid>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        r#"
        SELECT event.id, event.slug, event.title, event.starts_at,
               event.city_id, event.venue, city.name, city.country_code
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1 AND event.slug = $2
          AND event.status IN ('published','completed')
        "#,
    )
    .bind(workspace_id)
    .bind(event_slug)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let (event_id, slug, title, starts_at, city_id, venue_text, city_name, country_code) = event;

    let mut degraded: Vec<&'static str> = Vec::new();
    let mut press = Vec::new();
    let mut rooms_and_promoters = Vec::new();
    let mut communities = Vec::new();
    let mut cold_rooms = Vec::new();
    let mut bill_mates = Vec::new();
    let mut venue_channel = None;
    let mut photographers = Vec::new();
    let mut truncated: Vec<&'static str> = Vec::new();

    // The other bands on this bill (P.3) — event-scoped, not city-scoped,
    // so this answers even when the show has no city yet. A peer or
    // unclaimed act is a name, not an address: `on_roster` marks the ones
    // already carried by a beacon, and the rest are candidates for it.
    // Tenants are omitted — the crossbill edge is already their channel.
    // An act matching this workspace's own listing name is excluded too:
    // an unresolved collision must not offer "add yourself to your roster".
    match sqlx::query_as::<_, (String, String, i32, String, bool, i64)>(
        r#"
        SELECT act.act_slug, act.act_name, act.position,
               CASE WHEN act.peer_act_id IS NOT NULL THEN 'peer'
                    ELSE 'unclaimed' END AS resolution,
               EXISTS (
                   SELECT 1 FROM beacons AS beacon
                   WHERE beacon.workspace_id = $1
                     AND beacon.city_id IS NOT DISTINCT FROM $3
                     AND beacon.beacon_kind = 'scene_partner'
                     AND beacon.active
                     AND place_venue_key(beacon.display_name)
                         = place_venue_key(act.act_name)
               ) AS on_roster,
               (SELECT count(*) FROM event_acts AS other
                 JOIN events AS past ON past.id = other.event_id
                  AND past.status IN ('published','completed')
                 WHERE other.workspace_id = $1
                   AND other.event_id <> act.event_id
                   AND other.peer_act_id = act.peer_act_id
                   AND act.peer_act_id IS NOT NULL)::bigint AS shared_bills
        FROM event_acts AS act
        WHERE act.workspace_id = $1 AND act.event_id = $2
          AND act.act_workspace_id IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM band_listings AS own
              WHERE own.workspace_id = $1
                AND place_venue_key(own.act_name)
                    = place_venue_key(act.act_name)
          )
        ORDER BY act.position, act.act_name
        LIMIT $4
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .bind(city_id)
    .bind(MAX_HELPERS_PER_SECTION + 1)
    .fetch_all(pool)
    .await
    {
        Ok(mut rows) => {
            if rows.len() as i64 > MAX_HELPERS_PER_SECTION {
                rows.truncate(MAX_HELPERS_PER_SECTION as usize);
                truncated.push("bill_mates");
            }
            bill_mates = rows
                .into_iter()
                .map(
                    |(act_slug, act_name, position, resolution, on_roster, shared_bills)| {
                        BillMate {
                            act_slug,
                            act_name,
                            position,
                            resolution,
                            on_roster,
                            shared_bills,
                        }
                    },
                )
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "who-can-help bill_mates section failed");
            degraded.push("bill_mates");
        }
    }

    // The room itself (P.3) — event-scoped like the bill. The registry join
    // needs the city; the beacon check runs either way (a venue beacon may
    // carry no city), so an unresolved room still answers on_roster truly.
    if let Some(venue_text) = venue_text
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        match sqlx::query_as::<_, (Option<Uuid>, String, bool)>(
            r#"
            SELECT venue.id, venue.display_name,
                   EXISTS (
                       SELECT 1 FROM beacons AS beacon
                       WHERE beacon.workspace_id = $1
                         AND beacon.city_id IS NOT DISTINCT FROM $2
                         AND beacon.beacon_kind = 'venue'
                         AND beacon.active
                         AND place_venue_key(beacon.display_name)
                             = place_venue_key($3)
                   ) AS on_roster
            FROM place_venues AS venue
            WHERE venue.city_id = $2
              AND venue.name_key = place_venue_key($3)
            UNION ALL
            SELECT NULL, $3,
                   EXISTS (
                       SELECT 1 FROM beacons AS beacon
                       WHERE beacon.workspace_id = $1
                         AND beacon.city_id IS NOT DISTINCT FROM $2
                         AND beacon.beacon_kind = 'venue'
                         AND beacon.active
                         AND place_venue_key(beacon.display_name)
                             = place_venue_key($3)
                   )
            WHERE NOT EXISTS (
                SELECT 1 FROM place_venues AS venue
                WHERE venue.city_id = $2
                  AND venue.name_key = place_venue_key($3)
            )
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .bind(city_id)
        .bind(venue_text)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((venue_id, display_name, on_roster))) => {
                venue_channel = Some(VenueChannel {
                    venue_id,
                    display_name,
                    on_roster,
                    venue_prior: None,
                });
            }
            // The UNION guarantees a row whenever venue text exists; a None
            // is unreachable but still answered honestly rather than hidden.
            Ok(None) => {
                venue_channel = Some(VenueChannel {
                    venue_id: None,
                    display_name: venue_text.to_owned(),
                    on_roster: false,
                    venue_prior: None,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "who-can-help venue_channel section failed");
                degraded.push("venue_channel");
            }
        }

        // The room's play record rides the channel — the same counts the
        // cold-rooms list carries, on the room the band actually booked.
        // A failed read degrades to `None` like every prior does.
        if let Some(venue_id) = venue_channel.as_ref().and_then(|channel| channel.venue_id) {
            let prior = crate::cross_tenant_priors::venue_priors(pool, &[venue_id])
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "who-can-help venue-channel prior failed");
                    error
                })
                .ok();
            if let Some(channel) = venue_channel.as_mut() {
                channel.venue_prior =
                    prior.map(|priors| priors.get(&venue_id).copied().unwrap_or_default());
            }
        }
    }

    let Some(city_id) = city_id else {
        // No city means no local anybody for the city-scoped sections — but
        // the bill and the room already answered above. `degraded` still
        // names "city": the remaining sections are honestly empty because
        // of it.
        degraded.push("city");
        return Ok(Some(ShowHelpers {
            event: HelperEvent {
                slug,
                title,
                starts_at,
                city: city_name,
                country_code,
                city_id: None,
            },
            degraded,
            notes: vec!["staged_contacts_have_no_city"],
            press,
            rooms_and_promoters,
            communities,
            cold_rooms,
            bill_mates,
            venue_channel,
            photographers,
            truncated,
        }));
    };

    // Proposed press in this city — the staged half of the pipeline, which is
    // the half nobody sees anywhere else. The kind list is the media set the
    // CHECK constraint allows: 'webzine' is not in it ('press','radio',
    // 'playlist','media_patronage','endorsement','creator' is the constraint's
    // whole vocabulary), and endorsement/creator targets are not press —
    // a creator is a voice the autopilot's own outreach already owns.
    match sqlx::query_as::<_, (Uuid, String, String, Option<String>, String, bool)>(
        r#"
        SELECT id, target_kind, display_name, contact_domain, why_fit, verified
        FROM agent_outreach_targets
        WHERE workspace_id = $1 AND city_id = $2 AND status = 'proposed'
          AND target_kind IN ('press','radio','playlist','media_patronage')
        ORDER BY display_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(MAX_HELPERS_PER_SECTION + 1)
    .fetch_all(pool)
    .await
    {
        Ok(mut rows) => {
            if rows.len() as i64 > MAX_HELPERS_PER_SECTION {
                rows.truncate(MAX_HELPERS_PER_SECTION as usize);
                truncated.push("press");
            }
            press = rows
                .into_iter()
                .map(
                    |(id, target_kind, display_name, contact_domain, why_fit, verified)| {
                        PressCandidate {
                            id,
                            target_kind,
                            display_name,
                            contact_domain,
                            why_fit,
                            verified,
                        }
                    },
                )
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "who-can-help press section failed");
            degraded.push("press");
        }
    }

    // The tenant's own booking targets in this city — active only. A venue
    // target joined to the registry (`venue_id`) carries the room's record
    // with it; the flag is what the page shows, not the join.
    // contact_email and venue_id are fetch-only: they key the P.6 prior
    // lookups and must not serialize — who-can-help never hands out an
    // address, and the room's record is the prior, not the join key.
    match sqlx::query_as::<_, (Uuid, String, String, bool, i32, bool, String, Option<Uuid>)>(
        r#"
        SELECT id, target_kind, display_name, accepts_booking,
               relationship_score, (venue_id IS NOT NULL) AS venue_linked,
               contact_email, venue_id
        FROM booking_targets
        WHERE workspace_id = $1 AND city_id = $2 AND active
          -- A venue-kind target IS its room; a room on record as closed is
          -- not somebody who can help. The resolved status decides — a newer
          -- 'active' claim lifts the exclusion.
          AND NOT (
              target_kind = 'venue'
              AND EXISTS (
                  SELECT 1
                  FROM (
                      SELECT booking_targets.venue_id AS linked_venue_id
                      UNION
                      SELECT edge.venue_id
                      FROM booking_target_venues AS edge
                      WHERE edge.workspace_id = booking_targets.workspace_id
                        AND edge.target_id = booking_targets.id
                  ) AS linked
                  WHERE COALESCE((
                      SELECT lower(btrim(status_fact.value))
                      FROM place_venue_facts AS status_fact
                      WHERE status_fact.venue_id = linked.linked_venue_id
                        AND status_fact.attribute = 'status'
                        AND (status_fact.workspace_id IS NULL
                             OR status_fact.workspace_id = $1)
                        AND (status_fact.expires_at IS NULL
                             OR status_fact.expires_at > now())
                      ORDER BY CASE status_fact.provenance
                                   WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                                   WHEN 'event_evidence' THEN 2
                                   WHEN 'open_directory' THEN 3 ELSE 4 END,
                               status_fact.observed_at DESC
                      LIMIT 1
                  ), '') = 'closed'
              )
          )
        ORDER BY relationship_score DESC, display_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(MAX_HELPERS_PER_SECTION + 1)
    .fetch_all(pool)
    .await
    {
        Ok(mut rows) => {
            if rows.len() as i64 > MAX_HELPERS_PER_SECTION {
                rows.truncate(MAX_HELPERS_PER_SECTION as usize);
                truncated.push("rooms_and_promoters");
            }
            let emails: Vec<String> = rows
                .iter()
                .map(|row| row.6.to_lowercase().trim().to_owned())
                .collect();
            let venue_ids: Vec<Uuid> = rows.iter().filter_map(|row| row.7).collect();
            let counterparty_priors =
                crate::cross_tenant_priors::counterparty_priors(pool, &emails)
                    .await
                    .map_err(|error| {
                        tracing::warn!(%error, "who-can-help promoter priors failed");
                        error
                    })
                    .ok();
            let venue_priors = crate::cross_tenant_priors::venue_priors(pool, &venue_ids)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "who-can-help venue priors failed");
                    error
                })
                .ok();
            rooms_and_promoters = rows
                .into_iter()
                .map(
                    |(
                        id,
                        target_kind,
                        display_name,
                        accepts_booking,
                        score,
                        venue_linked,
                        email,
                        venue_id,
                    )| {
                        RoomCandidate {
                            id,
                            target_kind,
                            display_name,
                            accepts_booking,
                            relationship_score: score,
                            venue_linked,
                            counterparty_prior: counterparty_priors.as_ref().map(|priors| {
                                priors
                                    .get(&email.to_lowercase().trim().to_owned())
                                    .copied()
                                    .unwrap_or_default()
                            }),
                            // A failed prior read is `None` — never a
                            // measured-looking zero nobody measured.
                            venue_prior: venue_id.and_then(|id| {
                                venue_priors
                                    .as_ref()
                                    .map(|priors| priors.get(&id).copied().unwrap_or_default())
                            }),
                        }
                    },
                )
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "who-can-help rooms_and_promoters section failed");
            degraded.push("rooms_and_promoters");
        }
    }

    // Communities are matched on the city's country — the honest
    // granularity, since a community carries a `country_code` and nothing
    // finer. `country` is the field name in the response so nobody mistakes
    // the match for city-local.
    match sqlx::query_as::<_, (Uuid, String, String, String, String, String, i64, i64, i64)>(
        r#"
        SELECT target.id, target.community_name, target.platform, target.url,
               target.self_promo_policy, target.country_code,
               COALESCE(yield.measured_posts, 0)::bigint,
               COALESCE(yield.tracked_clicks, 0)::bigint,
               COALESCE(yield.fans_acquired, 0)::bigint
        FROM community_outreach_targets AS target
        LEFT JOIN LATERAL (
            SELECT
                COUNT(DISTINCT post.id)::bigint AS measured_posts,
                COUNT(DISTINCT click.id)::bigint AS tracked_clicks,
                COUNT(DISTINCT acquisition.fan_id)::bigint AS fans_acquired
            FROM community_posts AS post
            JOIN smart_links AS link
              ON link.workspace_id = post.workspace_id
             AND post.smart_link = '/l/' || link.slug
            LEFT JOIN click_events AS click
              ON click.workspace_id = link.workspace_id
             AND click.smart_link_id = link.id
             AND click.occurred_at >= post.posted_at
             AND click.occurred_at >= now() - INTERVAL '90 days'
            LEFT JOIN fan_acquisition_events AS acquisition
              ON acquisition.workspace_id = click.workspace_id
             AND acquisition.anonymous_visitor_id = click.anonymous_visitor_id
             AND acquisition.occurred_at >= click.occurred_at
             AND acquisition.occurred_at < click.occurred_at + INTERVAL '7 days'
            WHERE post.workspace_id = target.workspace_id
              AND post.target_id = target.id
              AND post.status = 'posted'
              AND post.posted_at >= now() - INTERVAL '90 days'
        ) AS yield ON true
        WHERE target.workspace_id = $1
          AND target.country_code = $2
          AND target.active
        ORDER BY
            COALESCE(yield.fans_acquired, 0) DESC,
            COALESCE(yield.tracked_clicks, 0) DESC,
            target.priority DESC,
            target.community_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(country_code.as_deref().unwrap_or(""))
    .bind(MAX_HELPERS_PER_SECTION + 1)
    .fetch_all(pool)
    .await
    {
        Ok(mut rows) => {
            if rows.len() as i64 > MAX_HELPERS_PER_SECTION {
                rows.truncate(MAX_HELPERS_PER_SECTION as usize);
                truncated.push("communities");
            }
            communities = rows
                .into_iter()
                .map(
                    |(
                        id,
                        community_name,
                        platform,
                        url,
                        self_promo_policy,
                        country,
                        measured_posts,
                        tracked_clicks,
                        fans_acquired,
                    )| {
                        let measured_posts_90d = u32::try_from(measured_posts).unwrap_or(u32::MAX);
                        CommunityCandidate {
                            id,
                            community_name,
                            platform,
                            url,
                            self_promo_policy,
                            country,
                            measured_posts_90d,
                            tracked_clicks_90d: (measured_posts_90d > 0)
                                .then_some(tracked_clicks.max(0) as u64),
                            fans_acquired_90d: (measured_posts_90d > 0)
                                .then_some(fans_acquired.max(0) as u64),
                        }
                    },
                )
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "who-can-help communities section failed");
            degraded.push("communities");
        }
    }

    // The rooms the registry knows in this city that this tenant has never
    // played. `place_venues` is the global table — it carries no
    // `workspace_id` at all, and the read returns only what any tenant could
    // see (name, public capacity). The tenant's own marks exclude rooms it
    // already knows; the capacity join is `workspace_id IS NULL` only, so a
    // contributor's private fact (a booking address, a fit judgement) can
    // never surface — the same resolved-fact read audience's city_venues
    // uses. Ordering puts rooms with a known capacity first: "how many can
    // fit" is the first question a cold room has to answer.
    match sqlx::query_as::<_, (Uuid, String, Option<String>)>(
        r#"
        SELECT venue.id, venue.display_name, cap.value AS capacity
        FROM place_venues AS venue
        LEFT JOIN LATERAL (
            SELECT fact.value
            FROM place_venue_facts AS fact
            WHERE fact.venue_id = venue.id
              AND fact.workspace_id IS NULL
              AND fact.attribute = 'capacity'
              AND (fact.expires_at IS NULL OR fact.expires_at > now())
            ORDER BY CASE fact.provenance
                         WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                         WHEN 'event_evidence' THEN 2
                         WHEN 'open_directory' THEN 3 ELSE 4 END,
                     fact.observed_at DESC
            LIMIT 1
        ) AS cap ON true
        WHERE venue.city_id = $1
          AND NOT EXISTS (
              SELECT 1
              FROM place_venue_marks AS mark
              WHERE mark.venue_id = venue.id AND mark.workspace_id = $2
          )
          -- A room on record as closed is not a cold room — it is a dead
          -- one, and "who can help" must not suggest writing to it. The
          -- resolved status decides: a newer 'active' claim lifts the
          -- exclusion, so a reopened room is suggested again.
          AND COALESCE((
              SELECT lower(btrim(status_fact.value))
              FROM place_venue_facts AS status_fact
              WHERE status_fact.venue_id = venue.id
                AND status_fact.attribute = 'status'
                AND (status_fact.workspace_id IS NULL
                     OR status_fact.workspace_id = $2)
                AND (status_fact.expires_at IS NULL
                     OR status_fact.expires_at > now())
              ORDER BY CASE status_fact.provenance
                           WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                           WHEN 'event_evidence' THEN 2
                           WHEN 'open_directory' THEN 3 ELSE 4 END,
                       status_fact.observed_at DESC
              LIMIT 1
          ), '') <> 'closed'
        ORDER BY (cap.value IS NULL), venue.display_name
        LIMIT $3
        "#,
    )
    .bind(city_id)
    .bind(workspace_id)
    .bind(MAX_HELPERS_PER_SECTION + 1)
    .fetch_all(pool)
    .await
    {
        Ok(mut rows) => {
            if rows.len() as i64 > MAX_HELPERS_PER_SECTION {
                rows.truncate(MAX_HELPERS_PER_SECTION as usize);
                truncated.push("cold_rooms");
            }
            // "Cold" names this band's history, not the room's — the
            // marks aggregate is the comparables record.
            let venue_ids: Vec<Uuid> = rows.iter().map(|(id, _, _)| *id).collect();
            let venue_priors = crate::cross_tenant_priors::venue_priors(pool, &venue_ids)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "who-can-help cold-room priors failed");
                    error
                })
                .ok();
            cold_rooms = rows
                .into_iter()
                .map(|(id, display_name, capacity)| ColdRoom {
                    id,
                    display_name,
                    capacity,
                    venue_prior: venue_priors
                        .as_ref()
                        .map(|priors| priors.get(&id).copied().unwrap_or_default()),
                })
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "who-can-help cold_rooms section failed");
            degraded.push("cold_rooms");
        }
    }

    // The local photographer (P.3): beacons of that kind in this city,
    // warmest first. The governor read names who has heard from us before.
    match sqlx::query_as::<_, (Uuid, String, bool, i32, bool)>(
        r#"
        SELECT beacon.id, beacon.display_name, beacon.verified,
               beacon.relationship_score,
               EXISTS (
                   SELECT 1 FROM contact_governor AS governor
                   WHERE governor.workspace_id = beacon.workspace_id
                     AND beacon.contact_email IS NOT NULL
                     AND governor.normalized_contact
                         = lower(btrim(beacon.contact_email))
               ) AS contacted_before
        FROM beacons AS beacon
        WHERE beacon.workspace_id = $1 AND beacon.city_id = $2
          AND beacon.beacon_kind = 'photographer'
          AND beacon.active AND NOT beacon.do_not_contact
        ORDER BY beacon.relationship_score DESC, beacon.display_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(MAX_HELPERS_PER_SECTION + 1)
    .fetch_all(pool)
    .await
    {
        Ok(mut rows) => {
            if rows.len() as i64 > MAX_HELPERS_PER_SECTION {
                rows.truncate(MAX_HELPERS_PER_SECTION as usize);
                truncated.push("photographers");
            }
            photographers = rows
                .into_iter()
                .map(
                    |(id, display_name, verified, relationship_score, contacted_before)| {
                        PhotographerCandidate {
                            id,
                            display_name,
                            verified,
                            relationship_score,
                            contacted_before,
                        }
                    },
                )
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "who-can-help photographers section failed");
            degraded.push("photographers");
        }
    }

    Ok(Some(ShowHelpers {
        event: HelperEvent {
            slug,
            title,
            starts_at,
            city: city_name,
            country_code,
            city_id: Some(city_id),
        },
        degraded,
        notes: vec!["staged_contacts_have_no_city"],
        press,
        rooms_and_promoters,
        communities,
        cold_rooms,
        bill_mates,
        venue_channel,
        photographers,
        truncated,
    }))
}
