//! Durable storage for free-reach waves, and the numbers a pitch may claim.
//!
//! Three things here are load-bearing and none is visible from the signatures.
//!
//! * **A wave's size is counted from the action ledger.** The same rule the
//!   plays learned: a queued or awaiting-approval pitch has already taken a
//!   place in the wave even though nobody has received anything, and counting
//!   only sent ones would let a wave draft past the budget it was sized
//!   against.
//! * **Approval is one statement over the whole wave.** Approving pitch by
//!   pitch inside a loop leaves a half-approved batch behind the first error,
//!   which is exactly the state an operator cannot reason about.
//! * **The evidence packet omits what it cannot read.** A zero the agent
//!   invented reads exactly like a zero it measured, and this exists so a pitch
//!   carries numbers rather than adjectives.

use super::*;

#[derive(sqlx::FromRow)]
struct WaveRow {
    id: Uuid,
    anchor_kind: String,
    anchor_id: Uuid,
    anchor_at: OffsetDateTime,
    target_kind: String,
    state: String,
    capacity: i32,
    opened_at: OffsetDateTime,
    pitches: i64,
    anchor_active: bool,
    eligible_targets: i64,
}

#[derive(sqlx::FromRow)]
struct WaveAnchorRow {
    anchor_kind: String,
    anchor_id: Uuid,
    anchor_at: OffsetDateTime,
    target_kind: String,
    hours_until: i64,
    eligible_targets: i64,
}

/// Every wave the cycle still has something to say about, with the two outside
/// facts its state machine needs: how many pitches it holds and whether its
/// anchor is still a reason to pitch.
const WAVE_SNAPSHOT_SQL: &str = r#"
SELECT
    wave.id,
    wave.anchor_kind,
    wave.anchor_id,
    wave.anchor_at,
    wave.target_kind,
    wave.state,
    wave.capacity,
    wave.opened_at,
    (
        -- Committed, not delivered. A pitch awaiting approval has already taken
        -- its place in the wave.
        SELECT count(*)::bigint
        FROM autopilot_actions AS action
        WHERE action.workspace_id = wave.workspace_id
          AND action.context = 'outreach'
          AND action.status <> 'cancelled'
          AND action.payload->>'wave_id' = wave.id::text
    ) AS pitches,
    CASE wave.anchor_kind
        WHEN 'release' THEN EXISTS (
            SELECT 1
            FROM release_plans AS plan
            WHERE plan.workspace_id = wave.workspace_id
              AND plan.id = wave.anchor_id
              AND plan.active
        )
        -- A catalogue or threads season stands until its own date passes.
        WHEN 'catalogue' THEN true
        WHEN 'threads' THEN true
        ELSE COALESCE((
            SELECT event.status = 'published'
            FROM events AS event
            WHERE event.workspace_id = wave.workspace_id
              AND event.id = wave.anchor_id
        ), false)
    END AS anchor_active,
    CASE WHEN wave.anchor_kind = 'threads' THEN (
        -- The threads lane counts a different pool: contacts whose latest
        -- message is the act's own hand-sent one, inside the follow-up
        -- window, unanswered since — and due now, since a thread younger
        -- than the silence floor cannot draft this cycle.
        SELECT count(*)::bigint
        FROM outreach_targets AS target
        WHERE target.workspace_id = wave.workspace_id
          AND target.target_kind = wave.target_kind
          AND target.active
          AND target.verified
          AND target.accepts_outreach
          AND NOT target.do_not_contact
          AND COALESCE(target.last_reply_disposition::text,'none') NOT IN ('received','positive','declined')
          AND EXISTS (
              SELECT 1
              FROM outreach_interactions AS last_out
              WHERE last_out.workspace_id = target.workspace_id
                AND last_out.target_id = target.id
                AND last_out.direction = 'outbound'
                AND last_out.opportunity_id IS NULL
                AND last_out.occurred_at <= now() - interval '10 days'
                AND last_out.occurred_at > now() - interval '60 days'
                AND NOT EXISTS (
                    SELECT 1
                    FROM outreach_interactions AS later
                    WHERE later.workspace_id = target.workspace_id
                      AND later.target_id = target.id
                      AND later.occurred_at > last_out.occurred_at
                )
          )
    ) ELSE (
        SELECT count(*)::bigint
        FROM outreach_targets AS target
        WHERE target.workspace_id = wave.workspace_id
          AND target.target_kind = wave.target_kind
          AND target.active
          AND target.verified
          AND target.accepts_outreach
          -- The send path filters on this too. Counting a suppressed target
          -- as eligible makes the plan promise reach the dispatch will not
          -- deliver, and the gap only shows up as a wave that under-performs
          -- its own forecast.
          AND NOT target.do_not_contact
          AND COALESCE(target.last_reply_disposition::text,'none') NOT IN ('received','positive','declined')
    ) END AS eligible_targets
FROM outreach_waves AS wave
WHERE wave.workspace_id = $1
  AND wave.settled_at IS NULL
ORDER BY wave.anchor_at
LIMIT $2
"#;

/// Releases and shows with no wave of a given kind yet.
///
/// One row per (anchor, kind) pair the workspace could pitch around. The kinds
/// are the free-reach ones: a support slot is a booking conversation and a
/// playlist has its own phase, and neither belongs in a press wave.
const WAVE_ANCHOR_SQL: &str = r#"
WITH anchors AS (
    -- Cast, because an unadorned literal in a CTE comes back as `unknown` and the
    -- decoder has nothing to turn into a string.
    SELECT 'release'::text AS anchor_kind, plan.id AS anchor_id, plan.release_at AS anchor_at
    FROM release_plans AS plan
    WHERE plan.workspace_id = $1
      AND plan.active
      -- A filler release owes no vertical — the free-reach waves are part
      -- of it, and the tier is the band's call, not a wave that went missing.
      AND plan.tier <> 'filler'
      AND plan.release_at > $2
    UNION ALL
    SELECT 'event'::text, event.id, event.starts_at
    FROM events AS event
    WHERE event.workspace_id = $1
      AND event.status = 'published'
      AND event.starts_at > $2
    UNION ALL
    -- The catalogue season: only while the catalogue is the pitch (no active
    -- release plan with a listen link), one id per catalogue item per month,
    -- closing a week into the next month so a wave opened on the last day
    -- still has its drafting window. See `outreach_supply::outreach_pitch`,
    -- whose catalogue choice this repeats.
    SELECT 'catalogue'::text,
           md5(pitch.id::text || ':' || to_char($2 AT TIME ZONE 'UTC', 'YYYY-MM'))::uuid,
           date_trunc('month', $2 AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
               + interval '1 month' + interval '8 days'
    FROM (
        SELECT source.id
        FROM content_sources AS source
        WHERE source.workspace_id = $1
          AND source.source_kind = 'release'
          AND source.metadata->>'release_type' IN ('album', 'ep', 'single')
          AND source.metadata->>'released_at' ~ '^[0-9]{1,12}$'
          AND COALESCE(btrim(source.metadata->>'url'), '') ~* '^https?://'
          AND btrim(source.title) <> ''
          AND NOT EXISTS (
              SELECT 1 FROM release_plans AS plan
              WHERE plan.workspace_id = $1 AND plan.active
                AND plan.listen_url IS NOT NULL AND btrim(plan.listen_url) <> ''
                AND btrim(plan.title) <> ''
          )
        ORDER BY (source.metadata->>'released_at')::bigint DESC, source.id
        LIMIT 1
    ) AS pitch
    UNION ALL
    -- The threads season: one wave per kind per month for the threads the
    -- act opened by hand and nobody answered. There is no record to anchor
    -- on, so the calendar month is the anchor — closing a week into the next
    -- one, the same overhang the catalogue season gets, so a wave opened on
    -- the last day still has its drafting window. The EXISTS keeps a
    -- workspace with nothing due from paying an empty anchor for it.
    SELECT 'threads'::text,
           md5('threads:' || to_char($2 AT TIME ZONE 'UTC', 'YYYY-MM'))::uuid,
           date_trunc('month', $2 AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
               + interval '1 month' + interval '8 days'
    WHERE EXISTS (
        SELECT 1
        FROM outreach_targets AS target
        WHERE target.workspace_id = $1
          AND target.active
          AND target.verified
          AND target.accepts_outreach
          AND NOT target.do_not_contact
          AND COALESCE(target.last_reply_disposition::text,'none') NOT IN ('received','positive','declined')
          AND EXISTS (
              SELECT 1
              FROM outreach_interactions AS last_out
              WHERE last_out.workspace_id = target.workspace_id
                AND last_out.target_id = target.id
                AND last_out.direction = 'outbound'
                AND last_out.opportunity_id IS NULL
                -- Due now: under the silence floor the evaluator only
                -- holds, so a younger thread must not open the season.
                AND last_out.occurred_at <= $2 - interval '10 days'
                AND last_out.occurred_at > $2 - interval '60 days'
                AND NOT EXISTS (
                    SELECT 1
                    FROM outreach_interactions AS later
                    WHERE later.workspace_id = target.workspace_id
                      AND later.target_id = target.id
                      AND later.occurred_at > last_out.occurred_at
                )
          )
    )
),
kinds AS (
    -- Pitch waves cover the free-reach kinds. A threads wave covers every
    -- kind the act can hold a thread with — a support slot or a playlist
    -- curator can owe a reply the same as an editor can. Representation
    -- kinds stay out on both lists: agents and labels are approached through
    -- the listing path, never pitched, and the nudge is still a pitch's lane.
    SELECT unnest(ARRAY['radio', 'press', 'creator', 'endorsement', 'media_patronage']::text[])
        AS target_kind, false AS thread_lane
    UNION ALL
    SELECT unnest(ARRAY['playlist', 'radio', 'press', 'creator', 'support_slot', 'endorsement', 'media_patronage']::text[]),
        true
)
SELECT
    anchors.anchor_kind,
    anchors.anchor_id,
    anchors.anchor_at,
    kinds.target_kind,
    FLOOR(EXTRACT(EPOCH FROM (anchors.anchor_at - $2)) / 3600)::bigint AS hours_until,
    (
        CASE WHEN anchors.anchor_kind = 'threads' THEN (
            SELECT count(*)::bigint
            FROM outreach_targets AS target
            WHERE target.workspace_id = $1
              AND target.target_kind = kinds.target_kind
              AND target.active
              AND target.verified
              AND target.accepts_outreach
              AND NOT target.do_not_contact
              AND COALESCE(target.last_reply_disposition::text,'none') NOT IN ('received','positive','declined')
              AND EXISTS (
                  SELECT 1
                  FROM outreach_interactions AS last_out
                  WHERE last_out.workspace_id = target.workspace_id
                    AND last_out.target_id = target.id
                    AND last_out.direction = 'outbound'
                    AND last_out.opportunity_id IS NULL
                    AND last_out.occurred_at <= $2 - interval '10 days'
                    AND last_out.occurred_at > $2 - interval '60 days'
                    AND NOT EXISTS (
                        SELECT 1
                        FROM outreach_interactions AS later
                        WHERE later.workspace_id = target.workspace_id
                          AND later.target_id = target.id
                          AND later.occurred_at > last_out.occurred_at
                    )
              )
        ) ELSE (
            SELECT count(*)::bigint
            FROM outreach_targets AS target
            WHERE target.workspace_id = $1
              AND target.target_kind = kinds.target_kind
              AND target.active
              AND target.verified
              AND target.accepts_outreach
              -- The send path filters on this too. Counting a suppressed
              -- target as eligible makes the plan promise reach the dispatch
              -- will not deliver, and the gap only shows up as a wave that
              -- under-performs its own forecast.
              AND NOT target.do_not_contact
              AND COALESCE(target.last_reply_disposition::text,'none') NOT IN ('received','positive','declined')
        ) END
    ) AS eligible_targets
FROM anchors
JOIN kinds ON kinds.thread_lane = (anchors.anchor_kind = 'threads')
WHERE NOT EXISTS (
    SELECT 1
    FROM outreach_waves AS wave
    WHERE wave.workspace_id = $1
      AND wave.anchor_kind = anchors.anchor_kind
      AND wave.anchor_id = anchors.anchor_id
      AND wave.target_kind = kinds.target_kind
)
ORDER BY anchors.anchor_at, kinds.target_kind
LIMIT $3
"#;

impl PostgresAutopilotRepository {
    pub(super) async fn load_outreach_waves_impl(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveSnapshot>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, WaveRow>(WAVE_SNAPSHOT_SQL)
                .bind(workspace_id.into_uuid())
                .bind(MAX_SNAPSHOTS_PER_CONTEXT)
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?;
            let mut snapshots = Vec::with_capacity(rows.len());
            for row in rows {
                snapshots.push(OutreachWaveSnapshot {
                    wave_id: row.id,
                    snapshot: WaveSnapshot {
                        anchor: parse_wave_anchor(&row.anchor_kind, row.anchor_id)?,
                        target_kind: OutreachTargetKind::parse(&row.target_kind)
                            .ok_or(RepositoryError::Unexpected)?,
                        state: WaveState::parse(&row.state).ok_or(RepositoryError::Unexpected)?,
                        opened_at: row.opened_at,
                        anchor_at: row.anchor_at,
                        pitches: u16::try_from(row.pitches).unwrap_or(u16::MAX),
                        eligible_targets: u32::try_from(row.eligible_targets).unwrap_or(u32::MAX),
                        // The capacity frozen at open, not today's budget. An
                        // operator reading a sealed wave should see the ceiling
                        // it was drafted under.
                        third_party_budget_remaining: u32::try_from(row.capacity).unwrap_or(0),
                        anchor_active: row.anchor_active,
                    },
                });
            }
            Ok(snapshots)
        })
        .await
    }

    pub(super) async fn load_outreach_wave_anchors_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveAnchor>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, WaveAnchorRow>(WAVE_ANCHOR_SQL)
                .bind(workspace_id.into_uuid())
                .bind(now)
                .bind(MAX_SNAPSHOTS_PER_CONTEXT)
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?;
            let mut anchors = Vec::with_capacity(rows.len());
            for row in rows {
                anchors.push(OutreachWaveAnchor {
                    anchor: parse_wave_anchor(&row.anchor_kind, row.anchor_id)?,
                    anchor_at: row.anchor_at,
                    target_kind: OutreachTargetKind::parse(&row.target_kind)
                        .ok_or(RepositoryError::Unexpected)?,
                    // Carried rather than filtered in SQL: the refusal to open
                    // is a domain rule somebody can read.
                    active: true,
                    hours_until: row.hours_until,
                    eligible_targets: u32::try_from(row.eligible_targets).unwrap_or(u32::MAX),
                });
            }
            Ok(anchors)
        })
        .await
    }

    pub(super) async fn open_outreach_wave_impl(
        &self,
        workspace_id: WorkspaceId,
        start: &OutreachWaveStart,
    ) -> Result<bool, RepositoryError> {
        self.bounded(async {
            let opened = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO outreach_waves (
                    workspace_id, anchor_kind, anchor_id, anchor_at, target_kind, capacity
                ) VALUES ($1,$2,$3,$4,$5,$6)
                ON CONFLICT (workspace_id, anchor_kind, anchor_id, target_kind) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(start.anchor.as_str())
            .bind(start.anchor.id())
            .bind(start.anchor_at)
            .bind(outreach_target_kind_str(start.target_kind))
            .bind(i32::from(start.capacity))
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
            // Another cycle got there first. Not a failure: the campaign exists
            // exactly once, which is what the constraint is for.
            Ok(opened.is_some())
        })
        .await
    }

    pub(super) async fn transition_outreach_wave_impl(
        &self,
        workspace_id: WorkspaceId,
        wave_id: Uuid,
        transition: OutreachWaveTransition,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            match transition {
                // Guarded on still drafting: a wave a human is already reading
                // must not be re-sealed under them, and one they approved must
                // not be reopened.
                OutreachWaveTransition::Seal => {
                    sqlx::query(
                        "UPDATE outreach_waves \
                         SET state='sealed', sealed_at=$3 \
                         WHERE workspace_id=$1 AND id=$2 AND state='drafting'",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(wave_id)
                    .bind(now)
                    .execute(&self.pool)
                    .await
                    .map_err(map_sqlx)?;
                }
                OutreachWaveTransition::Expire { reason } => {
                    // An expiring wave takes its unapproved pitches with it.
                    // Leaving them queued would send a release-week pitch a
                    // month late, one at a time, with nobody having decided to.
                    let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
                    sqlx::query(
                        "UPDATE autopilot_actions \
                         SET status='cancelled', finished_at=$3 \
                         WHERE workspace_id=$1 AND context='outreach' \
                           AND payload->>'wave_id' = $2::text \
                           AND status = 'awaiting_approval'",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(wave_id)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    sqlx::query(
                        "UPDATE outreach_waves \
                         SET state='expired', settled_at=$3, expiry_reason=$4 \
                         WHERE workspace_id=$1 AND id=$2 AND settled_at IS NULL",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(wave_id)
                    .bind(now)
                    .bind(reason.as_str())
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    transaction.commit().await.map_err(map_sqlx)?;
                }
            }
            Ok(())
        })
        .await
    }

    /// Approves a whole wave in one statement, inside the ledger's
    /// transaction.
    ///
    /// The ledger row and the release commit together — a crash between them
    /// used to leave the ledger insisting "approved" while every pitch stayed
    /// parked, and the replay then swore it happened. Pitch by pitch inside a
    /// loop, an error halfway leaves a half-approved batch — the one state an
    /// operator cannot reason about, because the thing they approved was the
    /// batch.
    pub(super) async fn approve_outreach_wave_impl(
        &self,
        workspace_id: WorkspaceId,
        wave_id: Uuid,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "approve_autopilot_outreach_wave",
                "outreach_wave",
                wave_id,
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"requested_status": "approved"}),
            )
            .await?;
            if let Some(existing) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: wave_id,
                    status: "approved".to_owned(),
                    replayed: true,
                });
            }
            let now = OffsetDateTime::now_utc();
            // Only a sealed wave. A drafting one is still growing, and
            // approving something that grows afterwards is approving something
            // nobody read.
            let sealed = sqlx::query_as::<_, WaveApprovalRow>(
                "UPDATE outreach_waves \
                 SET state='approved', settled_at=$3 \
                 WHERE workspace_id=$1 AND id=$2 AND state='sealed' \
                 RETURNING target_kind",
            )
            .bind(workspace_id.into_uuid())
            .bind(wave_id)
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let Some(sealed) = sealed else {
                return Err(RepositoryError::Conflict);
            };
            let released = sqlx::query(
                "UPDATE autopilot_actions \
                 SET status='queued', approved_at=$3, approved_by='operator:admin_api_key' \
                 WHERE workspace_id=$1 AND context='outreach' \
                   AND payload->>'wave_id' = $2::text \
                   AND status='awaiting_approval' \
                   AND (approval_expires_at IS NULL OR approval_expires_at > $3)",
            )
            .bind(workspace_id.into_uuid())
            .bind(wave_id)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let pitches_sent = u32::try_from(released.rows_affected()).unwrap_or(u32::MAX);
            // Schedule the wave's outcome settlement. The window closes 21 days
            // after approval — a reply that has not arrived by then is not
            // coming. ON CONFLICT DO NOTHING: a re-approval after a crash would
            // otherwise schedule the same wave twice.
            super::play_outcomes::create_wave_outcome(
                &mut transaction,
                workspace_id,
                wave_id,
                &sealed.target_kind,
                pitches_sent,
                now,
            )
            .await?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: wave_id,
                status: format!("approved:{pitches_sent}"),
                replayed: false,
            })
        })
        .await
    }
}

#[derive(sqlx::FromRow)]
struct WaveApprovalRow {
    target_kind: String,
}

fn parse_wave_anchor(kind: &str, anchor_id: Uuid) -> Result<WaveAnchor, RepositoryError> {
    match kind {
        "release" => Ok(WaveAnchor::Release {
            release_id: ReleasePlanId::from_uuid(anchor_id),
        }),
        "event" => Ok(WaveAnchor::Event {
            event_id: EventId::from_uuid(anchor_id),
        }),
        "catalogue" => Ok(WaveAnchor::Catalogue {
            season_id: anchor_id,
        }),
        "threads" => Ok(WaveAnchor::Threads {
            season_id: anchor_id,
        }),
        _ => Err(RepositoryError::Unexpected),
    }
}

const fn outreach_target_kind_str(kind: OutreachTargetKind) -> &'static str {
    match kind {
        OutreachTargetKind::Playlist => "playlist",
        OutreachTargetKind::Radio => "radio",
        OutreachTargetKind::Press => "press",
        OutreachTargetKind::Creator => "creator",
        OutreachTargetKind::SupportSlot => "support_slot",
        OutreachTargetKind::Endorsement => "endorsement",
        OutreachTargetKind::MediaPatronage => "media_patronage",
        OutreachTargetKind::Agent => "agent",
        OutreachTargetKind::Label => "label",
    }
}

/// The first-party numbers this pitch is allowed to claim.
///
/// Read at execution rather than at decision, so what goes out is what was true
/// when the band said it rather than when the agent drafted it. Every field is
/// `None` when the workspace cannot answer it: a zero the agent invented reads
/// exactly like a zero it measured, and the difference is why this exists.
pub(super) async fn evidence_packet(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<EvidencePacket, RepositoryError> {
    let trackers =
        play_outcomes::read_series(transaction, workspace_id, "bandsintown", "trackers", now)
            .await?;
    let paid_tickets_90d = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM ticket_orders AS ticket_order
        WHERE ticket_order.workspace_id = $1
          AND ticket_order.status IN ('paid', 'partially_refunded')
          AND ticket_order.created_at > $2 - INTERVAL '90 days'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let shows_played_12m = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM events AS event
        WHERE event.workspace_id = $1
          AND event.status = 'published'
          AND event.starts_at <= $2
          AND event.starts_at > $2 - INTERVAL '1 year'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let positive_replies_12m = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM outreach_targets AS target
        WHERE target.workspace_id = $1
          AND target.last_reply_disposition = 'positive'
          AND target.last_reply_at > $2 - INTERVAL '1 year'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(EvidencePacket {
        // The series reader already refuses an ambiguous metric, and `None`
        // there means the workspace has no honest number rather than zero.
        trackers: trackers.value,
        trackers_per_day_milli: trackers.milli_per_day,
        // A count over a window the workspace definitely has rows for. Zero
        // here is measured, not invented, so it is reported.
        paid_tickets_90d: Some(paid_tickets_90d),
        shows_played_12m: Some(shows_played_12m),
        positive_replies_12m: Some(positive_replies_12m),
        as_of: Some(now),
    })
}
