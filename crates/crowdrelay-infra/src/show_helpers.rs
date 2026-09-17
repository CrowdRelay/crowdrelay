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
//! Proximity to a date is a reason to *look*, never a reason to write.
//! Everything here is a suggestion to a person: promotion still stands between
//! a staged contact and any outreach, the contact governor still binds, the
//! admission wall still binds, and a `do_not_contact` is reported as the
//! blocked state rather than filtered out silently — a band that cannot see
//! why somebody is missing will ask for them again next month.
//!
//! # What is honestly absent
//!
//! Communities carry a country and no city (`discovery_places` records
//! `country_code` and nothing finer), so this read cannot place them. Rather
//! than guess from a name, it counts them and says so: `communities_unplaced`
//! is the number a city read would gain if the place graph ever carried one.
//! Contacts whose own city is unknown are counted the same way — a national
//! magazine belongs to no city and is not evidence of a broken import.
//!
//! Both counts come from `agent_outreach_targets`, split by whether the row
//! carries a `place_id`: a community is a target attached to a place, a press
//! contact is one that is not. One vocabulary, so the two numbers cannot
//! overlap and neither can silently be zero.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One person or place that could help with this night.
#[derive(Clone, Debug, Serialize)]
pub struct ShowHelper {
    /// `press`, `radio`, `creator`, `promoter`, `venue`, `festival` — the
    /// vocabulary of whichever list this came from, unchanged.
    pub kind: String,
    pub display_name: String,
    /// What this row is doing on the list, in the reader's language.
    pub why: String,
    /// `ready` — already promoted, contactable, nothing blocking.
    /// `needs_review` — staged; a person must promote it before anything can
    /// be written.
    /// `blocked` — promoted and not contactable, with `blocked_reason` saying
    /// which rule.
    pub state: HelperState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// The row's own id in the list it came from, so the console can open it.
    pub id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperState {
    Ready,
    NeedsReview,
    Blocked,
}

/// Everybody worth looking at for one night, and what this read could not
/// place.
#[derive(Clone, Debug, Serialize)]
pub struct ShowHelpers {
    pub city: String,
    pub city_id: Uuid,
    /// Local press, radio and creators — promoted and staged alike.
    pub voices: Vec<ShowHelper>,
    /// People who book in this city: promoters, venues, festivals.
    pub bookers: Vec<ShowHelper>,
    /// Press contacts we hold but cannot place in any city. Not a fault —
    /// a national title has no city — and reported so the number is not
    /// mistaken for a gap in the import.
    pub contacts_unplaced: u32,
    /// Communities we hold and cannot place, because the place graph records
    /// a country and no city. The number a city read would gain.
    pub communities_unplaced: u32,
}

/// Who could help with the show identified by this slug.
///
/// `Ok(None)` when the workspace has no such event. An event with no city on
/// it produces `Ok(None)` too: without a city there is no local anybody, and
/// answering with the whole contact list is the inventory this read exists to
/// replace.
///
/// # Errors
///
/// Propagates the database error.
pub async fn who_can_help(
    pool: &PgPool,
    workspace_id: Uuid,
    event_slug: &str,
) -> Result<Option<ShowHelpers>, sqlx::Error> {
    let Some((city_id, city)) = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT city.id, city.name
        FROM events AS event
        JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1 AND event.slug = $2
        "#,
    )
    .bind(workspace_id)
    .bind(event_slug)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let voices = local_voices(pool, workspace_id, city_id).await?;
    let bookers = local_bookers(pool, workspace_id, city_id, &city).await?;

    // Two counts, one query, over one vocabulary. The earlier version asked
    // `discovery_places` for `status = 'admitted'`, and that column's CHECK is
    // `active | archived | blocked` — the filter matched nothing, so the
    // console reported "0 communities we cannot place" however many the
    // workspace held. A count that can only be zero is worse than no count:
    // it reads as a measured absence.
    //
    // A community is an outreach target carrying a `place_id`; a press
    // contact is one without. Counted apart so neither number includes the
    // other, and both mean what their name says.
    let (contacts_unplaced, communities_unplaced) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT count(*) FILTER (WHERE place_id IS NULL)::bigint,
               count(*) FILTER (WHERE place_id IS NOT NULL)::bigint
        FROM agent_outreach_targets
        WHERE workspace_id = $1
          AND city_id IS NULL
          AND status <> 'discarded'
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    Ok(Some(ShowHelpers {
        city,
        city_id,
        voices,
        bookers,
        contacts_unplaced: bounded(contacts_unplaced),
        communities_unplaced: bounded(communities_unplaced),
    }))
}

/// Press, radio and creators placed in this city.
///
/// Both halves of the pipeline: a promoted target that can be written to, and
/// a proposed one that a person has not reviewed yet. The second is the larger
/// half in practice, and hiding it would make the list look empty in exactly
/// the workspaces where the archive scan has been doing its job.
async fn local_voices(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
) -> Result<Vec<ShowHelper>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, String, bool, bool, Option<String>)>(
        r#"
        SELECT id, target_kind, display_name, status,
               accepts_outreach, do_not_contact, NULLIF(btrim(why_fit), '')
        FROM agent_outreach_targets
        WHERE workspace_id = $1
          AND city_id = $2
          AND status <> 'discarded'
        -- Contactable first, and by an explicit rank rather than by the
        -- status text: 'promoted' sorts *before* 'proposed' alphabetically, so
        -- `ORDER BY status DESC` put every staged row above every ready one —
        -- and with the limit applied after the sort, a workspace with forty
        -- staged contacts would have shown none of the ones it can actually
        -- write to.
        ORDER BY CASE WHEN status = 'promoted' THEN 0 ELSE 1 END, display_name
        LIMIT 40
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, kind, display_name, status, accepts, do_not_contact, why_fit)| {
                let (state, blocked_reason) = if status != "promoted" {
                    (HelperState::NeedsReview, None)
                } else if do_not_contact {
                    (
                        HelperState::Blocked,
                        Some("they asked not to be contacted".to_owned()),
                    )
                } else if accepts {
                    (HelperState::Ready, None)
                } else {
                    (
                        HelperState::Blocked,
                        Some(
                            "no published route says they take approaches — find one before \
                             writing"
                                .to_owned(),
                        ),
                    )
                };
                ShowHelper {
                    why: why_fit.unwrap_or_else(|| {
                        format!("{kind} in this city, on file from the archive")
                    }),
                    kind,
                    display_name,
                    state,
                    blocked_reason,
                    id,
                }
            },
        )
        .collect())
}

/// Promoters, venues and festivals who book in this city.
///
/// The promoted list and the staged one again. A booking candidate that has
/// not been confirmed is exactly the row an operator should see the week a
/// show goes on sale, and it is invisible on every other screen.
async fn local_bookers(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    city: &str,
) -> Result<Vec<ShowHelper>, sqlx::Error> {
    let mut helpers = Vec::new();

    let targets = sqlx::query_as::<_, (Uuid, String, String, bool, bool, bool, i32)>(
        r#"
        SELECT target.id, target.target_kind, target.display_name,
               target.active, target.accepts_booking,
               -- The do-not-contact lives on the address, not on the booking
               -- row: it is the contact governor's, and it binds every act in
               -- the organisation. Reading the target's own flags alone would
               -- offer somebody the governor will refuse at dispatch.
               EXISTS (
                   SELECT 1 FROM viryaos_contact_governor AS governor
                   WHERE governor.workspace_id = target.workspace_id
                     AND governor.normalized_contact = lower(btrim(target.contact_email))
                     AND governor.do_not_contact
               ) AS do_not_contact,
               target.relationship_score
        FROM viryaos_booking_targets AS target
        WHERE target.workspace_id = $1 AND target.city_id = $2
        ORDER BY target.relationship_score DESC, target.display_name
        LIMIT 40
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .fetch_all(pool)
    .await?;
    for (id, kind, display_name, active, accepts_booking, do_not_contact, score) in targets {
        let (state, blocked_reason) = if do_not_contact {
            (
                HelperState::Blocked,
                Some("they asked not to be contacted".to_owned()),
            )
        } else if !active {
            (
                HelperState::Blocked,
                Some("marked inactive — check the contact still works".to_owned()),
            )
        } else if accepts_booking {
            (HelperState::Ready, None)
        } else {
            (
                HelperState::Blocked,
                Some("not marked as taking booking approaches".to_owned()),
            )
        };
        helpers.push(ShowHelper {
            why: format!("books in {city}, relationship {score}/100"),
            kind,
            display_name,
            state,
            blocked_reason,
            id,
        });
    }

    // Staged rooms and promoters the sheet or the archive scan produced. Keyed
    // by slug rather than id because the candidate table carries the city as
    // text — the seed's own column, resolved at promotion rather than at
    // staging.
    // Matched on this city's own slug, which is all a staging row carries.
    //
    // The catalogue is unique on (country_code, slug), so a row staged for a
    // namesake in another country matches here too, and this layer cannot
    // tell them apart — the sheet never said which country it meant. That is
    // why these are `needs_review` and not `ready`: promotion resolves the
    // city properly (`promote_beacon_booking` refuses an ambiguous name
    // outright), and nothing is written to a staged row until it does.
    let candidates = sqlx::query_as::<_, (Uuid, String, String, String)>(
        r#"
        SELECT candidate.id, candidate.target_kind, candidate.display_name, candidate.status
        FROM viryaos_booking_candidates AS candidate
        WHERE candidate.workspace_id = $1
          AND candidate.city_slug = (SELECT slug FROM cities WHERE id = $2)
          AND candidate.promoted_at IS NULL
          AND candidate.status <> 'refused'
        ORDER BY candidate.display_name
        LIMIT 40
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .fetch_all(pool)
    .await?;
    for (id, kind, display_name, status) in candidates {
        helpers.push(ShowHelper {
            why: format!("staged {kind} for {city} — {status}, nobody has confirmed it yet"),
            kind,
            display_name,
            state: HelperState::NeedsReview,
            blocked_reason: None,
            id,
        });
    }

    Ok(helpers)
}

fn bounded(value: i64) -> u32 {
    u32::try_from(value.clamp(0, i64::from(u32::MAX))).unwrap_or(u32::MAX)
}
