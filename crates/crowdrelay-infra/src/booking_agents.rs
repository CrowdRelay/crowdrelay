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
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub approached_at: Option<OffsetDateTime>,
    /// The day a decline stops binding — the door's own answer.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "crowdrelay_domain::iso_date::option"
    )]
    pub refused_until: Option<time::Date>,
    /// An approach already queued or in flight — the second ask the season
    /// exists to prevent.
    pub approach_pending: bool,
    /// An answer queued on the board — the card already exists, so the
    /// row's draft affordance stands down until it lands.
    pub reply_pending: bool,
    /// Their last word is theirs: an answerable reply (`received`,
    /// `positive`, `signed`) nobody has answered. The row's draft affordance
    /// keys off this — a reply waiting is the one thing the season door's
    /// state cannot say.
    pub awaiting_reply: bool,
    /// When their unanswered reply arrived.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub reply_waiting_at: Option<OffsetDateTime>,
    /// What the operator filed their answer as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_waiting_disposition: Option<String>,
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

/// What `request_reply` did — the same queued/replayed contract as the
/// approach, named for its own lane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BookingAgentReplyOutcome {
    Queued { action_id: Uuid },
    Replayed { action_id: Uuid, status: String },
}

/// An agent the wave could not take, with the gate's own sentence. The
/// caller lists these beside the queued card — "three approached, one
/// declined in-season" is the honest summary, and a hidden refusal would
/// read as a silent send.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BookingAgentWaveRefusal {
    pub agent_id: Uuid,
    pub name: String,
    pub reason: String,
}

/// What `request_approach_wave` did. `Queued` carries the wave's one
/// approval card plus the per-agent refusals the batch produced; a wave
/// that queued nobody is `AllRefused`, not an empty card.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BookingAgentWaveOutcome {
    Queued {
        action_id: Uuid,
        wave_id: Uuid,
        queued: usize,
        refused: Vec<BookingAgentWaveRefusal>,
    },
    Replayed {
        action_id: Uuid,
        status: String,
    },
    AllRefused {
        refused: Vec<BookingAgentWaveRefusal>,
    },
}

/// A wave is one card a person reads — bigger and the letters stop being
/// read, which is exactly the failure the season rule exists to prevent.
const MAX_APPROACH_WAVE: usize = 8;

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
                -- The version is the optimistic lock parked approvals pin
                -- to. Bumping it on a re-import that changed nothing would
                -- fail a queued letter for a refresh that never moved the
                -- row — the bump only counts a real change.
                version = CASE WHEN
                    booking_agents.name IS DISTINCT FROM EXCLUDED.name
                    OR booking_agents.agency IS DISTINCT FROM
                       COALESCE(EXCLUDED.agency, booking_agents.agency)
                    OR booking_agents.roster_url IS DISTINCT FROM
                       COALESCE(EXCLUDED.roster_url, booking_agents.roster_url)
                    OR booking_agents.genres IS DISTINCT FROM
                       CASE WHEN cardinality(EXCLUDED.genres) > 0
                            THEN EXCLUDED.genres
                            ELSE booking_agents.genres END
                    OR NOT (booking_agents.metadata @>
                            (EXCLUDED.metadata - 'imported_from'))
                THEN booking_agents.version + 1
                ELSE booking_agents.version END
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
                         AND action.status IN ('awaiting_approval','queued','processing')
                         AND (
                             (action.action_kind = 'booking_agent.approach.request'
                              AND action.subject_id = agent.id)
                             -- A wave carries its agents in the payload —
                             -- the pending flag must see a queued batch
                             -- letter the same way it sees a queued single.
                             OR (action.action_kind = 'booking_agent.approach_wave.request'
                                 AND action.payload->'approaches' @>
                                     jsonb_build_array(jsonb_build_object('agent_id', agent.id::text)))
                         )
                   ) AS approach_pending,
                   EXISTS (
                       SELECT 1 FROM autopilot_actions action
                       WHERE action.workspace_id = agent.workspace_id
                         AND action.context = 'booking_agent'
                         AND action.status IN ('awaiting_approval','queued','processing')
                         AND action.action_kind = 'booking_agent.reply.request'
                         AND action.subject_id = agent.id
                   ) AS reply_pending,
                   waiting.reply_at IS NOT NULL AS awaiting_reply,
                   waiting.reply_at AS reply_waiting_at,
                   waiting.reply_disposition AS reply_waiting_disposition
            FROM booking_agents AS agent
            -- The reply lane's "waiting on you": the newest answerable
            -- reply on the agent's ledger that no outbound touch has
            -- answered. `declined` and `do_not_contact` ask nothing — those
            -- rows already moved the door (`refused_until`, the flag).
            LEFT JOIN LATERAL (
                SELECT their.disposition AS reply_disposition,
                       their.occurred_at AS reply_at
                FROM booking_agent_interactions AS their
                WHERE their.workspace_id = agent.workspace_id
                  AND their.agent_id = agent.id
                  AND their.direction = 'inbound'
                  AND their.phase = 'reply'
                  AND their.disposition IN ('received','positive','signed')
                  AND NOT EXISTS (
                      -- An outbound *reply* answers it; an outbound approach
                      -- letter queued before it arrived is not an answer.
                      SELECT 1
                      FROM booking_agent_interactions AS ours
                      WHERE ours.workspace_id = their.workspace_id
                        AND ours.agent_id = their.agent_id
                        AND ours.direction = 'outbound'
                        AND ours.phase = 'reply'
                        AND ours.occurred_at > their.occurred_at
                  )
                  AND NOT EXISTS (
                      -- The conversation's latest word wins: a newer inbound
                      -- filing — a decline arriving after a positive — is the
                      -- state the board must read, not the stale warm one.
                      SELECT 1
                      FROM booking_agent_interactions AS newer
                      WHERE newer.workspace_id = their.workspace_id
                        AND newer.agent_id = their.agent_id
                        AND newer.direction = 'inbound'
                        AND newer.occurred_at > their.occurred_at
                  )
                ORDER BY their.occurred_at DESC, their.id DESC
                LIMIT 1
            ) AS waiting ON true
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
                reply_pending: row.get("reply_pending"),
                awaiting_reply: row.get("awaiting_reply"),
                reply_waiting_at: row.get("reply_waiting_at"),
                reply_waiting_disposition: row.get("reply_waiting_disposition"),
                version: row.get("version"),
            })
            .collect())
    }

    /// The workspace's current draw readings — the evidence every approach
    /// is pitched on. The band-facing list shows it beside the agents so
    /// "will the gate take this" is answerable before the request is made:
    /// a floor that does not clear today is a fact about the season, not an
    /// error to discover at submit.
    ///
    /// # Errors
    ///
    /// `Database` on any read failure.
    pub async fn draw_evidence(
        &self,
        workspace_id: Uuid,
    ) -> Result<AgentDrawEvidence, BookingAgentError> {
        let mut tx = self.pool.begin().await?;
        let evidence =
            load_agent_draw_evidence(&mut tx, workspace_id, OffsetDateTime::now_utc()).await?;
        tx.commit().await?;
        Ok(evidence)
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
                  AND status IN ('awaiting_approval','queued','processing')
                  AND (
                      (action_kind = 'booking_agent.approach.request' AND subject_id = $2)
                      -- A wave already queued to this agent is the pending
                      -- ask too — subject_id is the wave's, the agents ride
                      -- in the payload.
                      OR (action_kind = 'booking_agent.approach_wave.request'
                          AND payload->'approaches' @>
                              jsonb_build_array(jsonb_build_object('agent_id', $2::text)))
                  )
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

    /// Queues one `awaiting_approval` wave action covering a batch of
    /// agents the operator picked off the gate-state list.
    ///
    /// Same contract as `request_approach`, lifted to the batch: the
    /// advisory lock serializes it with dispatch, every agent is gated and
    /// composed inside the one transaction, and the draw evidence is
    /// measured once so every letter in the wave argues from the same
    /// numbers. An agent who fails the gate does not sink the wave — the
    /// refusal is reported beside the queued card, which is the honest
    /// answer to "pick the batch, see who it could not take". A wave that
    /// could take nobody is refused outright rather than queueing an empty
    /// card.
    ///
    /// The replay key is the request's, as on the single lane: a retried
    /// submit returns the wave it already made.
    ///
    /// # Errors
    ///
    /// `Refused` for a malformed batch (empty, or larger than the wave
    /// bound). `NotFound` is never returned per agent — a selected row
    /// that does not resolve is a refusal on the wave, not a 404.
    pub async fn request_approach_wave(
        &self,
        workspace_id: Uuid,
        agent_ids: &[Uuid],
        note: Option<&str>,
        idempotency_key: &IdempotencyKey,
    ) -> Result<BookingAgentWaveOutcome, BookingAgentError> {
        let mut tx = self.pool.begin().await?;
        let now = OffsetDateTime::now_utc();

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(workspace_id)
            .execute(&mut *tx)
            .await?;

        if let Some((existing, status)) = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, status FROM autopilot_actions WHERE workspace_id = $1 AND idempotency_key = $2 AND action_kind = 'booking_agent.approach_wave.request'",
        )
        .bind(workspace_id)
        .bind(idempotency_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(BookingAgentWaveOutcome::Replayed {
                action_id: existing,
                status,
            });
        }

        // The batch as the operator meant it: deduped, in the order they
        // picked, bounded by what one card can honestly show.
        let mut selected: Vec<Uuid> = Vec::new();
        for agent_id in agent_ids {
            if !selected.contains(agent_id) {
                selected.push(*agent_id);
            }
        }
        if selected.is_empty() {
            return Err(BookingAgentError::Refused(
                "a wave needs at least one agent — select who the season's letters go to"
                    .to_owned(),
            ));
        }
        if selected.len() > MAX_APPROACH_WAVE {
            return Err(BookingAgentError::Refused(format!(
                "a wave of {} is a mail-merge, not a batch — {MAX_APPROACH_WAVE} letters is \
                 what one approval card can honestly show",
                selected.len()
            )));
        }

        let trimmed_note = note.map(str::trim).filter(|value| !value.is_empty());
        if let Some(value) = trimmed_note
            && value.chars().count() > 280
        {
            return Err(BookingAgentError::Refused(
                "the note rides over the numbers — 280 characters is plenty for one line of the band's own words".to_owned(),
            ));
        }

        // One measurement for the whole wave — the workspace's draw is the
        // pitch every letter argues from.
        let evidence = load_agent_draw_evidence(&mut tx, workspace_id, now).await?;
        let sender = crate::gig_outreach::sender_identity(&self.pool, workspace_id)
            .await
            .map_err(|_| BookingAgentError::Refused("the sender could not be read".to_owned()))?;

        let mut prepared: Vec<crowdrelay_application::autopilot::BookingAgentApproachDraft> =
            Vec::new();
        let mut refused: Vec<BookingAgentWaveRefusal> = Vec::new();
        for agent_id in &selected {
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
            .await?;
            let Some(agent) = agent else {
                refused.push(BookingAgentWaveRefusal {
                    agent_id: *agent_id,
                    name: "unknown agent".to_owned(),
                    reason: "not in this workspace's registry".to_owned(),
                });
                continue;
            };
            let agent_name: String = agent.get("name");

            let pending = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM autopilot_actions
                    WHERE workspace_id = $1 AND context = 'booking_agent'
                      AND status IN ('awaiting_approval','queued','processing')
                      AND (
                          (action_kind = 'booking_agent.approach.request' AND subject_id = $2)
                          OR (action_kind = 'booking_agent.approach_wave.request'
                              AND payload->'approaches' @>
                                  jsonb_build_array(jsonb_build_object('agent_id', $2::text)))
                      )
                )
                "#,
            )
            .bind(workspace_id)
            .bind(agent_id)
            .fetch_one(&mut *tx)
            .await?;

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

            let gate = review_agent_approach(&AgentApproachRequest {
                active: agent.get("active"),
                do_not_contact: agent.get("do_not_contact"),
                route_verified: agent
                    .get::<Option<OffsetDateTime>, _>("contact_verified_at")
                    .is_some(),
                approached_at: approached_effective,
                refused_until: agent.get("refused_until"),
                approach_pending: pending,
                evidence,
                now,
            });
            if let Err(refusal) = gate {
                refused.push(BookingAgentWaveRefusal {
                    agent_id: *agent_id,
                    name: agent_name,
                    reason: refusal.message(),
                });
                continue;
            }

            match crowdrelay_domain::approach_letter::compose_booking_agent_letter(
                &crowdrelay_domain::approach_letter::BookingAgentLetterInput {
                    sender: &sender,
                    agent_name: &agent_name,
                    agency: agent.get::<Option<String>, _>("agency").as_deref(),
                    evidence: &evidence,
                    note: trimmed_note,
                },
            ) {
                Ok(draft) => prepared.push(
                    crowdrelay_application::autopilot::BookingAgentApproachDraft {
                        agent_id: BookingAgentId::from_uuid(*agent_id),
                        agent_version: agent.get("version"),
                        agent_name,
                        agency: agent.get("agency"),
                        draft,
                    },
                ),
                Err(refusal) => refused.push(BookingAgentWaveRefusal {
                    agent_id: *agent_id,
                    name: agent_name,
                    reason: refusal.message().to_owned(),
                }),
            }
        }

        if prepared.is_empty() {
            tx.commit().await?;
            return Ok(BookingAgentWaveOutcome::AllRefused { refused });
        }

        let wave_id = Uuid::now_v7();
        let payload = crowdrelay_application::autopilot::AutopilotActionPayload::RequestBookingAgentApproachWave {
            wave_id,
            note: trimmed_note.map(str::to_owned),
            evidence,
            approaches: prepared,
        };
        let action_kind = payload.action_kind();
        let action_class = payload.action_class().as_str();
        let payload_json = serde_json::to_value(&payload)
            .map_err(|_| BookingAgentError::Refused("the wave could not be encoded".to_owned()))?;

        let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
        let decision_key = format!("booking_agent.approach_wave:{}", idempotency_key.as_str());
        let decision_id = match sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
            ) VALUES ($1,$2,$3,'booking_agent','booking_agent_wave',$4,
                      'booking_agent.approach_wave',10000,'require_approval',
                      'Band-initiated booking-agent approach wave',
                      $5,$6,$7,$8,$9)
            ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(&decision_key)
        .bind(wave_id)
        .bind(json!({
            "wave_id": wave_id,
            "agent_ids": selected,
            "note": trimmed_note,
            "evidence": evidence,
            "refused": refused,
        }))
        .bind(json!({"require_approval": true}))
        .bind(payload_json.clone())
        .bind(now)
        .bind(trace.trace_id().into_uuid())
        .fetch_optional(&mut *tx)
        .await?
        {
            Some(id) => id,
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
            ) VALUES ($1,$2,$3,'booking_agent',$4,'booking_agent_wave',$5,$6,$7,
                      'awaiting_approval',$8, now() + INTERVAL '72 hours',
                      $9,$10)
            "#,
        )
        .bind(action_id)
        .bind(workspace_id)
        .bind(decision_id)
        .bind(action_kind)
        .bind(wave_id)
        .bind(idempotency_key.as_str())
        .bind(payload_json)
        .bind(action_class)
        .bind(action_trace.trace_id().into_uuid())
        .bind(action_trace.causation_id().map(|c| c.into_uuid()))
        .execute(&mut *tx)
        .await?;

        let queued = selected.len() - refused.len();
        tx.commit().await?;
        Ok(BookingAgentWaveOutcome::Queued {
            action_id,
            wave_id,
            queued,
            refused,
        })
    }

    /// Queues an answer to an agent who wrote back — the reply lane's
    /// counterpart of `request_approach`.
    ///
    /// The gate is the reply gate, not the approach gate: `SeasonWait` and
    /// the draw floor do not apply because the reply spends nothing — the
    /// agent already answered the season's ask. What still holds is the
    /// row's own truth: the agent must be active, the route confirmed by a
    /// person, `do_not_contact` the line that never moves, and there must
    /// actually be an answerable reply waiting — the newest inbound `reply`
    /// on their ledger with no later outbound touch.
    ///
    /// The draft composes at request time like every letter here — a
    /// scaffold shaped by the filed disposition, because the ledger holds
    /// the verdict and not the reply's words. The operator completes it on
    /// the approval card; dispatch re-runs the same lock before a word
    /// leaves.
    ///
    /// # Errors
    ///
    /// `Refused` carries the band-facing sentence — no reply waiting, the
    /// door closed, the route unconfirmed, or a reply already queued.
    /// `NotFound` means the id is not a booking agent of this workspace.
    pub async fn request_reply(
        &self,
        workspace_id: Uuid,
        agent_id: Uuid,
        idempotency_key: &IdempotencyKey,
    ) -> Result<BookingAgentReplyOutcome, BookingAgentError> {
        let mut tx = self.pool.begin().await?;
        let now = OffsetDateTime::now_utc();

        // Same serialization point as the approach lanes and dispatch: a
        // reply and a send count the same rows under the same lock.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(workspace_id)
            .execute(&mut *tx)
            .await?;

        if let Some((existing, status)) = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, status FROM autopilot_actions WHERE workspace_id = $1 AND idempotency_key = $2 AND action_kind = 'booking_agent.reply.request'",
        )
        .bind(workspace_id)
        .bind(idempotency_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(BookingAgentReplyOutcome::Replayed {
                action_id: existing,
                status,
            });
        }

        let agent = sqlx::query(
            r#"
            SELECT name, agency, active, do_not_contact, contact_verified_at, version
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

        if agent.get::<bool, _>("do_not_contact") {
            return Err(BookingAgentError::Refused(
                "the agent asked not to be contacted".to_owned(),
            ));
        }
        if !agent.get::<bool, _>("active") {
            return Err(BookingAgentError::Refused(
                "the agent is inactive".to_owned(),
            ));
        }
        if agent
            .get::<Option<OffsetDateTime>, _>("contact_verified_at")
            .is_none()
        {
            return Err(BookingAgentError::Refused(
                "the agent's route was never confirmed".to_owned(),
            ));
        }

        // One open reply card per agent — a second click while the first
        // still waits for the board is a refusal, not a duplicate.
        let reply_pending = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM autopilot_actions
                WHERE workspace_id = $1 AND context = 'booking_agent'
                  AND status IN ('awaiting_approval','queued','processing')
                  AND action_kind = 'booking_agent.reply.request'
                  AND subject_id = $2
            )
            "#,
        )
        .bind(workspace_id)
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
        if reply_pending {
            return Err(BookingAgentError::Refused(
                "a reply to this agent is already waiting for approval".to_owned(),
            ));
        }

        // The newest answerable reply nobody answered — the same read the
        // registry list makes, run under the lock so the draft the operator
        // asked for cannot compose against a conversation that moved.
        let waiting = sqlx::query_as::<_, (i64, String)>(
            r#"
            SELECT their.id, their.disposition
            FROM booking_agent_interactions AS their
            WHERE their.workspace_id = $1
              AND their.agent_id = $2
              AND their.direction = 'inbound'
              AND their.phase = 'reply'
              AND their.disposition IN ('received','positive','signed')
              AND NOT EXISTS (
                  -- An outbound *reply* answers it; an approach letter
                  -- queued before it arrived is not an answer.
                  SELECT 1
                  FROM booking_agent_interactions AS ours
                  WHERE ours.workspace_id = their.workspace_id
                    AND ours.agent_id = their.agent_id
                    AND ours.direction = 'outbound'
                    AND ours.phase = 'reply'
                    AND ours.occurred_at > their.occurred_at
              )
              AND NOT EXISTS (
                  -- A newer inbound filing supersedes — the draft must be
                  -- composed against the conversation's latest word.
                  SELECT 1
                  FROM booking_agent_interactions AS newer
                  WHERE newer.workspace_id = their.workspace_id
                    AND newer.agent_id = their.agent_id
                    AND newer.direction = 'inbound'
                    AND newer.occurred_at > their.occurred_at
              )
            ORDER BY their.occurred_at DESC, their.id DESC
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((reply_interaction_id, reply_disposition)) = waiting else {
            return Err(BookingAgentError::Refused(
                "no reply from this agent is waiting on an answer".to_owned(),
            ));
        };

        // The scaffold is composed now — the approver reads the words the
        // agent would get, edits them against the real thread, and a send
        // refuses an empty draft rather than composing after the approval.
        let sender = crate::gig_outreach::sender_identity(&self.pool, workspace_id)
            .await
            .map_err(|_| BookingAgentError::Refused("the sender could not be read".to_owned()))?;
        let draft = crowdrelay_domain::approach_letter::compose_booking_agent_reply_scaffold(
            &crowdrelay_domain::approach_letter::BookingAgentReplyInput {
                sender: &sender,
                agent_name: agent.get("name"),
                agency: agent.get::<Option<String>, _>("agency").as_deref(),
                reply_disposition: &reply_disposition,
            },
        )
        .map_err(|refusal| BookingAgentError::Refused(refusal.message().to_owned()))?;

        let payload =
            crowdrelay_application::autopilot::AutopilotActionPayload::RequestBookingAgentReply {
                agent_id: BookingAgentId::from_uuid(agent_id),
                agent_version: agent.get("version"),
                agent_name: agent.get("name"),
                agency: agent.get("agency"),
                reply_interaction_id,
                reply_disposition: reply_disposition.clone(),
                draft,
            };
        let action_kind = payload.action_kind();
        let action_class = payload.action_class().as_str();
        let payload_json = serde_json::to_value(&payload)
            .map_err(|_| BookingAgentError::Refused("the reply could not be encoded".to_owned()))?;

        let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
        let decision_key = format!("booking_agent.reply:{}", idempotency_key.as_str());
        let decision_id = match sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
            ) VALUES ($1,$2,$3,'booking_agent','booking_agent',$4,
                      'booking_agent.reply',10000,'require_approval',
                      'Band-initiated answer to a booking-agent reply',
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
            "reply_interaction_id": reply_interaction_id,
            "reply_disposition": reply_disposition,
        }))
        .bind(json!({"require_approval": true}))
        .bind(payload_json.clone())
        .bind(now)
        .bind(trace.trace_id().into_uuid())
        .fetch_optional(&mut *tx)
        .await?
        {
            Some(id) => id,
            // The action lookup found nothing under this key but the
            // decision survived — a prior attempt died between the two
            // inserts. Reuse the decision rather than mint a twin.
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
        Ok(BookingAgentReplyOutcome::Queued { action_id })
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

/// Locks the replied-to agent for the send — the reply lane's counterpart
/// of `lock_agent_for_execution`, without the approach gate.
///
/// A reply answers somebody who wrote to us, so the checks that protect an
/// ask — the unspent season, the draw floor, `ApproachPending` — do not
/// apply here: they would refuse every real reply, since the agent being
/// answered was by definition already approached. What still binds is the
/// version pin, `active`, `do_not_contact` and the confirmed route — the
/// lines that do not move because the email is an answer rather than an
/// ask.
pub(crate) async fn lock_agent_reply_for_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    agent_id: BookingAgentId,
    agent_version: i64,
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
        ),
    >(
        r#"
        SELECT name, agency, contact_email, active, do_not_contact, contact_verified_at
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

    let (name, agency, contact_email, active, do_not_contact, contact_verified_at) = agent;
    if do_not_contact {
        return Err(crowdrelay_application::RepositoryError::ConflictBecause(
            "the agent asked not to be contacted",
        ));
    }
    if !active {
        return Err(crowdrelay_application::RepositoryError::ConflictBecause(
            "the agent is inactive",
        ));
    }
    if contact_verified_at.is_none() {
        return Err(crowdrelay_application::RepositoryError::ConflictBecause(
            "the agent's route was never confirmed",
        ));
    }
    Ok(BookingAgentLock {
        name,
        agency,
        contact_email,
        evidence: AgentDrawEvidence::default(),
    })
}

/// Records that the reply went out — the outbound half of the
/// conversation, so the agents board's "waiting on you" closes the loop.
///
/// Phase is `reply`: direction `outbound` + phase `reply` is the pair every
/// "is this conversation waiting on us" read keys off. The season's
/// `approached_at` does not move — an answer is not a new ask.
pub(crate) async fn record_agent_reply_sent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    agent_id: BookingAgentId,
    reply_interaction_id: i64,
    now: OffsetDateTime,
) -> Result<(), crowdrelay_application::RepositoryError> {
    sqlx::query(
        "UPDATE booking_agents SET version = version + 1 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(agent_id.into_uuid())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    sqlx::query(
        r#"INSERT INTO booking_agent_interactions
           (workspace_id, agent_id, direction, phase, disposition, source_key, occurred_at, metadata)
           VALUES ($1,$2,'outbound','reply','none',$3,$4,
                   jsonb_build_object('answers_interaction_id', $5))
           ON CONFLICT (workspace_id, agent_id, source_key) DO NOTHING"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(agent_id.into_uuid())
    .bind(format!("autopilot:reply:{action_id}"))
    .bind(now)
    .bind(reply_interaction_id)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    sqlx::query(
        r#"INSERT INTO reach_events
           (workspace_id, action_id, recipient_kind, recipient_id, channel, template_id, estimated_reach, status, metadata)
           VALUES ($1, $2, 'booking_agent', $3::text, 'email', 'booking_agent_reply', 1, 'sent',
                   jsonb_build_object('kind', 'reply', 'answers_interaction_id', $4))
           ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL DO NOTHING"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(agent_id.into_uuid())
    .bind(reply_interaction_id)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
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
