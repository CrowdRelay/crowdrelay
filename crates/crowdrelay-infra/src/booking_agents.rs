//! The booking-agent registry's band-facing surface (§4h-10, §12-5).
//!
//! A venue sells the band a room, a promoter a night, and an agent sells the
//! band — the ask is representation for a season, which is why the approach
//! is the strictest letter on the outward surface: the proof is the pitch,
//! and an application without real draw numbers is refused rather than sent
//! weaker.
//!
//! The representation feature (`crate::representation`) already answers the
//! same gates for an `outreach_target` of kind `agent` by joining the
//! registry row on address. This module is the registry's own path: the
//! subject is `booking_agents.id`, the ledger is
//! `booking_agent_interactions`, and the reply an operator files
//! here writes `refused_until` and `do_not_contact` on the row every other
//! path reads.
//!
//! The two properties `representation` keeps hold here too:
//!
//! **The address stays hidden.** `list_agents` never selects
//! `contact_email`; the mailbox only enters the emitted event, after the
//! gates, inside the transaction that sends it.
//!
//! **A request is a decision, not a send.** `request_approach` writes a
//! decision + an `awaiting_approval` action; dispatch re-runs every gate
//! under the row lock, so an approval that went stale — a decline landed,
//! the season spent, the draw gone — cannot send.

use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::booking_agent::{
    AgentApproachRequest, AgentDrawEvidence, review_agent_approach,
};
use crowdrelay_domain::trace::TraceContext;
use crowdrelay_domain::{BookingAgentId, WorkspaceId};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum BookingAgentError {
    #[error("booking-agent database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("no such booking agent")]
    NotFound,
    /// The domain gate refused the approach. Carries the band-facing
    /// sentence because the caller's job is to show it, not translate it.
    #[error("{0}")]
    Refused(String),
}

/// An agent as the band sees it: who it is, where they work, and the state
/// of the season door. No `contact_email` — the address is the asset the
/// platform brokers, and the band never needs to see it to decide.
#[derive(Clone, Debug, Serialize)]
pub struct BookingAgentListRow {
    pub agent_id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roster_url: Option<String>,
    pub genres: Vec<String>,
    pub active: bool,
    pub do_not_contact: bool,
    /// Whether the route was ever confirmed — promotion is a human
    /// confirmation and a reply re-confirms it. Exposed as the fact, not
    /// the timestamp: the band needs to know it can be mailed, not when.
    pub route_verified: bool,
    /// When the season's letter went out, if one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approached_at: Option<OffsetDateTime>,
    /// The day a decline stops binding — the door's own answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused_until: Option<time::Date>,
    /// An approach already queued or in flight — the second ask the season
    /// exists to prevent.
    pub approach_pending: bool,
    /// Optimistic-concurrency token the approach request pins to.
    pub version: i64,
}

/// What `request_approach` did. `Replayed` means the same idempotency key
/// already queued an approach — the caller gets the existing action id and
/// its real stored status, which may already be `succeeded` or `failed`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BookingAgentApproachOutcome {
    Queued { action_id: Uuid },
    Replayed { action_id: Uuid, status: String },
}

#[derive(Clone)]
pub struct PostgresBookingAgentRepository {
    pool: PgPool,
}

impl PostgresBookingAgentRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// A registry-workbook seed row, upserted on the address.
    ///
    /// The sheet owns identity — name, agency, roster link, genres — and on
    /// a fresh row its `Active` verdict too. On conflict `active` is left
    /// alone: liveness belongs to the sync's verdict pass, which already
    /// ran on this sheet's staged contacts — the same rule
    /// `promote_beacon_agent` keeps. `approached_at`, `refused_until`,
    /// `do_not_contact` and `contact_verified_at` are the agent's or the
    /// operator's answers and a sheet never writes them.
    ///
    /// Returns `true` when the row was inserted, `false` when the address
    /// was already on file and refreshed.
    pub async fn upsert_seed(
        &self,
        workspace_id: Uuid,
        agent: &crowdrelay_domain::booking_agent_seed::SeededAgent,
        source_file: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut metadata = json!({
            "imported_from": {
                "source": "registry_sheet",
                "file": source_file,
            },
        });
        if let Some(map) = metadata.as_object_mut() {
            if let Some(source_url) = &agent.source_url {
                map.insert("source_url".to_owned(), json!(source_url));
            }
            if let Some(research_date) = &agent.research_date {
                map.insert("research_date".to_owned(), json!(research_date));
            }
            if let Some(notes) = &agent.notes {
                map.insert("notes".to_owned(), json!(notes));
            }
        }
        // `xmax = 0` tells an insert from a conflict-refresh — RETURNING
        // answers the row either way, so the tag is how the caller counts
        // "new" rather than "seen again". `imported_from` is first-source
        // history: excluded from the merge so a refresh never rewrites it.
        sqlx::query_scalar::<_, bool>(
            r#"
            INSERT INTO booking_agents (
              id, workspace_id, name, agency, contact_email, roster_url,
              genres, active, metadata
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (workspace_id, contact_email) DO UPDATE SET
                name = EXCLUDED.name,
                agency = COALESCE(EXCLUDED.agency, booking_agents.agency),
                roster_url = COALESCE(EXCLUDED.roster_url, booking_agents.roster_url),
                genres = CASE WHEN cardinality(EXCLUDED.genres) > 0
                              THEN EXCLUDED.genres
                              ELSE booking_agents.genres END,
                metadata = booking_agents.metadata || (EXCLUDED.metadata - 'imported_from'),
                version = booking_agents.version + 1
            RETURNING (xmax = 0)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(&agent.name)
        .bind(&agent.agency)
        .bind(&agent.email)
        .bind(&agent.roster_url)
        .bind(&agent.genres)
        .bind(agent.active)
        .bind(&metadata)
        .fetch_one(&self.pool)
        .await
    }

    /// The registry as the band sees it — every agent the screened intake
    /// promoted, contactable and season-open first. The address column is
    /// deliberately absent from the select list.
    pub async fn list_agents(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<BookingAgentListRow>, BookingAgentError> {
        let rows = sqlx::query(
            r#"
            SELECT agent.id, agent.name, agent.agency, agent.roster_url,
                   agent.genres, agent.active, agent.do_not_contact,
                   agent.contact_verified_at, agent.approached_at,
                   agent.refused_until, agent.version,
                   EXISTS (
                       SELECT 1 FROM autopilot_actions action
                       WHERE action.workspace_id = agent.workspace_id
                         AND action.context = 'booking_agent'
                         AND action.action_kind = 'booking_agent.approach.request'
                         AND action.subject_id = agent.id
                         AND action.status IN ('awaiting_approval','queued','processing')
                   ) AS approach_pending
            FROM booking_agents AS agent
            WHERE agent.workspace_id = $1
            ORDER BY agent.do_not_contact, NOT agent.active,
                     agent.approached_at ASC NULLS FIRST, agent.name ASC
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| BookingAgentListRow {
                agent_id: row.get("id"),
                name: row.get("name"),
                agency: row.get("agency"),
                roster_url: row.get("roster_url"),
                genres: row.get("genres"),
                active: row.get("active"),
                do_not_contact: row.get("do_not_contact"),
                route_verified: row
                    .get::<Option<OffsetDateTime>, _>("contact_verified_at")
                    .is_some(),
                approached_at: row.get("approached_at"),
                refused_until: row.get("refused_until"),
                approach_pending: row.get("approach_pending"),
                version: row.get("version"),
            })
            .collect())
    }

    /// Queues a band-initiated agent application as an `awaiting_approval`
    /// action.
    ///
    /// The domain gate runs first under the workspace advisory lock — the
    /// same lock dispatch takes — so a request that would be refused at
    /// send time is refused here, with the band-facing reason. The draw
    /// snapshot is measured inside the same locked transaction and written
    /// into the payload: the approval screen and the letter argue from the
    /// same numbers. The insert is idempotent on `idempotency_key`: a
    /// retried form submit returns the action it already queued.
    ///
    /// # Errors
    ///
    /// `Refused` carries the gate's sentence. `NotFound` means the id is
    /// not a booking agent of this workspace.
    pub async fn request_approach(
        &self,
        workspace_id: Uuid,
        agent_id: Uuid,
        note: Option<&str>,
        idempotency_key: &IdempotencyKey,
    ) -> Result<BookingAgentApproachOutcome, BookingAgentError> {
        let mut tx = self.pool.begin().await?;
        let now = OffsetDateTime::now_utc();

        // Same serialization point as dispatch: requests and sends count the
        // same rows under the same lock.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(workspace_id)
            .execute(&mut *tx)
            .await?;

        // A replay is answered before any gate runs: the key identifies this
        // request, and a retried submit returns the queued row rather than a
        // refusal — the refusal below is for a *different* approach to the
        // same agent.
        if let Some((existing, status)) = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, status FROM autopilot_actions WHERE workspace_id = $1 AND idempotency_key = $2 AND action_kind = 'booking_agent.approach.request'",
        )
        .bind(workspace_id)
        .bind(idempotency_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(BookingAgentApproachOutcome::Replayed {
                action_id: existing,
                status,
            });
        }

        let agent = sqlx::query(
            r#"
            SELECT name, agency, active, do_not_contact, contact_verified_at,
                   approached_at, refused_until, version
            FROM booking_agents
            WHERE workspace_id = $1 AND id = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(BookingAgentError::NotFound)?;

        let approach_pending = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM autopilot_actions
                WHERE workspace_id = $1 AND context = 'booking_agent'
                  AND action_kind = 'booking_agent.approach.request'
                  AND subject_id = $2
                  AND status IN ('awaiting_approval','queued','processing')
            )
            "#,
        )
        .bind(workspace_id)
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;

        // The ledger counts beside the registry stamp: a send recorded only
        // in `booking_agent_interactions` still spent the season.
        let ledger_approach = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
            r#"
            SELECT max(occurred_at) FROM booking_agent_interactions
            WHERE workspace_id = $1 AND agent_id = $2
              AND direction = 'outbound' AND phase = 'approach'
            "#,
        )
        .bind(workspace_id)
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
        let approached_effective = match (
            agent.get::<Option<OffsetDateTime>, _>("approached_at"),
            ledger_approach,
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };

        let trimmed_note = note.map(str::trim).filter(|value| !value.is_empty());
        if let Some(value) = trimmed_note
            && value.chars().count() > 280
        {
            return Err(BookingAgentError::Refused(
                "the note rides over the numbers — 280 characters is plenty for one line of the band's own words".to_owned(),
            ));
        }

        // The pitch is the numbers: measured inside the lock so what the
        // approver sees is what the letter will cite.
        let evidence = load_agent_draw_evidence(&mut tx, workspace_id, now).await?;

        review_agent_approach(&AgentApproachRequest {
            active: agent.get("active"),
            do_not_contact: agent.get("do_not_contact"),
            route_verified: agent
                .get::<Option<OffsetDateTime>, _>("contact_verified_at")
                .is_some(),
            approached_at: approached_effective,
            refused_until: agent.get("refused_until"),
            approach_pending,
            evidence,
            now,
        })
        .map_err(|refusal| BookingAgentError::Refused(refusal.message()))?;

        // The letter is composed now — the operator approves the sentences
        // the agent will read, not a description of them. An action queued
        // before this carried no draft and dispatch refuses it rather than
        // letting anything write on the band's behalf after the approval.
        let sender = crate::gig_outreach::sender_identity(&self.pool, workspace_id)
            .await
            .map_err(|_| BookingAgentError::Refused("the sender could not be read".to_owned()))?;
        let draft = crowdrelay_domain::approach_letter::compose_booking_agent_letter(
            &crowdrelay_domain::approach_letter::BookingAgentLetterInput {
                sender: &sender,
                agent_name: agent.get("name"),
                agency: agent.get::<Option<String>, _>("agency").as_deref(),
                evidence: &evidence,
                note: trimmed_note,
            },
        )
        .map_err(|refusal| BookingAgentError::Refused(refusal.message().to_owned()))?;

        let payload = crowdrelay_application::autopilot::AutopilotActionPayload::RequestBookingAgentApproach {
            agent_id: BookingAgentId::from_uuid(agent_id),
            agent_version: agent.get("version"),
            agent_name: agent.get("name"),
            agency: agent.get("agency"),
            note: trimmed_note.map(str::to_owned),
            evidence,
            draft,
        };
        let action_kind = payload.action_kind();
        let action_class = payload.action_class().as_str();
        let payload_json = serde_json::to_value(&payload).map_err(|_| {
            BookingAgentError::Refused("the approach could not be encoded".to_owned())
        })?;

        let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
        let decision_key = format!("booking_agent.approach:{}", idempotency_key.as_str());
        let decision_id = match sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
            ) VALUES ($1,$2,$3,'booking_agent','booking_agent',$4,
                      'booking_agent.approach',10000,'require_approval',
                      'Band-initiated booking-agent approach',
                      $5,$6,$7,$8,$9)
            ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(&decision_key)
        .bind(agent_id)
        .bind(json!({
            "agent_id": agent_id,
            "note": trimmed_note,
            "approach_pending": approach_pending,
            "evidence": evidence,
        }))
        .bind(json!({"require_approval": true}))
        .bind(payload_json.clone())
        .bind(now)
        .bind(trace.trace_id().into_uuid())
        .fetch_optional(&mut *tx)
        .await?
        {
            Some(id) => id,
            // A decision under this key already exists — the action lookup
            // above found no matching action, so a prior attempt died between
            // the two inserts. Reuse the decision and queue the action it was
            // meant to carry.
            None => sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM autopilot_decisions WHERE workspace_id = $1 AND decision_key = $2",
            )
            .bind(workspace_id)
            .bind(&decision_key)
            .fetch_one(&mut *tx)
            .await?,
        };

        let action_id = Uuid::now_v7();
        let action_trace = TraceContext::for_action(
            WorkspaceId::from_uuid(workspace_id),
            trace.trace_id(),
            action_id,
            Some(decision_id),
        );
        sqlx::query(
            r#"
            INSERT INTO autopilot_actions (
                id, workspace_id, decision_id, context, action_kind,
                subject_kind, subject_id, idempotency_key, payload, status,
                action_class, approval_expires_at, trace_id, causation_id
            ) VALUES ($1,$2,$3,'booking_agent',$4,'booking_agent',$5,$6,$7,
                      'awaiting_approval',$8, now() + INTERVAL '72 hours',
                      $9,$10)
            "#,
        )
        .bind(action_id)
        .bind(workspace_id)
        .bind(decision_id)
        .bind(action_kind)
        .bind(agent_id)
        .bind(idempotency_key.as_str())
        .bind(payload_json)
        .bind(action_class)
        .bind(action_trace.trace_id().into_uuid())
        .bind(action_trace.causation_id().map(|c| c.into_uuid()))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(BookingAgentApproachOutcome::Queued { action_id })
    }
}

// ── The dispatch half ────────────────────────────────────────────────
//
// Called from `autopilot::actions_execution` when a queued approach action
// fires. Both functions run inside the dispatch transaction; they return the
// application's `RepositoryError` because the dispatcher's failure ladder
// reads it, not because this layer owns it.

fn map_sqlx(error: sqlx::Error) -> crowdrelay_application::RepositoryError {
    use crowdrelay_application::RepositoryError as E;
    match crate::database::classify_sqlx_error(&error) {
        crate::database::SqlxErrorClass::NotFound => E::NotFound,
        crate::database::SqlxErrorClass::Conflict => E::Conflict,
        crate::database::SqlxErrorClass::Unavailable => E::Unavailable,
        crate::database::SqlxErrorClass::Unexpected => E::Unexpected,
    }
}

/// The draw readings an agent application is made of (§4h-10): the
/// workspace's own played shows in the last twelve months, the paid tickets
/// behind them, the buyers counted as people rather than orders, whether
/// they came back, how far the reach went, and the best single night.
///
/// Every figure is a measured count, so each is `Some` when the read ran —
/// a workspace with no played shows measures zero, which is the honest
/// answer the floor refuses on. `None` stays reserved for a reading the
/// system could not take, because inventing a zero for it would decide the
/// same question with a different lie.
pub(crate) async fn load_agent_draw_evidence(
    executor: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<AgentDrawEvidence, sqlx::Error> {
    sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(
        r#"
        WITH played AS (
            SELECT event.id, event.city_id
            FROM events AS event
            WHERE event.workspace_id = $1
              AND event.status IN ('published','completed')
              AND event.starts_at >= $2 - INTERVAL '12 months'
              AND event.starts_at <= $2
        ),
        paid_orders AS (
            SELECT orders.id, orders.buyer_email, sale.event_id,
                   (SELECT COALESCE(sum(item.quantity), 0)
                      FROM ticket_order_items AS item
                     WHERE item.workspace_id = orders.workspace_id
                       AND item.ticket_order_id = orders.id) AS tickets
            FROM ticket_orders AS orders
            JOIN ticket_sales AS sale
              ON sale.workspace_id = orders.workspace_id
             AND sale.id = orders.ticket_sale_id
            JOIN played ON played.id = sale.event_id
            WHERE orders.workspace_id = $1
              AND orders.status IN ('paid','partially_refunded')
        )
        SELECT
            (SELECT count(*) FROM played)::bigint,
            (SELECT COALESCE(sum(tickets), 0) FROM paid_orders)::bigint,
            (SELECT count(DISTINCT buyer_email) FROM paid_orders)::bigint,
            (SELECT count(*) FROM (
                SELECT buyer_email FROM paid_orders
                GROUP BY buyer_email HAVING count(*) >= 2
            ) repeats)::bigint,
            (SELECT count(DISTINCT city_id) FROM played
              WHERE city_id IS NOT NULL)::bigint,
            (SELECT COALESCE(max(per_show.total), 0) FROM (
                SELECT sum(tickets) AS total FROM paid_orders GROUP BY event_id
            ) per_show)::bigint
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_one(&mut **executor)
    .await
    .map(
        |(shows, paid, buyers, repeats, cities, best_show)| AgentDrawEvidence {
            shows_played_12m: Some(shows),
            paid_tickets_12m: Some(paid),
            distinct_buyers_12m: Some(buyers),
            repeat_buyers_12m: Some(repeats),
            cities_reached_12m: Some(cities),
            best_show_paid_tickets_12m: Some(best_show),
            as_of: Some(now),
        },
    )
}

/// An agent locked for dispatch — the fields the letter is addressed to,
/// plus the draw snapshot the lock's own gate re-measured. The lock is the
/// moment the gates were re-run, so the evidence it carries is the number
/// that just cleared the floor, not a stale payload figure.
pub(crate) struct BookingAgentLock {
    pub name: String,
    pub agency: Option<String>,
    pub contact_email: String,
    pub evidence: AgentDrawEvidence,
}

/// Locks a booking agent for an approach and re-runs every request-time
/// gate: standing (do-not-contact, active, verified route), the season door
/// (`refused_until`), the season spend (`approached_at` and the approach
/// ledger), and the draw floor — re-measured inside this transaction so a
/// stale snapshot cannot send.
///
/// `approach_pending` is deliberately not re-checked here: the action being
/// dispatched *is* the pending approach, and a second one cannot exist —
/// the request gate takes the same advisory lock before it inserts. Two
/// racing dispatches serialize on the row lock and the loser sees the
/// winner's `approached_at` stamp, which the season gate refuses on.
///
/// Re-gating matters because the flags can move between approval and
/// dispatch: an operator files a decline or a do-not-contact, the agent
/// goes inactive, the season's numbers change.
pub(crate) async fn lock_agent_for_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    agent_id: BookingAgentId,
    agent_version: i64,
    now: OffsetDateTime,
) -> Result<BookingAgentLock, crowdrelay_application::RepositoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(workspace_id.into_uuid())
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;

    let agent = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            String,
            bool,
            bool,
            Option<OffsetDateTime>,
            Option<OffsetDateTime>,
            Option<time::Date>,
        ),
    >(
        r#"
        SELECT name, agency, contact_email, active, do_not_contact,
               contact_verified_at, approached_at, refused_until
        FROM booking_agents
        WHERE workspace_id = $1 AND id = $2 AND version = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(agent_id.into_uuid())
    .bind(agent_version)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(crowdrelay_application::RepositoryError::Conflict)?;

    // The interaction ledger counts beside `approached_at` — a send the
    // registry stamp missed still spends the season, so the gate sees the
    // later of the two records of the knock.
    let ledger_approach = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        r#"
        SELECT max(occurred_at) FROM booking_agent_interactions
        WHERE workspace_id = $1 AND agent_id = $2
          AND direction = 'outbound' AND phase = 'approach'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(agent_id.into_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    let evidence = load_agent_draw_evidence(tx, workspace_id.into_uuid(), now)
        .await
        .map_err(map_sqlx)?;

    let (_, _, _, active, do_not_contact, contact_verified_at, approached_at, refused_until) =
        &agent;
    let approached_effective = match (*approached_at, ledger_approach) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    crowdrelay_domain::booking_agent::review_agent_approach(&AgentApproachRequest {
        active: *active,
        do_not_contact: *do_not_contact,
        route_verified: contact_verified_at.is_some(),
        approached_at: approached_effective,
        refused_until: *refused_until,
        approach_pending: false,
        evidence,
        now,
    })
    .map_err(|refusal| {
        // The reason survives the dispatch ladder: a parked action that fails
        // on a moved gate must say which gate moved, or the failure reads as
        // a broken button.
        use crowdrelay_domain::booking_agent::AgentApproachRefusal as R;
        crowdrelay_application::RepositoryError::ConflictBecause(match refusal {
            R::DoNotContact => "the agent asked not to be contacted",
            R::SeasonalRefusal { .. } => {
                "the agent's refusal still binds — the season door is closed"
            }
            R::SeasonWait { .. } => "the agent was already approached this season",
            R::InsufficientDraw { .. } => "insufficient_draw_evidence",
            R::Inactive => "the agent is inactive",
            R::RouteUnverified => "the agent's route was never confirmed",
            R::ApproachPending => "an approach is already in flight",
        })
    })?;

    Ok(BookingAgentLock {
        name: agent.0,
        agency: agent.1,
        contact_email: agent.2,
        evidence,
    })
}

/// Records that an approach went out: the registry's `approached_at` (the
/// season's spend), the outbound `approach` interaction, and the
/// reach-ledger event — all inside the dispatch transaction, so a letter
/// that never emitted never spends the season.
pub(crate) async fn record_approach_sent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    agent_id: BookingAgentId,
    now: OffsetDateTime,
) -> Result<(), crowdrelay_application::RepositoryError> {
    sqlx::query(
        "UPDATE booking_agents SET approached_at = $3, version = version + 1 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(agent_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    sqlx::query(
        r#"INSERT INTO booking_agent_interactions
           (workspace_id, agent_id, direction, phase, disposition, source_key, occurred_at)
           VALUES ($1,$2,'outbound','approach','none',$3,$4)
           ON CONFLICT (workspace_id, agent_id, source_key) DO NOTHING"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(agent_id.into_uuid())
    .bind(format!("autopilot:{action_id}"))
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    sqlx::query(
        r#"INSERT INTO reach_events
           (workspace_id, action_id, recipient_kind, recipient_id, channel, template_id, estimated_reach, status, metadata)
           VALUES ($1, $2, 'booking_agent', $3::text, 'email', 'booking_agent_approach', 1, 'sent',
                   jsonb_build_object('kind', 'approach'))
           ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL DO NOTHING"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(agent_id.into_uuid())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
