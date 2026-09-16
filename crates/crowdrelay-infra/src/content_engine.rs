//! Content engine persistence (sprint 3.5a): peers, peer observations, the
//! format catalogue, production events, capture plans, arcs, suggestions and
//! outcomes.
//!
//! Every query is workspace-scoped — `viryaos_content_format_entries` is the
//! only global table, because the catalogue is a seeded prior shared by all
//! tenants. Status moves go through `*_can_transition`-guarded updates that
//! match the expected current state in the `WHERE` clause, so two concurrent
//! transitions cannot both win.

use sqlx::{FromRow, PgPool};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crowdrelay_domain::{
    ArcId, CapturePlanId, ContentSuggestionId, EventId, PeerId, ProductionEventId, WorkspaceId,
    WorkspaceMemberId,
    content_engine::{
        Arc, ArcStatus, CapabilityProfile, CapturePlan, CapturePlanStatus, ContentFormatEntry,
        ContentSuggestion, Effort, FanObservation, FormatCadence, FormatCategory, FormatPurpose,
        FormatRequirement, Peer, PeerObservation, PeerStatus, PeerTier, ProductionEvent,
        ProductionEventKind, ProductionEventStatus, SuggestionOutcome, SuggestionOutcomeKind,
        SuggestionStatus, normalize_watch_for, parse_format_skill,
    },
    team_operations::TeamSkill,
};

#[derive(Debug, thiserror::Error)]
pub enum ContentEngineError {
    /// The `WHERE status = expected` clause matched nothing: either the row
    /// is gone or another transition already moved it. Failing closed keeps
    /// a stale writer from resurrecting a dead row.
    #[error("status transition is not valid for the row's current state")]
    InvalidTransition,
    /// A `PeerStatus`/`ArcStatus`/... literal from the database that the
    /// domain enums do not know. Surfaced rather than silently mapped so a
    /// vocabulary drift is loud.
    #[error("unrecognized stored value: {0}")]
    UnknownValue(&'static str),
    /// A rejection without its reason would let the same wrong name be
    /// proposed again — the reason is the suppression record.
    #[error("a rejection must carry its reason")]
    MissingReason,
    #[error("content engine database operation failed")]
    Database(sqlx::Error),
}

impl From<sqlx::Error> for ContentEngineError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// `pub(crate)` so the trend-extension module shares the error vocabulary.
pub(crate) type Result<T> = std::result::Result<T, ContentEngineError>;

#[derive(Clone)]
pub struct PostgresContentEngineRepository {
    /// `pub(crate)` so extension impl blocks (content_trends.rs) reach it —
    /// inherent impls in sibling modules cannot touch private fields.
    pub(crate) pool: PgPool,
}

// ── Rows ──────────────────────────────────────────────────────────────────

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

#[derive(Debug, FromRow)]
struct ObservationRow {
    id: i64,
    workspace_id: Uuid,
    peer_id: Uuid,
    observed_at: Date,
    platform: String,
    kind: String,
    fact: String,
    url: Option<String>,
    metrics: serde_json::Value,
}

impl From<ObservationRow> for PeerObservation {
    fn from(row: ObservationRow) -> Self {
        Self {
            id: row.id,
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            peer_id: PeerId::from_uuid(row.peer_id),
            observed_at: row.observed_at,
            platform: row.platform,
            kind: row.kind,
            fact: row.fact,
            url: row.url,
            metrics: row.metrics,
        }
    }
}

#[derive(Debug, FromRow)]
struct FanObservationRow {
    id: i64,
    workspace_id: Uuid,
    place_id: Uuid,
    observed_at: Date,
    platform: String,
    kind: String,
    fact: String,
    url: Option<String>,
    metrics: serde_json::Value,
}

impl From<FanObservationRow> for FanObservation {
    fn from(row: FanObservationRow) -> Self {
        Self {
            id: row.id,
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            place_id: row.place_id,
            observed_at: row.observed_at,
            platform: row.platform,
            kind: row.kind,
            fact: row.fact,
            url: row.url,
            metrics: row.metrics,
        }
    }
}

#[derive(Debug, FromRow)]
struct FormatEntryRow {
    key: String,
    name: String,
    category: String,
    purpose: String,
    effort_standalone: String,
    effort_marginal: String,
    skill: String,
    requires: String,
    distribution: String,
    cadence: String,
    genre_fit: Vec<String>,
    notes: String,
    active: bool,
}

impl TryFrom<FormatEntryRow> for ContentFormatEntry {
    type Error = ContentEngineError;

    fn try_from(row: FormatEntryRow) -> Result<Self> {
        Ok(Self {
            key: row.key,
            name: row.name,
            category: FormatCategory::parse(&row.category)
                .ok_or(ContentEngineError::UnknownValue("format.category"))?,
            purpose: FormatPurpose::parse(&row.purpose)
                .ok_or(ContentEngineError::UnknownValue("format.purpose"))?,
            effort_standalone: Effort::parse(&row.effort_standalone)
                .ok_or(ContentEngineError::UnknownValue("format.effort_standalone"))?,
            effort_marginal: Effort::parse(&row.effort_marginal)
                .ok_or(ContentEngineError::UnknownValue("format.effort_marginal"))?,
            skill: parse_format_skill(&row.skill)
                .ok_or(ContentEngineError::UnknownValue("format.skill"))?,
            requires: FormatRequirement::parse(&row.requires)
                .ok_or(ContentEngineError::UnknownValue("format.requires"))?,
            distribution: row.distribution,
            cadence: FormatCadence::parse(&row.cadence)
                .ok_or(ContentEngineError::UnknownValue("format.cadence"))?,
            genre_fit: normalize_watch_for(&row.genre_fit),
            notes: row.notes,
            active: row.active,
        })
    }
}

/// How far past release day a plan still counts as material — a lyric
/// video for a single that dropped last week is still executable. Kept
/// narrow on purpose: a quarter-old release is a catalogue item, not a
/// current campaign input.
const RELEASE_MATERIAL_TRAILING_DAYS: i32 = 30;

#[derive(Debug, FromRow)]
struct CapabilityProfileRow {
    skills: Vec<String>,
    has_release_material: bool,
    has_show_material: bool,
}

#[derive(Debug, FromRow)]
struct ProductionEventRow {
    id: Uuid,
    workspace_id: Uuid,
    kind: String,
    title: String,
    scheduled_for: Date,
    event_id: Option<Uuid>,
    status: String,
    notes: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<ProductionEventRow> for ProductionEvent {
    type Error = ContentEngineError;

    fn try_from(row: ProductionEventRow) -> Result<Self> {
        Ok(Self {
            id: ProductionEventId::from_uuid(row.id),
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            kind: ProductionEventKind::parse(&row.kind)
                .ok_or(ContentEngineError::UnknownValue("production_event.kind"))?,
            title: row.title,
            scheduled_for: row.scheduled_for,
            event_id: row.event_id.map(EventId::from_uuid),
            status: ProductionEventStatus::parse(&row.status)
                .ok_or(ContentEngineError::UnknownValue("production_event.status"))?,
            notes: row.notes,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct CapturePlanRow {
    id: Uuid,
    workspace_id: Uuid,
    production_event_id: Uuid,
    items: serde_json::Value,
    assignee_member_id: Option<Uuid>,
    status: String,
    issued_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<CapturePlanRow> for CapturePlan {
    type Error = ContentEngineError;

    fn try_from(row: CapturePlanRow) -> Result<Self> {
        Ok(Self {
            id: CapturePlanId::from_uuid(row.id),
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            production_event_id: ProductionEventId::from_uuid(row.production_event_id),
            items: row.items,
            assignee_member_id: row.assignee_member_id.map(WorkspaceMemberId::from_uuid),
            status: CapturePlanStatus::parse(&row.status)
                .ok_or(ContentEngineError::UnknownValue("capture_plan.status"))?,
            issued_at: row.issued_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct ArcRow {
    id: Uuid,
    workspace_id: Uuid,
    title: String,
    summary: String,
    horizon_start: Option<Date>,
    horizon_end: Option<Date>,
    spine: serde_json::Value,
    evidence: serde_json::Value,
    status: String,
    approved_at: Option<OffsetDateTime>,
    approved_by: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<ArcRow> for Arc {
    type Error = ContentEngineError;

    fn try_from(row: ArcRow) -> Result<Self> {
        Ok(Self {
            id: ArcId::from_uuid(row.id),
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            title: row.title,
            summary: row.summary,
            horizon_start: row.horizon_start,
            horizon_end: row.horizon_end,
            spine: row.spine,
            evidence: row.evidence,
            status: ArcStatus::parse(&row.status)
                .ok_or(ContentEngineError::UnknownValue("arc.status"))?,
            approved_at: row.approved_at,
            approved_by: row.approved_by,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
pub(crate) struct SuggestionRow {
    id: Uuid,
    workspace_id: Uuid,
    arc_id: Option<Uuid>,
    format_key: Option<String>,
    concept: String,
    reason: String,
    evidence: serde_json::Value,
    suggested_after: Option<Date>,
    suggested_before: Option<Date>,
    effort: Option<String>,
    proposed_assignee_member_id: Option<Uuid>,
    distribution_promise: serde_json::Value,
    status: String,
    expires_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<SuggestionRow> for ContentSuggestion {
    type Error = ContentEngineError;

    fn try_from(row: SuggestionRow) -> Result<Self> {
        Ok(Self {
            id: ContentSuggestionId::from_uuid(row.id),
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            arc_id: row.arc_id.map(ArcId::from_uuid),
            format_key: row.format_key,
            concept: row.concept,
            reason: row.reason,
            evidence: row.evidence,
            suggested_after: row.suggested_after,
            suggested_before: row.suggested_before,
            effort: row
                .effort
                .map(|value| {
                    Effort::parse(&value)
                        .ok_or(ContentEngineError::UnknownValue("suggestion.effort"))
                })
                .transpose()?,
            proposed_assignee_member_id: row
                .proposed_assignee_member_id
                .map(WorkspaceMemberId::from_uuid),
            distribution_promise: row.distribution_promise,
            status: SuggestionStatus::parse(&row.status)
                .ok_or(ContentEngineError::UnknownValue("suggestion.status"))?,
            expires_at: row.expires_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct OutcomeRow {
    id: i64,
    workspace_id: Uuid,
    suggestion_id: Uuid,
    outcome: String,
    resolved_at: OffsetDateTime,
    decided_by: Option<String>,
    reason: Option<String>,
    results: serde_json::Value,
}

impl TryFrom<OutcomeRow> for SuggestionOutcome {
    type Error = ContentEngineError;

    fn try_from(row: OutcomeRow) -> Result<Self> {
        Ok(Self {
            id: row.id,
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            suggestion_id: ContentSuggestionId::from_uuid(row.suggestion_id),
            outcome: SuggestionOutcomeKind::parse(&row.outcome)
                .ok_or(ContentEngineError::UnknownValue("outcome.outcome"))?,
            resolved_at: row.resolved_at,
            decided_by: row.decided_by,
            reason: row.reason,
            results: row.results,
        })
    }
}

// ── Inputs ────────────────────────────────────────────────────────────────

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

#[derive(Clone, Debug)]
pub struct NewPeerObservation {
    pub peer_id: PeerId,
    pub observed_at: Date,
    pub platform: String,
    pub kind: String,
    pub fact: String,
    pub url: Option<String>,
    pub metrics: serde_json::Value,
}

/// The demand-side twin of `NewPeerObservation`: a community post fans
/// engaged with, keyed to the `discovery_places` row it surfaced in.
#[derive(Clone, Debug)]
pub struct NewFanObservation {
    pub place_id: Uuid,
    pub observed_at: Date,
    pub platform: String,
    pub kind: String,
    pub fact: String,
    pub url: Option<String>,
    pub metrics: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct NewProductionEvent {
    pub kind: ProductionEventKind,
    pub title: String,
    pub scheduled_for: Date,
    pub event_id: Option<EventId>,
    pub notes: String,
}

#[derive(Clone, Debug)]
pub struct NewCapturePlan {
    pub production_event_id: ProductionEventId,
    pub items: serde_json::Value,
    pub assignee_member_id: Option<WorkspaceMemberId>,
}

#[derive(Clone, Debug)]
pub struct NewArc {
    pub title: String,
    pub summary: String,
    pub horizon_start: Option<Date>,
    pub horizon_end: Option<Date>,
    pub spine: serde_json::Value,
    pub evidence: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct NewSuggestion {
    pub arc_id: Option<ArcId>,
    pub format_key: Option<String>,
    pub concept: String,
    pub reason: String,
    pub evidence: serde_json::Value,
    pub suggested_after: Option<Date>,
    pub suggested_before: Option<Date>,
    pub effort: Option<Effort>,
    pub proposed_assignee_member_id: Option<WorkspaceMemberId>,
    pub distribution_promise: serde_json::Value,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug)]
pub struct NewOutcome {
    pub suggestion_id: ContentSuggestionId,
    pub outcome: SuggestionOutcomeKind,
    pub decided_by: Option<String>,
    pub reason: Option<String>,
    pub results: serde_json::Value,
}

// ── Repository ────────────────────────────────────────────────────────────

impl PostgresContentEngineRepository {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // Peers ────────────────────────────────────────────────────────────────

    /// Inserts a peer in `proposed` or `confirmed` status. The unique index
    /// on `(workspace_id, lower(name))` keeps a name from landing twice;
    /// `ON CONFLICT DO NOTHING` returns `None` on a clash rather than
    /// failing, so a scanner re-proposing the same artist is a no-op.
    pub async fn create_peer(
        &self,
        workspace_id: WorkspaceId,
        peer: &NewPeer,
    ) -> Result<Option<Peer>> {
        let status = if peer.confirmed {
            PeerStatus::Confirmed
        } else {
            PeerStatus::Proposed
        };
        let row = sqlx::query_as::<_, PeerRow>(
            r#"
            INSERT INTO viryaos_peers (
                id, workspace_id, name, handles, tier, watch_for, why,
                proposed_by, status, confirmed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                    CASE WHEN $9 = 'confirmed' THEN now() END)
            ON CONFLICT DO NOTHING
            RETURNING *
            "#,
        )
        .bind(PeerId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(peer.name.trim())
        .bind(&peer.handles)
        .bind(peer.tier.as_str())
        .bind(normalize_watch_for(&peer.watch_for))
        .bind(&peer.why)
        .bind(&peer.proposed_by)
        .bind(status.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Peer::try_from).transpose()
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
            SELECT * FROM viryaos_peers
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
    pub async fn resolve_peer(
        &self,
        workspace_id: WorkspaceId,
        peer_id: PeerId,
        next: PeerStatus,
        rejection_reason: Option<&str>,
    ) -> Result<Peer> {
        if !PeerStatus::Proposed.can_transition_to(next) {
            return Err(ContentEngineError::InvalidTransition);
        }
        let reason = rejection_reason
            .map(str::trim)
            .filter(|value| !value.is_empty());
        match (next, reason) {
            (PeerStatus::Rejected, None) => return Err(ContentEngineError::MissingReason),
            (PeerStatus::Confirmed, Some(_)) => return Err(ContentEngineError::InvalidTransition),
            _ => {}
        }
        let row = sqlx::query_as::<_, PeerRow>(
            r#"
            UPDATE viryaos_peers
            SET status = $3,
                rejection_reason = CASE WHEN $3 = 'rejected' THEN $4 END,
                confirmed_at = CASE WHEN $3 = 'confirmed' THEN now() END,
                updated_at = now()
            WHERE id = $2 AND workspace_id = $1 AND status = 'proposed'
            RETURNING *
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(peer_id.into_uuid())
        .bind(next.as_str())
        .bind(reason)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentEngineError::InvalidTransition)?;
        Peer::try_from(row)
    }

    // Observations ─────────────────────────────────────────────────────────

    /// Records one dated fact. The dedup index makes a repeated sweep
    /// idempotent: the same fact at the same date for the same peer returns
    /// `None` instead of a second row.
    pub async fn record_observation(
        &self,
        workspace_id: WorkspaceId,
        observation: &NewPeerObservation,
    ) -> Result<Option<i64>> {
        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO viryaos_peer_observations (
                workspace_id, peer_id, observed_at, platform, kind, fact, url, metrics
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(observation.peer_id.into_uuid())
        .bind(observation.observed_at)
        .bind(&observation.platform)
        .bind(&observation.kind)
        .bind(&observation.fact)
        .bind(&observation.url)
        .bind(&observation.metrics)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id)
    }

    /// Recent observations across the workspace's peers, newest fact first —
    /// the tail the trend detector reads.
    pub async fn recent_observations(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<PeerObservation>> {
        let rows = sqlx::query_as::<_, ObservationRow>(
            r#"
            SELECT id, workspace_id, peer_id, observed_at, platform, kind, fact, url, metrics
            FROM viryaos_peer_observations
            WHERE workspace_id = $1
            ORDER BY observed_at DESC, id DESC
            LIMIT $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PeerObservation::from).collect())
    }

    /// Records one fan-side fact. Same dedup contract as
    /// `record_observation`: a post that stays hot for a week returns
    /// `None` on the resweep instead of a second row.
    pub async fn record_fan_observation(
        &self,
        workspace_id: WorkspaceId,
        observation: &NewFanObservation,
    ) -> Result<Option<i64>> {
        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO viryaos_fan_observations (
                workspace_id, place_id, observed_at, platform, kind, fact, url, metrics
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(observation.place_id)
        .bind(observation.observed_at)
        .bind(&observation.platform)
        .bind(&observation.kind)
        .bind(&observation.fact)
        .bind(&observation.url)
        .bind(&observation.metrics)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id)
    }

    /// Recent fan-side facts across the workspace's communities — the
    /// demand-side tail the trend detector reads next to peer rows.
    pub async fn recent_fan_observations(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<FanObservation>> {
        let rows = sqlx::query_as::<_, FanObservationRow>(
            r#"
            SELECT id, workspace_id, place_id, observed_at, platform, kind, fact, url, metrics
            FROM viryaos_fan_observations
            WHERE workspace_id = $1
            ORDER BY observed_at DESC, id DESC
            LIMIT $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(FanObservation::from).collect())
    }

    // Catalogue ────────────────────────────────────────────────────────────

    /// The seeded format catalogue — global prior data, deliberately
    /// unscoped. Only `active` entries: learning retires a format by
    /// flipping the flag, never by deleting the prior.
    pub async fn list_format_entries(&self) -> Result<Vec<ContentFormatEntry>> {
        let rows = sqlx::query_as::<_, FormatEntryRow>(
            r#"
            SELECT key, name, category, purpose, effort_standalone, effort_marginal,
                   skill, requires, distribution, cadence, genre_fit, notes, active
            FROM viryaos_content_format_entries
            WHERE active
            ORDER BY category, key
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ContentFormatEntry::try_from).collect()
    }

    /// What this roster can execute right now — the profile every
    /// suggestion is checked against before it is offered.
    ///
    /// Skills are the union over members routable *today* — both
    /// `profile.active` and `member.status = 'active'`, the same pair the
    /// assignment loader requires. A member disabled at the identity level
    /// cannot receive the beat either, so their skills must not count.
    ///
    /// Release material means an active plan whose date is upcoming or no
    /// more than `RELEASE_MATERIAL_TRAILING_DAYS` behind — announce formats
    /// need the upcoming side, sustain formats the trailing side. The check
    /// is deliberately tier-blind (a `filler` plan still counts): whether a
    /// release is *worth* a beat is the suggestion engine's timing call,
    /// not the capability gate's.
    ///
    /// Show material means a published event that has not ended —
    /// `COALESCE(ends_at, starts_at)` so a gig in progress still feeds
    /// `aftermovie`/`tour_diary` capture. A draft is not material because
    /// nothing public points at it.
    pub async fn capability_profile(&self, workspace_id: WorkspaceId) -> Result<CapabilityProfile> {
        let row = sqlx::query_as::<_, CapabilityProfileRow>(
            r#"
            SELECT
                COALESCE(
                    (SELECT array_agg(DISTINCT skill)
                     FROM viryaos_team_profiles p
                     JOIN workspace_members m
                       ON m.workspace_id = p.workspace_id
                      AND m.id = p.member_id
                      AND m.status = 'active'
                     CROSS JOIN unnest(p.skills) AS skill
                     WHERE p.workspace_id = $1 AND p.active
                       AND skill IS NOT NULL),
                    ARRAY[]::text[]
                ) AS skills,
                EXISTS (
                    SELECT 1 FROM viryaos_release_plans
                    WHERE workspace_id = $1 AND active
                      AND release_at >= now() - make_interval(days => $2)
                ) AS has_release_material,
                EXISTS (
                    SELECT 1 FROM events
                    WHERE workspace_id = $1 AND status = 'published'
                      AND COALESCE(ends_at, starts_at) > now()
                ) AS has_show_material
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(RELEASE_MATERIAL_TRAILING_DAYS)
        .fetch_one(&self.pool)
        .await?;
        Ok(CapabilityProfile {
            skills: row
                .skills
                .into_iter()
                .filter_map(|skill| TeamSkill::parse(&skill))
                .collect(),
            has_release_material: row.has_release_material,
            has_show_material: row.has_show_material,
        })
    }

    // Production events ────────────────────────────────────────────────────

    pub async fn create_production_event(
        &self,
        workspace_id: WorkspaceId,
        event: &NewProductionEvent,
    ) -> Result<ProductionEvent> {
        let row = sqlx::query_as::<_, ProductionEventRow>(
            r#"
            INSERT INTO viryaos_production_events (
                id, workspace_id, kind, title, scheduled_for, event_id, notes
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING *
            "#,
        )
        .bind(ProductionEventId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(event.kind.as_str())
        .bind(event.title.trim())
        .bind(event.scheduled_for)
        .bind(event.event_id.map(EventId::into_uuid))
        .bind(&event.notes)
        .fetch_one(&self.pool)
        .await?;
        ProductionEvent::try_from(row)
    }

    /// Scheduled events on or after `from` — the list the capture-plan
    /// issuer walks each cycle.
    pub async fn upcoming_production_events(
        &self,
        workspace_id: WorkspaceId,
        from: Date,
    ) -> Result<Vec<ProductionEvent>> {
        let rows = sqlx::query_as::<_, ProductionEventRow>(
            r#"
            SELECT * FROM viryaos_production_events
            WHERE workspace_id = $1 AND status = 'scheduled' AND scheduled_for >= $2
            ORDER BY scheduled_for ASC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(from)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ProductionEvent::try_from).collect()
    }

    pub async fn set_production_event_status(
        &self,
        workspace_id: WorkspaceId,
        event_id: ProductionEventId,
        expected: ProductionEventStatus,
        next: ProductionEventStatus,
    ) -> Result<ProductionEvent> {
        if !expected.can_transition_to(next) {
            return Err(ContentEngineError::InvalidTransition);
        }
        let row = sqlx::query_as::<_, ProductionEventRow>(
            r#"
            UPDATE viryaos_production_events
            SET status = $4, updated_at = now()
            WHERE id = $2 AND workspace_id = $1 AND status = $3
            RETURNING *
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .bind(expected.as_str())
        .bind(next.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentEngineError::InvalidTransition)?;
        ProductionEvent::try_from(row)
    }

    // Capture plans ────────────────────────────────────────────────────────

    /// Creates the plan in `draft`. `issued_at` stays empty until `issue`
    /// stamps it — a plan the member never saw must be distinguishable from
    /// one they did.
    pub async fn create_capture_plan(
        &self,
        workspace_id: WorkspaceId,
        plan: &NewCapturePlan,
    ) -> Result<CapturePlan> {
        let row = sqlx::query_as::<_, CapturePlanRow>(
            r#"
            INSERT INTO viryaos_capture_plans (
                id, workspace_id, production_event_id, items, assignee_member_id
            )
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
            "#,
        )
        .bind(CapturePlanId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(plan.production_event_id.into_uuid())
        .bind(&plan.items)
        .bind(plan.assignee_member_id.map(WorkspaceMemberId::into_uuid))
        .fetch_one(&self.pool)
        .await?;
        CapturePlan::try_from(row)
    }

    /// Marks the plan issued to its assignee — the reminder machinery keys
    /// off `issued_at`, not `created_at`. The parent event must still be
    /// `scheduled`: issuing a shot list for a cancelled or finished day is
    /// work nobody can do.
    pub async fn issue_capture_plan(
        &self,
        workspace_id: WorkspaceId,
        plan_id: CapturePlanId,
        assignee_member_id: WorkspaceMemberId,
    ) -> Result<CapturePlan> {
        let row = sqlx::query_as::<_, CapturePlanRow>(
            r#"
            UPDATE viryaos_capture_plans
            SET status = 'issued', assignee_member_id = $3,
                issued_at = now(), updated_at = now()
            WHERE id = $2 AND workspace_id = $1 AND status = 'draft'
              AND EXISTS (
                  SELECT 1 FROM viryaos_production_events e
                  WHERE e.workspace_id = $1
                    AND e.id = viryaos_capture_plans.production_event_id
                    AND e.status = 'scheduled'
              )
            RETURNING *
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(plan_id.into_uuid())
        .bind(assignee_member_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentEngineError::InvalidTransition)?;
        CapturePlan::try_from(row)
    }

    pub async fn capture_plans_for_event(
        &self,
        workspace_id: WorkspaceId,
        production_event_id: ProductionEventId,
    ) -> Result<Vec<CapturePlan>> {
        let rows = sqlx::query_as::<_, CapturePlanRow>(
            r#"
            SELECT * FROM viryaos_capture_plans
            WHERE workspace_id = $1 AND production_event_id = $2
            ORDER BY created_at ASC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(production_event_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(CapturePlan::try_from).collect()
    }

    // Arcs ─────────────────────────────────────────────────────────────────

    pub async fn create_arc(&self, workspace_id: WorkspaceId, arc: &NewArc) -> Result<Arc> {
        let row = sqlx::query_as::<_, ArcRow>(
            r#"
            INSERT INTO viryaos_arcs (
                id, workspace_id, title, summary, horizon_start, horizon_end, spine, evidence
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING *
            "#,
        )
        .bind(ArcId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(arc.title.trim())
        .bind(&arc.summary)
        .bind(arc.horizon_start)
        .bind(arc.horizon_end)
        .bind(&arc.spine)
        .bind(&arc.evidence)
        .fetch_one(&self.pool)
        .await?;
        Arc::try_from(row)
    }

    /// Moves an arc along its approval graph. `Approved` also stamps
    /// `approved_at`/`approved_by`; the `WHERE status = expected` clause is
    /// the transition guard, so a double-approve or a stale activation fails
    /// closed instead of overwriting state.
    pub async fn transition_arc(
        &self,
        workspace_id: WorkspaceId,
        arc_id: ArcId,
        expected: ArcStatus,
        next: ArcStatus,
        approved_by: Option<&str>,
    ) -> Result<Arc> {
        if !expected.can_transition_to(next) {
            return Err(ContentEngineError::InvalidTransition);
        }
        let row = sqlx::query_as::<_, ArcRow>(
            r#"
            UPDATE viryaos_arcs
            SET status = $4,
                approved_at = CASE WHEN $4 = 'approved' THEN now() ELSE approved_at END,
                approved_by = CASE WHEN $4 = 'approved' THEN $5 ELSE approved_by END,
                updated_at = now()
            WHERE id = $2 AND workspace_id = $1 AND status = $3
            RETURNING *
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(arc_id.into_uuid())
        .bind(expected.as_str())
        .bind(next.as_str())
        .bind(approved_by)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentEngineError::InvalidTransition)?;
        Arc::try_from(row)
    }

    pub async fn list_arcs(
        &self,
        workspace_id: WorkspaceId,
        status: Option<ArcStatus>,
    ) -> Result<Vec<Arc>> {
        let rows = sqlx::query_as::<_, ArcRow>(
            r#"
            SELECT * FROM viryaos_arcs
            WHERE workspace_id = $1 AND ($2::text IS NULL OR status = $2)
            ORDER BY created_at DESC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(status.map(ArcStatus::as_str))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Arc::try_from).collect()
    }

    // Suggestions ──────────────────────────────────────────────────────────

    /// Raises a suggestion. The caller — the suggestion engine — has already
    /// applied `distribution_promise_is_empty`; an empty promise never
    /// reaches this table.
    pub async fn create_suggestion(
        &self,
        workspace_id: WorkspaceId,
        suggestion: &NewSuggestion,
    ) -> Result<ContentSuggestion> {
        let row = sqlx::query_as::<_, SuggestionRow>(
            r#"
            INSERT INTO viryaos_content_suggestions (
                id, workspace_id, arc_id, format_key, concept, reason, evidence,
                suggested_after, suggested_before, effort,
                proposed_assignee_member_id, distribution_promise, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            RETURNING *
            "#,
        )
        .bind(ContentSuggestionId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(suggestion.arc_id.map(ArcId::into_uuid))
        .bind(&suggestion.format_key)
        .bind(suggestion.concept.trim())
        .bind(&suggestion.reason)
        .bind(&suggestion.evidence)
        .bind(suggestion.suggested_after)
        .bind(suggestion.suggested_before)
        .bind(suggestion.effort.map(Effort::as_str))
        .bind(
            suggestion
                .proposed_assignee_member_id
                .map(WorkspaceMemberId::into_uuid),
        )
        .bind(&suggestion.distribution_promise)
        .bind(suggestion.expires_at)
        .fetch_one(&self.pool)
        .await?;
        ContentSuggestion::try_from(row)
    }

    /// Open suggestions (`raised` + `approved`), newest first — the queue the
    /// operator sees.
    pub async fn list_open_suggestions(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ContentSuggestion>> {
        let rows = sqlx::query_as::<_, SuggestionRow>(
            r#"
            SELECT * FROM viryaos_content_suggestions
            WHERE workspace_id = $1 AND status IN ('raised', 'approved')
            ORDER BY created_at DESC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ContentSuggestion::try_from).collect()
    }

    /// Resolves a suggestion and writes its outcome row in the same
    /// transaction — a decision with no outcome is exactly the hole this
    /// table exists to close, so the two writes cannot separate.
    pub async fn resolve_suggestion(
        &self,
        workspace_id: WorkspaceId,
        outcome: &NewOutcome,
    ) -> Result<SuggestionOutcome> {
        let mut tx = self.pool.begin().await?;
        let suggestion_status = match outcome.outcome {
            SuggestionOutcomeKind::Done | SuggestionOutcomeKind::DoneDifferently => "done",
            SuggestionOutcomeKind::Declined => "declined",
            SuggestionOutcomeKind::Expired => "expired",
        };
        let changed = sqlx::query(
            r#"
            UPDATE viryaos_content_suggestions
            SET status = $3, updated_at = now()
            WHERE id = $2 AND workspace_id = $1 AND status IN ('raised', 'approved')
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(outcome.suggestion_id.into_uuid())
        .bind(suggestion_status)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(ContentEngineError::InvalidTransition);
        }
        let row = sqlx::query_as::<_, OutcomeRow>(
            r#"
            INSERT INTO viryaos_suggestion_outcomes (
                workspace_id, suggestion_id, outcome, decided_by, reason, results
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING *
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(outcome.suggestion_id.into_uuid())
        .bind(outcome.outcome.as_str())
        .bind(&outcome.decided_by)
        .bind(&outcome.reason)
        .bind(&outcome.results)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        SuggestionOutcome::try_from(row)
    }

    /// The outcome history behind a suggestion. Today every suggestion
    /// resolves to exactly one row — the measured-results follow-up the
    /// learning loop writes (3.5b) lands as further rows against the same id.
    pub async fn outcomes_for_suggestion(
        &self,
        workspace_id: WorkspaceId,
        suggestion_id: ContentSuggestionId,
    ) -> Result<Vec<SuggestionOutcome>> {
        let rows = sqlx::query_as::<_, OutcomeRow>(
            r#"
            SELECT * FROM viryaos_suggestion_outcomes
            WHERE workspace_id = $1 AND suggestion_id = $2
            ORDER BY resolved_at DESC, id DESC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(suggestion_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(SuggestionOutcome::try_from).collect()
    }
}
