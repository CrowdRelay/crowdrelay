//! The shared night's storage — `place_events`, its contributions, and the
//! organiser link (§12-9, Sprint 4V.6b).
//!
//! The policy lives in `crowdrelay_domain::night`: the lens derivation and
//! the contribution shapes. This file is what a domain cannot do — resolve
//! the caller's relationship against the schema and persist what they
//! publish.
//!
//! # The boundary is the read, not a filter over it
//!
//! Every payload is assembled for one lens, and the parts another lens may
//! never see are never selected for it. The organiser's `combined_reachable`
//! and `payout_total_minor` are sums over contributed rows — the per-
//! workspace parts that fed them stay inside this file, and no field on the
//! organiser view can name one act's terms. That is the proof a billed band
//! needs: the promoter cannot see its guarantee because the guarantee is
//! never projected onto the organiser's night.
//!
//! `place_events` and `place_event_links` are global by design — the night
//! belongs to no workspace — while `place_event_contributions` carries the
//! contributor's `workspace_id` on every row, so every statement here names
//! the workspace it read or wrote for.

use std::collections::{BTreeMap, BTreeSet};

use crowdrelay_domain::night::{ContributionKind, NightLens, derive_lens};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use thiserror::Error;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// The organiser link's lifetime — long enough to survive the booking
/// conversation, short enough that a forgotten forward is not a credential.
const ORGANISER_LINK_DAYS: i32 = 30;

#[derive(Debug, Error)]
pub enum NightError {
    #[error("night database operation failed")]
    Database(#[from] sqlx::Error),
    /// The night does not exist, or the caller has no relationship to it —
    /// the two are deliberately the same answer.
    #[error("night not found for this caller")]
    NotFound,
}

/// The repository over the shared-night tables.
#[derive(Clone)]
pub struct PostgresNightRepository {
    pool: PgPool,
}

/// `skip_serializing_if` takes a reference; `Not::not` does not.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One billed act as the night reads it — the resolved display name, the
/// bill's own ordering, and whether the act's own workspace confirmed it is
/// really on this bill.
#[derive(Clone, Debug, Serialize)]
pub struct NightActView {
    pub act_slug: String,
    pub name: String,
    pub position: i32,
    /// True only when the bill row's `confirmed_by` is the act's own
    /// workspace — a tenant's claim about somebody else's act stays `false`
    /// until the named side says so.
    pub confirmed: bool,
    /// True when the act resolves to the reader's own workspace. The caller
    /// already knows which acts are theirs — this saves the UI a guess —
    /// and it is omitted entirely for lenses where "mine" means nothing.
    #[serde(skip_serializing_if = "is_false")]
    pub mine: bool,
}

/// A contributed announce state, attributed to the act it speaks for.
#[derive(Clone, Debug, Serialize)]
pub struct NightAnnounceView {
    /// The contributing workspace's billed act, when it has one — a
    /// contribution from a workspace with no act on the bill still counts,
    /// and `act` stays null rather than guessing a name.
    pub act: Option<String>,
    pub state: String,
}

/// An asks contribution another workspace published — the co-promotion
/// asks a billed act may answer.
#[derive(Clone, Debug, Serialize)]
pub struct NightAskView {
    /// The contributing workspace's acts on this bill — empty when it has
    /// none, because the asks are public-by-choice either way.
    pub from_acts: Vec<String>,
    pub items: Vec<String>,
}

/// The caller's own contributed kinds, plus how many other workspaces are
/// contributing at all — a count, never which kinds or which workspaces.
#[derive(Debug, Serialize)]
pub struct ContributionSummary {
    pub own: Vec<String>,
    pub shared_by_others: i64,
}

/// The active organiser link as the band copies it — the token plus its
/// expiry, minted elsewhere and returned only at mint time.
#[derive(Clone, Debug, Serialize)]
pub struct OrganiserLinkView {
    pub token: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// The roster's read on the shared draw: what its own acts contributed
/// against everyone else's — the split nobody but the roster may see.
#[derive(Clone, Debug, Serialize)]
pub struct NightDrawSplit {
    pub ours: i64,
    pub rest: i64,
}

/// The venue block every lens carries — a name and a city, nothing more.
#[derive(Debug, Serialize)]
pub struct NightVenueView {
    pub display_name: String,
    pub city_name: String,
}

/// One night, projected for one lens.
///
/// The base fields every reader gets are always present; each lens-only
/// block is `Option` and skipped when absent, so the JSON a lens receives
/// never even names a field another lens owns. The two-level options —
/// `Option<Option<T>>` — are the deliberate split between *not this lens*
/// (outer `None`, the field is omitted) and *this lens, no data* (inner
/// `None`, serialized `null`): the organiser's `combined_reachable` is
/// `null` when nobody contributed, never `0`, because zero is a number and
/// absent is a fact about consent.
#[derive(Debug, Serialize)]
pub struct NightView {
    pub place_event_id: Uuid,
    pub lens: NightLens,
    pub venue: NightVenueView,
    /// The room's night — the UTC date the rendezvous is keyed on.
    pub event_date: Date,
    /// Every act on every bill of the night.
    pub lineup: Vec<NightActView>,
    /// Roll-up of the night's events by status — a cancelled show stays on
    /// the room's calendar and reads as `cancelled`, not as absent.
    pub status: BTreeMap<String, i64>,

    // ── OwnBand and Roster ──────────────────────────────────────────
    /// The caller's own event on the night, so the UI hops to the existing
    /// ladder page rather than rebuilding it here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_event_slug: Option<String>,
    /// The other acts on the night — names, positions, confirmed. Never
    /// their draw, their terms, or their costs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub co_bill: Option<Vec<NightActView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributions: Option<ContributionSummary>,
    /// The caller's own terms contribution, `null` when it never made one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_terms: Option<Option<serde_json::Value>>,
    /// The live organiser link for the band to copy — `null` until minted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organiser_link: Option<Option<OrganiserLinkView>>,

    // ── CoBilled ────────────────────────────────────────────────────
    /// Every workspace's contributed announce state — contributed means
    /// the act chose to publish it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_announce: Option<Vec<NightAnnounceView>>,
    /// Asks other workspaces published — public-by-choice by contribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asks: Option<Vec<NightAskView>>,

    // ── Organiser ───────────────────────────────────────────────────
    /// SUM of contributed `reachable_fans` — `null` when nobody
    /// contributed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub combined_reachable: Option<Option<i64>>,
    /// Paid and partially-refunded order items over the night's events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tickets_sold: Option<i64>,
    /// The venue's resolved capacity fact — `null` when the registry holds
    /// no global capacity claim for the room.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<Option<i64>>,
    /// SUM of contributed `terms.amount_minor` — the payer's single
    /// scalar. `null` when no act contributed terms; a per-act breakdown
    /// does not exist on this lens and cannot be made to appear here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payout_total_minor: Option<Option<i64>>,
    /// Per-act announce states — the contributed, therefore public, half.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce: Option<Vec<NightAnnounceView>>,

    // ── Roster additions ────────────────────────────────────────────
    /// The roster's own acts on the bill, with positions and confirmation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roster_acts: Option<Vec<NightActView>>,
    /// Its acts' contributed draw against the rest of the bill's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draw_split: Option<NightDrawSplit>,
}

#[derive(Debug, FromRow)]
struct NightRow {
    id: Uuid,
    venue_id: Uuid,
    event_date: Date,
    display_name: String,
    city_name: String,
}

#[derive(Debug, FromRow)]
struct NightEventRow {
    workspace_id: Uuid,
    slug: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct NightActRow {
    act_slug: String,
    position: i32,
    act_workspace_id: Option<Uuid>,
    confirmed_at: Option<OffsetDateTime>,
    confirmed_by: Option<Uuid>,
    resolved_name: String,
}

#[derive(Debug, FromRow)]
struct ContributionRow {
    workspace_id: Uuid,
    kind: String,
    value: serde_json::Value,
}

/// One terms contribution as the venue aggregate consumes it (§4h-9): the
/// contributing workspace is carried only so the domain can count distinct
/// contributors against `MIN_CONTRIBUTORS` — it is never part of what a
/// reader sees.
#[derive(Debug)]
pub struct VenueTermsRow {
    pub venue_id: Uuid,
    pub workspace_id: Uuid,
    pub amount_minor: i64,
    pub currency: String,
    pub contributed_at: OffsetDateTime,
}

/// Everything the payload builder reads — the night row, its events, its
/// billed acts, its live contributions. Loaded once per request; the four
/// lenses are projections over this one fact set.
struct NightFacts {
    night: NightRow,
    events: Vec<NightEventRow>,
    acts: Vec<NightActRow>,
    contributions: Vec<ContributionRow>,
}

impl PostgresNightRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The night read for a workspace-bearing caller. The lens is derived
    /// here, from the caller's relationship — never accepted as a parameter,
    /// and a caller with no relationship gets `NotFound`, which is also the
    /// answer for a night that does not exist.
    pub async fn load(
        &self,
        workspace_id: Uuid,
        place_event_id: Uuid,
    ) -> Result<NightView, NightError> {
        let facts = self.load_facts(place_event_id).await?;
        let owns_event = facts
            .events
            .iter()
            .any(|event| event.workspace_id == workspace_id);
        // The caller's billed slots on the night, confirmed or not — the
        // claim alone earns the least lens; confirmation governs the flag
        // on the lineup, not admission to the read.
        let billed_slots = facts
            .acts
            .iter()
            .filter(|act| act.act_workspace_id == Some(workspace_id))
            .count() as u64;
        let Some(lens) = derive_lens(owns_event, billed_slots) else {
            return Err(NightError::NotFound);
        };
        self.assemble(facts, lens, Some(workspace_id)).await
    }

    /// The night read for a link-holder — the organiser lens, forced. The
    /// token is the whole credential: a revoked, expired, or never-minted
    /// token is the same `NotFound` as a bad uuid.
    pub async fn load_organiser_by_token(&self, token: Uuid) -> Result<NightView, NightError> {
        let place_event_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT place_event_id
            FROM place_event_links
            WHERE token = $1 AND lens = 'organiser'
              AND revoked_at IS NULL AND expires_at > now()
            "#,
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(NightError::NotFound)?;
        let facts = self.load_facts(place_event_id).await?;
        self.assemble(facts, NightLens::Organiser, None).await
    }

    /// Publish or replace a contribution. Only a workspace with a
    /// relationship to the night may contribute — the shared view is not a
    /// wall anyone may post on.
    ///
    /// # Errors
    ///
    /// `NotFound` when the caller has no relationship to the night;
    /// `Database` on the write itself.
    pub async fn upsert_contribution(
        &self,
        workspace_id: Uuid,
        place_event_id: Uuid,
        kind: ContributionKind,
        value: &serde_json::Value,
    ) -> Result<(), NightError> {
        self.require_relationship(workspace_id, place_event_id)
            .await?;
        // The unique key covers revoked rows too, so the write is an
        // upsert: a re-contribute refreshes the value, clears the
        // revocation, and restarts `created_at` — the live version's own
        // timestamp is the audit this one-row-per-kind record carries.
        sqlx::query(
            r#"
            INSERT INTO place_event_contributions
                (place_event_id, workspace_id, kind, value)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (place_event_id, workspace_id, kind)
            DO UPDATE SET value = EXCLUDED.value,
                          status = 'active',
                          revoked_at = NULL,
                          revoke_reason = NULL,
                          created_at = now()
            "#,
        )
        .bind(place_event_id)
        .bind(workspace_id)
        .bind(kind.as_str())
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Withdraw a contribution. The row stays — `revoked` is the audit that
    /// the workspace once chose to share this, not a deletion of that fact.
    ///
    /// # Errors
    ///
    /// `NotFound` when no live contribution of that kind exists for the
    /// caller on this night.
    pub async fn revoke_contribution(
        &self,
        workspace_id: Uuid,
        place_event_id: Uuid,
        kind: ContributionKind,
    ) -> Result<(), NightError> {
        let changed = sqlx::query(
            r#"
            UPDATE place_event_contributions
            SET status = 'revoked', revoked_at = now()
            WHERE place_event_id = $1 AND workspace_id = $2 AND kind = $3
              AND status = 'active'
            "#,
        )
        .bind(place_event_id)
        .bind(workspace_id)
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(NightError::NotFound);
        }
        Ok(())
    }

    /// Mint the night's organiser link — revoke-then-mint, so there is only
    /// ever one live link and every link already sent dies on the rotation.
    /// Returns the token once; it is not readable back afterwards except by
    /// the band-side lenses that copy it out.
    ///
    /// # Errors
    ///
    /// `NotFound` when the caller has no relationship to the night.
    pub async fn mint_organiser_link(
        &self,
        workspace_id: Uuid,
        place_event_id: Uuid,
    ) -> Result<OrganiserLinkView, NightError> {
        self.require_relationship(workspace_id, place_event_id)
            .await?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "UPDATE place_event_links SET revoked_at = now() \
             WHERE place_event_id = $1 AND revoked_at IS NULL",
        )
        .bind(place_event_id)
        .execute(&mut *transaction)
        .await?;
        let (token, expires_at) = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
            r#"
            INSERT INTO place_event_links
                (place_event_id, lens, created_by, expires_at)
            VALUES ($1, 'organiser', $2, now() + make_interval(days => $3))
            RETURNING token, expires_at
            "#,
        )
        .bind(place_event_id)
        .bind(workspace_id)
        .bind(ORGANISER_LINK_DAYS)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(OrganiserLinkView { token, expires_at })
    }

    /// Revoke the night's live organiser link. The link's minter may revoke
    /// it, and so may a workspace that owns an event on the night — a
    /// co-billed act cannot take down a link it did not create.
    ///
    /// # Errors
    ///
    /// `NotFound` when no live link exists, or the caller is neither its
    /// minter nor an event owner on the night.
    pub async fn revoke_organiser_link(
        &self,
        workspace_id: Uuid,
        place_event_id: Uuid,
    ) -> Result<(), NightError> {
        let changed = sqlx::query(
            r#"
            UPDATE place_event_links AS link
            SET revoked_at = now()
            WHERE link.place_event_id = $1 AND link.revoked_at IS NULL
              AND (
                  link.created_by = $2
                  OR EXISTS (
                      SELECT 1 FROM events AS event
                      WHERE event.place_event_id = $1
                        AND event.workspace_id = $2
                  )
              )
            "#,
        )
        .bind(place_event_id)
        .bind(workspace_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(NightError::NotFound);
        }
        Ok(())
    }

    /// A billed act confirms it is really on the bill. The row's
    /// `act_workspace_id` must be the caller — a band confirms itself only,
    /// and anything else is the same `NotFound` as a nonexistent night.
    ///
    /// # Errors
    ///
    /// `NotFound` when the named act is not billed to the caller on this
    /// night.
    pub async fn confirm_act(
        &self,
        workspace_id: Uuid,
        place_event_id: Uuid,
        act_slug: &str,
    ) -> Result<(), NightError> {
        let changed = sqlx::query(
            r#"
            UPDATE event_acts AS act
            SET confirmed_at = now(), confirmed_by = $1
            FROM events AS event
            WHERE event.id = act.event_id
              AND event.workspace_id = act.workspace_id
              AND event.place_event_id = $2
              AND act.act_workspace_id = $1
              AND act.act_slug = $3
            "#,
        )
        .bind(workspace_id)
        .bind(place_event_id)
        .bind(act_slug)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(NightError::NotFound);
        }
        Ok(())
    }

    /// The relationship gate every write passes: the caller owns an event
    /// on the night or one of its acts is claimed on a bill of the night.
    /// Anything else is `NotFound` — a caller with no relationship learns
    /// nothing, including that the night exists.
    async fn require_relationship(
        &self,
        workspace_id: Uuid,
        place_event_id: Uuid,
    ) -> Result<(), NightError> {
        let related = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM events AS event
                WHERE event.place_event_id = $1 AND event.workspace_id = $2
            ) OR EXISTS(
                SELECT 1 FROM event_acts AS act
                JOIN events AS event
                  ON event.workspace_id = act.workspace_id
                 AND event.id = act.event_id
                WHERE event.place_event_id = $1 AND act.act_workspace_id = $2
            )
            "#,
        )
        .bind(place_event_id)
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        if related {
            Ok(())
        } else {
            Err(NightError::NotFound)
        }
    }

    async fn load_facts(&self, place_event_id: Uuid) -> Result<NightFacts, NightError> {
        let night = sqlx::query_as::<_, NightRow>(
            r#"
            SELECT night.id, night.venue_id, night.event_date,
                   venue.display_name, city.name AS city_name
            FROM place_events AS night
            JOIN place_venues AS venue ON venue.id = night.venue_id
            JOIN cities AS city ON city.id = venue.city_id
            WHERE night.id = $1
            "#,
        )
        .bind(place_event_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(NightError::NotFound)?;

        let events = sqlx::query_as::<_, NightEventRow>(
            r#"
            SELECT event.workspace_id, event.slug, event.status
            FROM events AS event
            WHERE event.place_event_id = $1
            ORDER BY event.starts_at, event.id
            "#,
        )
        .bind(place_event_id)
        .fetch_all(&self.pool)
        .await?;

        // The night's bill is every tenant's `event_acts` on these events.
        // The display name resolves listing-first — the act's own listing
        // name, then the peer record's earliest-seen spelling, then the
        // name the bill's writer typed: the claim, not a guess at one.
        let acts = sqlx::query_as::<_, NightActRow>(
            r#"
            SELECT act.act_slug, act.position, act.act_workspace_id,
                   act.confirmed_at, act.confirmed_by,
                   COALESCE(listing.act_name, peer.display_name, act.act_name)
                       AS resolved_name
            FROM event_acts AS act
            JOIN events AS event
              ON event.workspace_id = act.workspace_id
             AND event.id = act.event_id
            LEFT JOIN viryaos_band_listings AS listing
              ON listing.workspace_id = act.act_workspace_id
            LEFT JOIN place_peer_acts AS peer
              ON peer.id = act.peer_act_id
            WHERE event.place_event_id = $1
            ORDER BY act.position, act.act_slug, act.workspace_id
            "#,
        )
        .bind(place_event_id)
        .fetch_all(&self.pool)
        .await?;

        let contributions = sqlx::query_as::<_, ContributionRow>(
            r#"
            SELECT contribution.workspace_id, contribution.kind, contribution.value
            FROM place_event_contributions AS contribution
            WHERE contribution.place_event_id = $1
              AND contribution.status = 'active'
            ORDER BY contribution.workspace_id, contribution.kind
            "#,
        )
        .bind(place_event_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(NightFacts {
            night,
            events,
            acts,
            contributions,
        })
    }

    /// The venue's resolved capacity — the same global-fact resolve order
    /// `city_venues` uses: played beats researched beats evidence beats a
    /// directory, first non-expired claim wins, and contributor-private
    /// facts never enter a cross-tenant read.
    async fn resolved_capacity(&self, venue_id: Uuid) -> Result<Option<i64>, NightError> {
        let value = sqlx::query_scalar::<_, String>(
            r#"
            SELECT fact.value
            FROM place_venue_facts AS fact
            WHERE fact.venue_id = $1
              AND fact.attribute = 'capacity'
              AND fact.workspace_id IS NULL
              AND (fact.expires_at IS NULL OR fact.expires_at > now())
            ORDER BY CASE fact.provenance
                         WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                         WHEN 'event_evidence' THEN 2 WHEN 'open_directory' THEN 3
                         ELSE 4 END,
                     fact.observed_at DESC
            LIMIT 1
            "#,
        )
        .bind(venue_id)
        .fetch_optional(&self.pool)
        .await?;
        // The fact's value is text; a non-numeric claim reads as absent
        // rather than failing the night.
        Ok(value.and_then(|raw| raw.trim().parse::<i64>().ok()))
    }

    /// Paid order items across the night's events — the same predicates the
    /// Sprint M tickets-sold fix uses: `ticket_order_items.quantity` under
    /// orders in `paid`/`partially_refunded`.
    async fn tickets_sold(&self, place_event_id: Uuid) -> Result<i64, NightError> {
        let sold = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COALESCE(SUM(item.quantity), 0)::bigint
            FROM events AS event
            JOIN ticket_sales AS sale
              ON sale.workspace_id = event.workspace_id
             AND sale.event_id = event.id
             AND sale.active
            JOIN ticket_orders AS orders
              ON orders.workspace_id = sale.workspace_id
             AND orders.ticket_sale_id = sale.id
             AND orders.status IN ('paid', 'partially_refunded')
            JOIN ticket_order_items AS item
              ON item.workspace_id = orders.workspace_id
             AND item.ticket_order_id = orders.id
            WHERE event.place_event_id = $1
            "#,
        )
        .bind(place_event_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(sold)
    }

    /// The venue-level terms read (§4h-9): every active `terms`
    /// contribution on every night at the given rooms, across all
    /// workspaces — the raw material `aggregate_venue_terms` bands over.
    ///
    /// Deliberately global: the fee band exists only as a cross-tenant
    /// pool, because scoped to one workspace it would be exactly the
    /// identifiable single-tenant answer the aggregate exists to prevent.
    /// The statement still selects `workspace_id` — the contributor key
    /// the domain's `MIN_CONTRIBUTORS` floor counts — which is also what
    /// keeps the workspace-scope ratchet satisfied; the deliberate
    /// non-scoping is the absent `WHERE`, not a dropped column. Declining
    /// to contribute costs no read access: the caller's workspace is not
    /// even a parameter of this read.
    ///
    /// # Errors
    ///
    /// `Database` on the read itself.
    pub async fn venue_terms_contributions(
        &self,
        venue_ids: &[Uuid],
    ) -> Result<Vec<VenueTermsRow>, NightError> {
        let rows = sqlx::query_as::<_, (Uuid, Uuid, serde_json::Value, OffsetDateTime)>(
            r#"
            SELECT night.venue_id, contribution.workspace_id,
                   contribution.value, contribution.created_at
            FROM place_event_contributions AS contribution
            JOIN place_events AS night
              ON night.id = contribution.place_event_id
            WHERE night.venue_id = ANY($1)
              AND contribution.status = 'active'
              AND contribution.kind = 'terms'
            ORDER BY night.venue_id, contribution.created_at, contribution.id
            "#,
        )
        .bind(venue_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(venue_id, workspace_id, value, contributed_at)| {
                // The write gate shapes the value, but a row that cannot
                // name a fee and its currency is not evidence — skipped,
                // not decoded as a zero.
                let amount_minor = value.get("amount_minor")?.as_i64()?;
                let currency = value.get("currency")?.as_str()?.to_owned();
                Some(VenueTermsRow {
                    venue_id,
                    workspace_id,
                    amount_minor,
                    currency,
                    contributed_at,
                })
            })
            .collect())
    }

    /// The currently-live organiser link, for the band-side lenses that
    /// copy it out. `None` until one is minted — or after it dies.
    async fn active_organiser_link(
        &self,
        place_event_id: Uuid,
    ) -> Result<Option<OrganiserLinkView>, NightError> {
        let link = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
            r#"
            SELECT token, expires_at
            FROM place_event_links
            WHERE place_event_id = $1 AND revoked_at IS NULL AND expires_at > now()
            "#,
        )
        .bind(place_event_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(link.map(|(token, expires_at)| OrganiserLinkView { token, expires_at }))
    }

    /// Project the fact set into one lens's payload. `caller` is the
    /// workspace asking; the organiser-link path passes `None` — no
    /// workspace answers for a bearer.
    async fn assemble(
        &self,
        facts: NightFacts,
        lens: NightLens,
        caller: Option<Uuid>,
    ) -> Result<NightView, NightError> {
        let place_event_id = facts.night.id;

        let lineup: Vec<NightActView> =
            facts.acts.iter().map(|act| act_view(act, caller)).collect();

        let mut status: BTreeMap<String, i64> = BTreeMap::new();
        for event in &facts.events {
            *status.entry(event.status.clone()).or_insert(0) += 1;
        }

        // Contributions, read once: the caller's own kinds, the draw sums
        // split by contributor, the announce states attributed to their
        // acts, the asks other workspaces published, and the payout total
        // the organiser alone may read.
        let mut own_kinds: Vec<String> = Vec::new();
        let mut own_terms: Option<serde_json::Value> = None;
        let mut combined_reachable: Option<i64> = None;
        let mut own_reachable: i64 = 0;
        let mut payout_total_minor: Option<i64> = None;
        let mut announce: Vec<NightAnnounceView> = Vec::new();
        let mut asks: Vec<NightAskView> = Vec::new();
        let mut other_contributors: BTreeSet<Uuid> = BTreeSet::new();

        for contribution in &facts.contributions {
            let is_own = Some(contribution.workspace_id) == caller;
            if is_own {
                own_kinds.push(contribution.kind.clone());
            } else {
                other_contributors.insert(contribution.workspace_id);
            }
            match ContributionKind::parse(&contribution.kind) {
                Some(ContributionKind::DrawEstimate) => {
                    let reachable = contribution
                        .value
                        .get("reachable_fans")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    *combined_reachable.get_or_insert(0) += reachable;
                    if is_own {
                        own_reachable += reachable;
                    }
                }
                Some(ContributionKind::Terms) => {
                    let amount = contribution
                        .value
                        .get("amount_minor")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    *payout_total_minor.get_or_insert(0) += amount;
                    if is_own {
                        own_terms = Some(contribution.value.clone());
                    }
                }
                Some(ContributionKind::AnnounceStatus) => {
                    let state = contribution
                        .value
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("planned")
                        .to_owned();
                    // Attribute the state to each of the contributing
                    // workspace's billed acts; a contributor with no act on
                    // the bill still gets one row, act unnamed.
                    let act_names: Vec<String> = facts
                        .acts
                        .iter()
                        .filter(|act| act.act_workspace_id == Some(contribution.workspace_id))
                        .map(|act| act.resolved_name.clone())
                        .collect();
                    if act_names.is_empty() {
                        announce.push(NightAnnounceView { act: None, state });
                    } else {
                        announce.extend(act_names.into_iter().map(|name| NightAnnounceView {
                            act: Some(name),
                            state: state.clone(),
                        }));
                    }
                }
                Some(ContributionKind::Asks) if !is_own => {
                    let items = contribution
                        .value
                        .get("items")
                        .and_then(serde_json::Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    let from_acts = facts
                        .acts
                        .iter()
                        .filter(|act| act.act_workspace_id == Some(contribution.workspace_id))
                        .map(|act| act.resolved_name.clone())
                        .collect();
                    asks.push(NightAskView { from_acts, items });
                }
                _ => {}
            }
        }
        let rest_reachable = combined_reachable.unwrap_or(0) - own_reachable;

        let band_side = matches!(lens, NightLens::OwnBand | NightLens::Roster);
        let own_event_slug = caller.and_then(|caller_id| {
            facts
                .events
                .iter()
                .find(|event| event.workspace_id == caller_id)
                .map(|event| event.slug.clone())
        });

        // The band-side lenses copy the live link out; the organiser's
        // two fact reads run only for that lens — a band-side payload
        // never even asks for them.
        let organiser_link = if band_side {
            Some(self.active_organiser_link(place_event_id).await?)
        } else {
            None
        };
        let (tickets_sold, capacity) = if lens == NightLens::Organiser {
            (
                Some(self.tickets_sold(place_event_id).await?),
                Some(self.resolved_capacity(facts.night.venue_id).await?),
            )
        } else {
            (None, None)
        };

        // The band-side split views read off the same lineup — computed
        // before the move below.
        let co_bill = band_side.then(|| {
            lineup
                .iter()
                .filter(|act| !act.mine)
                .map(|act| NightActView {
                    mine: false,
                    ..act.clone()
                })
                .collect::<Vec<_>>()
        });
        let roster_acts = (lens == NightLens::Roster).then(|| {
            lineup
                .iter()
                .filter(|act| act.mine)
                .cloned()
                .collect::<Vec<_>>()
        });

        Ok(NightView {
            place_event_id,
            lens,
            venue: NightVenueView {
                display_name: facts.night.display_name.clone(),
                city_name: facts.night.city_name.clone(),
            },
            event_date: facts.night.event_date,
            lineup,
            status,
            own_event_slug: own_event_slug.filter(|_| band_side),
            co_bill,
            contributions: band_side.then_some(ContributionSummary {
                own: own_kinds,
                shared_by_others: other_contributors.len() as i64,
            }),
            own_terms: band_side.then_some(own_terms),
            organiser_link,
            public_announce: (lens == NightLens::CoBilled).then(|| announce.clone()),
            asks: (lens == NightLens::CoBilled).then_some(asks),
            combined_reachable: (lens == NightLens::Organiser).then_some(combined_reachable),
            tickets_sold,
            capacity,
            payout_total_minor: (lens == NightLens::Organiser).then_some(payout_total_minor),
            announce: (lens == NightLens::Organiser).then_some(announce),
            roster_acts,
            draw_split: (lens == NightLens::Roster).then_some(NightDrawSplit {
                ours: own_reachable,
                rest: rest_reachable,
            }),
        })
    }
}

/// The act row as a lens reads it: resolved name, the bill's position, the
/// confirmation flag that only counts when the act's own workspace signed
/// it, and `mine` for the caller.
fn act_view(act: &NightActRow, caller: Option<Uuid>) -> NightActView {
    NightActView {
        act_slug: act.act_slug.clone(),
        name: act.resolved_name.clone(),
        position: act.position,
        confirmed: act.confirmed_at.is_some() && act.confirmed_by == act.act_workspace_id,
        mine: act.act_workspace_id.is_some() && act.act_workspace_id == caller,
    }
}
