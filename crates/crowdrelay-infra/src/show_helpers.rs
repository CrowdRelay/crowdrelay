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
//! `viryaos_drive_contacts` rows carry at most a free-text `city` (migration
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
}

/// A room the shared registry knows in this city that this tenant has never
/// played — "cold" in the literal sense.
#[derive(Debug, Serialize)]
pub struct ColdRoom {
    pub id: Uuid,
    pub display_name: String,
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
    /// Slot order on the bill — 0 headlines by convention the sheet uses.
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

    let Some(city_id) = city_id else {
        // No city means no local anybody — and no country either, since the
        // country is the city's own. Every section is honestly empty.
        degraded.push("city");
        return Ok(Some(ShowHelpers {
            event: HelperEvent {
                slug,
                title,
                starts_at,
                city: city_name,
                country_code,
            },
            degraded,
            notes: vec!["staged_contacts_have_no_city"],
            press,
            rooms_and_promoters,
            communities,
            cold_rooms,
            // Bill-mates are the bill's own answer, not the city's — but
            // with no city the whole card is honestly empty by the same
            // "no local anybody" rule the read already makes.
            bill_mates,
            venue_channel,
            photographers,
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
    .bind(MAX_HELPERS_PER_SECTION)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
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
    match sqlx::query_as::<_, (Uuid, String, String, bool, i32, bool)>(
        r#"
        SELECT id, target_kind, display_name, accepts_booking,
               relationship_score, (venue_id IS NOT NULL) AS venue_linked
        FROM viryaos_booking_targets
        WHERE workspace_id = $1 AND city_id = $2 AND active
        ORDER BY relationship_score DESC, display_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(MAX_HELPERS_PER_SECTION)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            rooms_and_promoters = rows
                .into_iter()
                .map(
                    |(id, target_kind, display_name, accepts_booking, score, venue_linked)| {
                        RoomCandidate {
                            id,
                            target_kind,
                            display_name,
                            accepts_booking,
                            relationship_score: score,
                            venue_linked,
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
    match sqlx::query_as::<_, (Uuid, String, String, String, String, String)>(
        r#"
        SELECT id, community_name, platform, url, self_promo_policy, country_code
        FROM viryaos_community_outreach_targets
        WHERE workspace_id = $1 AND country_code = $2 AND active
        ORDER BY priority DESC, community_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(country_code.as_deref().unwrap_or(""))
    .bind(MAX_HELPERS_PER_SECTION)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            communities = rows
                .into_iter()
                .map(
                    |(id, community_name, platform, url, self_promo_policy, country)| {
                        CommunityCandidate {
                            id,
                            community_name,
                            platform,
                            url,
                            self_promo_policy,
                            country,
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
        ORDER BY (cap.value IS NULL), venue.display_name
        LIMIT $3
        "#,
    )
    .bind(city_id)
    .bind(workspace_id)
    .bind(MAX_HELPERS_PER_SECTION)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            cold_rooms = rows
                .into_iter()
                .map(|(id, display_name, capacity)| ColdRoom {
                    id,
                    display_name,
                    capacity,
                })
                .collect();
        }
        Err(error) => {
            tracing::warn!(%error, "who-can-help cold_rooms section failed");
            degraded.push("cold_rooms");
        }
    }

    // The other bands on this bill (P.3) — the side nobody contacts. A peer
    // or unclaimed act is a name, not an address: `on_roster` marks the ones
    // already carried by a beacon, and the rest are candidates for it.
    // Tenants are omitted — the crossbill edge is already their channel.
    match sqlx::query_as::<_, (String, String, i32, String, bool, i64)>(
        r#"
        SELECT act.act_slug, act.act_name, act.position,
               CASE WHEN act.peer_act_id IS NOT NULL THEN 'peer'
                    ELSE 'unclaimed' END AS resolution,
               EXISTS (
                   SELECT 1 FROM viryaos_beacons AS beacon
                   WHERE beacon.workspace_id = $1
                     AND beacon.active
                     AND place_venue_key(beacon.display_name)
                         = place_venue_key(act.act_name)
               ) AS on_roster,
               (SELECT count(*) FROM event_acts AS other
                 WHERE other.workspace_id = $1
                   AND other.peer_act_id = act.peer_act_id
                   AND act.peer_act_id IS NOT NULL)::bigint AS shared_bills
        FROM event_acts AS act
        WHERE act.workspace_id = $1 AND act.event_id = $2
          AND act.act_workspace_id IS NULL
        ORDER BY act.position, act.act_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .bind(MAX_HELPERS_PER_SECTION)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
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

    // The room itself (P.3) — the venue's own channel. The event's venue
    // text resolves against the shared registry the same way the upload
    // match does; unresolved still names the room the operator typed.
    if let Some(venue_text) = venue_text
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        match sqlx::query_as::<_, (Uuid, String, bool)>(
            r#"
            SELECT venue.id, venue.display_name,
                   EXISTS (
                       SELECT 1 FROM viryaos_beacons AS beacon
                       WHERE beacon.workspace_id = $1
                         AND beacon.city_id = $2
                         AND beacon.beacon_kind = 'venue'
                         AND beacon.active
                         AND place_venue_key(beacon.display_name)
                             = venue.name_key
                   ) AS on_roster
            FROM place_venues AS venue
            WHERE venue.city_id = $2
              AND venue.name_key = place_venue_key($3)
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
                    venue_id: Some(venue_id),
                    display_name,
                    on_roster,
                });
            }
            // Not on the registry yet — the room is still the room.
            Ok(None) => {
                venue_channel = Some(VenueChannel {
                    venue_id: None,
                    display_name: venue_text.to_owned(),
                    on_roster: false,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "who-can-help venue_channel section failed");
                degraded.push("venue_channel");
            }
        }
    }

    // The local photographer (P.3): beacons of that kind in this city,
    // warmest first. The governor read names who has heard from us before.
    match sqlx::query_as::<_, (Uuid, String, bool, i32, bool)>(
        r#"
        SELECT beacon.id, beacon.display_name, beacon.verified,
               beacon.relationship_score,
               EXISTS (
                   SELECT 1 FROM viryaos_contact_governor AS governor
                   WHERE governor.workspace_id = beacon.workspace_id
                     AND beacon.contact_email IS NOT NULL
                     AND governor.normalized_contact
                         = lower(btrim(beacon.contact_email))
               ) AS contacted_before
        FROM viryaos_beacons AS beacon
        WHERE beacon.workspace_id = $1 AND beacon.city_id = $2
          AND beacon.beacon_kind = 'photographer'
          AND beacon.active
        ORDER BY beacon.relationship_score DESC, beacon.display_name
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(MAX_HELPERS_PER_SECTION)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
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
    }))
}
