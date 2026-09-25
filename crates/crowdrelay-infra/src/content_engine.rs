//! Content engine persistence (sprint 3.5a): peer observations, the
//! format catalogue, production events, capture plans, arcs, suggestions
//! and outcomes. The peer write path — operator create, resolve, scanner
//! proposals — lives in `content_peers.rs`.
//!
//! Every query is workspace-scoped — `content_format_entries` is the
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
        FormatRequirement, PeerObservation, ProductionEvent, ProductionEventKind,
        ProductionEventStatus, SuggestionOutcome, SuggestionOutcomeKind, SuggestionStatus,
        normalize_watch_for, parse_format_skill,
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
    /// The one-open-plan-per-day index refused the insert — surfaced as
    /// a domain answer rather than a raw 23505 so the caller can tell a
    /// conflict from a database failure.
    #[error("an open capture plan already exists for the production event")]
    PlanAlreadyOpen,
    /// The name index's partial guard found a live twin. For the operator
    /// that is a conflict, not the scanner's silent dedup — they asked for
    /// a new entry and "already there" is the answer.
    #[error("a live peer already carries this name")]
    PeerNameTaken,
    /// The idempotency key is already spent on a different request — the
    /// operator-action ledger is the record of what a key did, and a reused
    /// key answering with somebody else's action would be a lie.
    #[error("idempotency key already spent on a different request")]
    KeyConflict,
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
pub(crate) struct ArcRow {
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
pub struct NewArc {
    pub title: String,
    pub summary: String,
    pub horizon_start: Option<Date>,
    pub horizon_end: Option<Date>,
    pub spine: serde_json::Value,
    pub evidence: serde_json::Value,
}

// ── Repository ────────────────────────────────────────────────────────────

impl PostgresContentEngineRepository {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
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
            INSERT INTO peer_observations (
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
            INSERT INTO fan_observations (
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

    // Catalogue ────────────────────────────────────────────────────────────

    /// The seeded format catalogue — global prior data, deliberately
    /// unscoped. Only `active` entries: learning retires a format by
    /// flipping the flag, never by deleting the prior.
    pub async fn list_format_entries(&self) -> Result<Vec<ContentFormatEntry>> {
        let rows = sqlx::query_as::<_, FormatEntryRow>(
            r#"
            SELECT key, name, category, purpose, effort_standalone, effort_marginal,
                   skill, requires, distribution, cadence, genre_fit, notes, active
            FROM content_format_entries
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
                     FROM team_profiles p
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
                    SELECT 1 FROM release_plans
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

    /// Scheduled events on or after `from` — the list the capture-plan
    /// issuer walks each cycle.
    pub async fn upcoming_production_events(
        &self,
        workspace_id: WorkspaceId,
        from: Date,
    ) -> Result<Vec<ProductionEvent>> {
        let rows = sqlx::query_as::<_, ProductionEventRow>(
            r#"
            SELECT * FROM production_events
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
}
