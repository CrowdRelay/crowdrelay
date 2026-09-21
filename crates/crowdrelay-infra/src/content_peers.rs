//! The peer write path — operator entries, scanner proposals, resolutions.
//!
//! `PostgresContentEngineRepository` keeps the plain per-entity storage in
//! `content_engine.rs`; this module holds the paths that *decide* a peer row.
//! The operator's create rides the operator-action ledger every admin write
//! passes through, so a retried request answers with the peer it already
//! made rather than landing a second row — and a live same-name row is a
//! conflict, not the scanner's silent dedup. The resolution carries the
//! confirm-time patch: a scanner proposal arrives without handles and can
//! never be observed until the operator adds them, so confirming and
//! patching are one `UPDATE` behind the same `status = 'proposed'` guard.
//!
//! The proposal pass reads the peer-act graph: `place_peer_acts` whose
//! canonicalised genres intersect the workspace's band-listing genres land
//! `proposed` for the operator to confirm or refuse. A refused name is
//! never re-proposed — `rejection_reason` is the suppression record.

use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crowdrelay_application::{IdempotencyKey, RepositoryError, RequestId};
use crowdrelay_domain::{
    PeerId, WorkspaceId,
    content_engine::{Peer, PeerStatus, PeerTier, normalize_watch_for},
};
use serde_json::json;

use crate::autopilot::operator_actions::insert_operator_action;
use crate::content_engine::{ContentEngineError, PostgresContentEngineRepository, Result};

#[derive(Debug, FromRow)]
struct PeerRow {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    handles: serde_json::Value,
    tier: String,
    watch_for: Vec<String>,
    why: String,
    proposed_by: String,
    status: String,
    rejection_reason: Option<String>,
    confirmed_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<PeerRow> for Peer {
    type Error = ContentEngineError;

    fn try_from(row: PeerRow) -> Result<Self> {
        Ok(Self {
            id: PeerId::from_uuid(row.id),
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            name: row.name,
            handles: row.handles,
            tier: PeerTier::parse(&row.tier)
                .ok_or(ContentEngineError::UnknownValue("peer.tier"))?,
            watch_for: normalize_watch_for(&row.watch_for),
            why: row.why,
            proposed_by: row.proposed_by,
            status: PeerStatus::parse(&row.status)
                .ok_or(ContentEngineError::UnknownValue("peer.status"))?,
            rejection_reason: row.rejection_reason,
            confirmed_at: row.confirmed_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// What the API or a scanner knows when it proposes a peer. The peer lands
/// `proposed` until the operator confirms — a candidate is not a peer.
#[derive(Clone, Debug)]
pub struct NewPeer {
    pub name: String,
    pub handles: serde_json::Value,
    pub tier: PeerTier,
    pub watch_for: Vec<String>,
    pub why: String,
    /// `'operator'` for typed-in entries; a scanner name for proposals.
    pub proposed_by: String,
    /// `true` only for rows the operator confirmed at entry — scanner
    /// proposals always start `proposed`.
    pub confirmed: bool,
}

/// The fields an operator may set while confirming a proposal. `None` keeps
/// the stored value — a patch of all-`None` confirms the row as it stands.
/// Carried into the same `UPDATE` as the transition: a proposal confirmed
/// with handles is observable from the very next sweep.
#[derive(Clone, Debug, Default)]
pub struct PeerPatch {
    pub handles: Option<serde_json::Value>,
    pub watch_for: Option<Vec<String>>,
    pub tier: Option<PeerTier>,
}

/// What the ledger'd operator writes answer: the row, and whether this
/// request wrote it or the idempotency key already had.
#[derive(Debug)]
pub enum PeerOutcome {
    /// The write landed on this request.
    Applied(Peer),
    /// The same key already carried this request — the stored row answers
    /// instead of a second write.
    Replayed(Peer),
}

/// `proposed_by` for rows the peer-act-graph pass writes.
const SCANNER_NAME: &str = "peer-act-graph";
/// Proposals per workspace per sweep — a rich genre graph must not flood
/// the operator's queue in one tick.
const MAX_PROPOSALS_PER_SWEEP: i64 = 10;

/// The transition's input rules, checked before any row or ledger write so
/// a refused request leaves nothing behind. Returns the trimmed reason.
fn check_resolution<'a>(
    next: PeerStatus,
    rejection_reason: Option<&'a str>,
    patch: Option<&PeerPatch>,
) -> Result<Option<&'a str>> {
    if !PeerStatus::Proposed.can_transition_to(next) {
        return Err(ContentEngineError::InvalidTransition);
    }
    let reason = rejection_reason
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (next, reason, patch) {
        (PeerStatus::Rejected, None, _) => Err(ContentEngineError::MissingReason),
        // A rejection is terminal: there is no live row left to patch, and
        // the reason is the suppression record. Fields on a reject are a
        // caller bug, not an edit.
        (PeerStatus::Rejected, Some(_), Some(_)) => Err(ContentEngineError::InvalidTransition),
        // A confirmation must not carry a rejection reason.
        (PeerStatus::Confirmed, Some(_), _) => Err(ContentEngineError::InvalidTransition),
        _ => Ok(reason),
    }
}

/// The ledger refusing the key is a caller-visible conflict, not a database
/// fault; everything else is infrastructure.
fn ledger_error(error: RepositoryError) -> ContentEngineError {
    match error {
        RepositoryError::Conflict => ContentEngineError::KeyConflict,
        _ => ContentEngineError::Database(sqlx::Error::Protocol(
            "operator action ledger write failed".to_owned(),
        )),
    }
}

impl PostgresContentEngineRepository {
    /// Inserts a peer in `proposed` or `confirmed` status. The unique index
    /// on `(workspace_id, lower(name))` keeps a name from landing twice;
    /// `ON CONFLICT DO NOTHING` returns `None` on a clash rather than
    /// failing, so a scanner re-proposing the same artist is a no-op.
    pub async fn create_peer(
        &self,
        workspace_id: WorkspaceId,
        peer: &NewPeer,
    ) -> Result<Option<Peer>> {
        Self::insert_peer(&self.pool, workspace_id, PeerId::new().into_uuid(), peer).await
    }

    async fn insert_peer<'e, E>(
        executor: E,
        workspace_id: WorkspaceId,
        peer_id: Uuid,
        peer: &NewPeer,
    ) -> Result<Option<Peer>>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let status = if peer.confirmed {
            PeerStatus::Confirmed
        } else {
            PeerStatus::Proposed
        };
        let row = sqlx::query_as::<_, PeerRow>(
            r#"
            INSERT INTO peers (
                id, workspace_id, name, handles, tier, watch_for, why,
                proposed_by, status, confirmed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                    CASE WHEN $9 = 'confirmed' THEN now() END)
            ON CONFLICT DO NOTHING
            RETURNING *
            "#,
        )
        .bind(peer_id)
        .bind(workspace_id.into_uuid())
        .bind(peer.name.trim())
        .bind(&peer.handles)
        .bind(peer.tier.as_str())
        .bind(normalize_watch_for(&peer.watch_for))
        .bind(&peer.why)
        .bind(&peer.proposed_by)
        .bind(status.as_str())
        .fetch_optional(executor)
        .await?;
        row.map(Peer::try_from).transpose()
    }

    /// The operator's create, inside the operator-action ledger every admin
    /// write passes through: the audit row lands first, keyed on the
    /// caller's idempotency key, so a retried request replays its own
    /// answer. A live same-name row is `PeerNameTaken` — the operator asked
    /// for a new entry and "already there" is the honest answer, where the
    /// scanner's same clash is a silent no-op.
    pub async fn create_operator_peer(
        &self,
        workspace_id: WorkspaceId,
        peer: &NewPeer,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<PeerOutcome> {
        let mut tx = self.pool.begin().await?;
        // The name is a peer's natural identity — the ledger row's target
        // is the standing row when one exists, so a retried create resolves
        // to the same target instead of a fresh id the ledger could never
        // match.
        let standing = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id FROM peers
            WHERE workspace_id = $1 AND lower(btrim(name)) = lower(btrim($2))
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(peer.name.trim())
        .fetch_optional(&mut *tx)
        .await?;
        let peer_id = standing.unwrap_or_else(|| PeerId::new().into_uuid());
        // The ledger dedupes on the request's own content — the fields the
        // caller sent, so a same-key retry of the same create replays and a
        // reused key on a different peer conflicts.
        let details = json!({
            "name": peer.name.trim(),
            "tier": peer.tier.as_str(),
            "handles": peer.handles,
            "watch_for": peer.watch_for,
            "why": peer.why,
        });
        match insert_operator_action(
            &mut tx,
            workspace_id,
            Uuid::now_v7(),
            "create_peer",
            "peer",
            peer_id,
            "admin_api_key",
            idempotency_key,
            request_id,
            &details,
        )
        .await
        {
            Ok(Some(_)) => {
                let row = sqlx::query_as::<_, PeerRow>(
                    r#"
                    SELECT * FROM peers
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(peer_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ContentEngineError::Database(sqlx::Error::RowNotFound))?;
                tx.commit().await?;
                return Ok(PeerOutcome::Replayed(Peer::try_from(row)?));
            }
            Ok(None) => {}
            Err(error) => return Err(ledger_error(error)),
        }
        // The ledger's target is the id this insert lands with — an audit
        // row naming a uuid no row carries would be a lie. Binding the
        // standing row's id also closes the rejected-tombstone case: the
        // name index is partial (`status <> 'rejected'`), so a refused name
        // passes `ON CONFLICT` and would land a live twin — the primary-key
        // clash turns that into the `None` this branch reads as taken.
        let Some(peer) = Self::insert_peer(&mut *tx, workspace_id, peer_id, peer).await? else {
            return Err(ContentEngineError::PeerNameTaken);
        };
        tx.commit().await?;
        Ok(PeerOutcome::Applied(peer))
    }

    /// Lists peers for a workspace, optionally narrowed to one status.
    /// Sweeps ask for `Confirmed` only — proposals are never observed.
    pub async fn list_peers(
        &self,
        workspace_id: WorkspaceId,
        status: Option<PeerStatus>,
    ) -> Result<Vec<Peer>> {
        let rows = sqlx::query_as::<_, PeerRow>(
            r#"
            SELECT * FROM peers
            WHERE workspace_id = $1
              AND ($2::text IS NULL OR status = $2)
            ORDER BY created_at ASC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(status.map(PeerStatus::as_str))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Peer::try_from).collect()
    }

    /// Resolves a `proposed` peer to `confirmed` or `rejected`. The
    /// transition guard runs in the `WHERE` clause so a stale approval can
    /// never un-reject a peer. A rejection must carry its reason — that is
    /// the record that stops the same wrong name being proposed twice — and
    /// a confirmation must not carry one.
    ///
    /// `patch` applies only on confirm: a scanner proposal arrives without
    /// handles and is never observable until the operator adds them, so the
    /// fields patch the row in the same `UPDATE` that confirms it.
    pub async fn resolve_peer(
        &self,
        workspace_id: WorkspaceId,
        peer_id: PeerId,
        next: PeerStatus,
        rejection_reason: Option<&str>,
        patch: Option<&PeerPatch>,
    ) -> Result<Peer> {
        let reason = check_resolution(next, rejection_reason, patch)?;
        Self::resolve_peer_in(&self.pool, workspace_id, peer_id, next, reason, patch).await
    }

    /// The operator's resolve, ledger'd like the create: the audit row lands
    /// first, a same-key replay answers with the row as it now stands, and a
    /// key reused for a different resolution conflicts rather than
    /// answering with somebody else's action.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_peer_operator(
        &self,
        workspace_id: WorkspaceId,
        peer_id: PeerId,
        next: PeerStatus,
        rejection_reason: Option<&str>,
        patch: Option<&PeerPatch>,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<PeerOutcome> {
        let reason = check_resolution(next, rejection_reason, patch)?;
        let details = json!({
            "status": next.as_str(),
            "rejection_reason": reason,
            "handles": patch.and_then(|patch| patch.handles.as_ref()),
            "watch_for": patch.and_then(|patch| patch.watch_for.as_ref()),
            "tier": patch.and_then(|patch| patch.tier.map(PeerTier::as_str)),
        });
        let mut tx = self.pool.begin().await?;
        match insert_operator_action(
            &mut tx,
            workspace_id,
            Uuid::now_v7(),
            "resolve_peer",
            "peer",
            peer_id.into_uuid(),
            "admin_api_key",
            idempotency_key,
            request_id,
            &details,
        )
        .await
        {
            Ok(Some(_)) => {
                let row = sqlx::query_as::<_, PeerRow>(
                    r#"
                    SELECT * FROM peers
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(peer_id.into_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ContentEngineError::Database(sqlx::Error::RowNotFound))?;
                tx.commit().await?;
                return Ok(PeerOutcome::Replayed(Peer::try_from(row)?));
            }
            Ok(None) => {}
            Err(error) => return Err(ledger_error(error)),
        }
        let peer =
            Self::resolve_peer_in(&mut *tx, workspace_id, peer_id, next, reason, patch).await?;
        tx.commit().await?;
        Ok(PeerOutcome::Applied(peer))
    }

    async fn resolve_peer_in<'e, E>(
        executor: E,
        workspace_id: WorkspaceId,
        peer_id: PeerId,
        next: PeerStatus,
        reason: Option<&str>,
        patch: Option<&PeerPatch>,
    ) -> Result<Peer>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let row = sqlx::query_as::<_, PeerRow>(
            r#"
            UPDATE peers
            SET status = $3,
                rejection_reason = CASE WHEN $3 = 'rejected' THEN $4 END,
                confirmed_at = CASE WHEN $3 = 'confirmed' THEN now() END,
                handles = COALESCE($5, handles),
                watch_for = COALESCE($6, watch_for),
                tier = COALESCE($7, tier),
                updated_at = now()
            WHERE id = $2 AND workspace_id = $1 AND status = 'proposed'
            RETURNING *
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(peer_id.into_uuid())
        .bind(next.as_str())
        .bind(reason)
        .bind(patch.and_then(|patch| patch.handles.as_ref()))
        .bind(patch.and_then(|patch| patch.watch_for.as_ref().map(|w| normalize_watch_for(w))))
        .bind(patch.and_then(|patch| patch.tier.map(PeerTier::as_str)))
        .fetch_optional(executor)
        .await?
        .ok_or(ContentEngineError::InvalidTransition)?;
        Peer::try_from(row)
    }

    /// One proposal pass over the peer-act graph for this workspace: every
    /// `place_peer_acts` row whose canonicalised genres intersect the
    /// workspace's band-listing genres lands `proposed`, capped so a rich
    /// graph cannot flood the operator's queue in one tick.
    ///
    /// Both genre sides resolve through `place_genre_aliases` — the same
    /// shape `gig_planning`'s comparable-acts test uses, so a spelling
    /// variant cannot strand a real match. The `NOT EXISTS` names the
    /// standing rows in *every* status: the name index only guards live
    /// ones, and a rejected name must never be re-asked — the operator's
    /// `rejection_reason` is what that row exists to honour. Rooms are
    /// counted across the whole venue registry — shared knowledge, like a
    /// room's capacity — so the evidence reads the act's real billing, not
    /// only this tenant's view of it.
    pub async fn propose_peers(&self, workspace_id: WorkspaceId) -> Result<Vec<Peer>> {
        let rows = sqlx::query_as::<_, PeerRow>(
            r#"
            WITH mine AS (
                SELECT DISTINCT COALESCE(alias.canonical, lower(btrim(tag))) AS genre
                FROM band_listings AS listing
                CROSS JOIN unnest(listing.genre_tags) AS tag
                LEFT JOIN place_genre_aliases AS alias
                    ON alias.alias = lower(btrim(tag))
                WHERE listing.workspace_id = $1
            ), candidates AS (
                SELECT act.id AS peer_act_id, act.display_name,
                       act.home_city_id,
                       shared.shared_genres, billing.tracked_rooms,
                       EXISTS (
                           SELECT 1 FROM place_peer_act_facts AS fact
                           WHERE fact.peer_act_id = act.id
                             AND fact.workspace_id = $1
                             AND fact.attribute = 'contact_email'
                       ) AS holds_contact
                FROM place_peer_acts AS act
                CROSS JOIN LATERAL (
                    SELECT array_agg(DISTINCT their.genre ORDER BY their.genre)
                        AS shared_genres
                    FROM (
                        SELECT COALESCE(their_alias.canonical,
                                        lower(btrim(their_genre.genre_tag))) AS genre
                        FROM place_peer_act_genres AS their_genre
                        LEFT JOIN place_genre_aliases AS their_alias
                            ON their_alias.alias = lower(btrim(their_genre.genre_tag))
                        WHERE their_genre.peer_act_id = act.id
                    ) AS their
                    JOIN mine ON mine.genre = their.genre
                ) AS shared
                CROSS JOIN LATERAL (
                    SELECT count(DISTINCT mark.venue_id) AS tracked_rooms
                    FROM event_acts AS billed
                    JOIN place_venue_marks AS mark
                        ON mark.event_id = billed.event_id
                    WHERE billed.peer_act_id = act.id
                ) AS billing
                WHERE shared.shared_genres IS NOT NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM peers AS existing
                      WHERE existing.workspace_id = $1
                        AND lower(btrim(existing.name)) = lower(btrim(act.display_name))
                  )
                ORDER BY billing.tracked_rooms DESC, act.name_key
                LIMIT $2
            )
            INSERT INTO peers (
                id, workspace_id, name, handles, tier, watch_for, why,
                proposed_by, status
            )
            SELECT uuidv7(), $1, display_name,
                   -- A seeded band sheet carries the act's public page as a
                   -- global `link:social` fact; when that page is a YouTube
                   -- channel the observer already knows how to read it, so
                   -- the proposal arrives watchable instead of waiting for
                   -- an operator to paste the same link in. Other platforms
                   -- stay '{}' — a handle the observer cannot read is noise.
                   COALESCE((
                       SELECT jsonb_build_object('youtube', fact.value)
                       FROM place_peer_act_facts AS fact
                       WHERE fact.peer_act_id = candidates.peer_act_id
                         AND fact.workspace_id IS NULL
                         AND fact.attribute = 'link:social'
                         AND fact.value ILIKE '%youtube.com/%'
                       ORDER BY fact.observed_at DESC
                       LIMIT 1
                   ), '{}'::jsonb),
                   -- A tier is a claim about level, and the registry only
                   -- supports it with scene evidence: the act bills rooms we
                   -- track, its home town resolves against the catalogue,
                   -- or this tenant already holds its lead. A sheet row
                   -- with none of the three — however famous, however
                   -- genre-fitting — is a name worth watching, not a
                   -- demonstrated peer, so it lands aspirational instead of
                   -- borrowing a peer's claim on the strength of a genre
                   -- tag alone.
                   CASE WHEN tracked_rooms > 0
                           OR home_city_id IS NOT NULL
                           OR holds_contact
                        THEN 'near_peer' ELSE 'aspirational' END,
                   '{}'::text[],
                   CASE WHEN array_length(shared_genres, 1) = 1
                        THEN 'shared genre ' || shared_genres[1]
                        ELSE 'shared genres ' || array_to_string(shared_genres, ', ')
                   END ||
                   CASE WHEN tracked_rooms > 0
                        THEN ' — billed in ' || tracked_rooms || ' tracked rooms'
                        WHEN home_city_id IS NOT NULL
                        THEN ' — placed in a catalogued city'
                        WHEN holds_contact THEN ' — contact lead on file'
                        ELSE ' — directory entry, no circuit evidence yet'
                   END,
                   $3, 'proposed'
            FROM candidates
            ON CONFLICT DO NOTHING
            RETURNING *
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(MAX_PROPOSALS_PER_SWEEP)
        .bind(SCANNER_NAME)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Peer::try_from).collect()
    }
}
