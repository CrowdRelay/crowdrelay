//! Durable storage for the agent's only stateful context.
//!
//! Three things are load-bearing here and none of them are visible from the
//! Rust signatures alone.
//!
//! * **A step's committed audience is counted from the action ledger, not from
//!   the delivered-recipient table.** A queued or awaiting-approval send has
//!   already spent the step's budget even though nobody has received anything
//!   yet, and counting only delivered rows would let a slow executor make the
//!   evaluator enqueue the same step's ceiling over and over.
//! * **The same ledger decides who is still eligible.** Without it the play
//!   re-offers the fan whose action is still pending every cycle, makes no
//!   progress, and stalls completely the moment a step needs approval.
//! * **`viryaos_play_step_recipients` still means *reached*.** It is written
//!   when the send is dispatched, so it stays the honest record of who actually
//!   heard from the band — which is what a later measurement has to read.

use super::*;

#[derive(sqlx::FromRow)]
struct PlayAnchorRow {
    anchor_id: Uuid,
    anchor_at: OffsetDateTime,
    active: bool,
    hours_until: i64,
}

#[derive(sqlx::FromRow)]
struct PlayRow {
    id: Uuid,
    play_kind: String,
    anchor_kind: String,
    anchor_id: Uuid,
    anchor_at: OffsetDateTime,
    anchor_active: bool,
}

#[derive(sqlx::FromRow)]
struct PlayStepRow {
    play_id: Uuid,
    step_index: i32,
    step_kind: String,
    action_class: String,
    due_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    settled: bool,
    recipients_emitted: i64,
}

#[derive(sqlx::FromRow)]
struct PlayAudienceRow {
    fan_id: Uuid,
    remaining: i64,
}

/// The slug of the tracked link every follow ask points at.
///
/// The play's whole content is one call to action, and a call to action nobody
/// tracks turns the campaign into an unmeasurable guess. The operator owns the
/// destination — a Bandsintown artist page, or wherever they would rather send
/// people — and the agent refuses to run the ladder until one exists rather
/// than inventing a URL of its own.
pub(super) const FOLLOW_ASK_SMART_LINK_SLUG: &str = "follow";

/// Shows with no play of this kind yet.
const PLAY_EVENT_ANCHORS_SQL: &str = r#"
SELECT
    event.id AS anchor_id,
    event.starts_at AS anchor_at,
    (event.status = 'published') AS active,
    FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $3)) / 3600)::bigint AS hours_until
FROM events AS event
WHERE event.workspace_id = $1
  AND event.status = 'published'
  AND event.starts_at > $3
  AND NOT EXISTS (
      SELECT 1
      FROM viryaos_plays AS play
      WHERE play.workspace_id = event.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'event'
        AND play.anchor_id = event.id
  )
ORDER BY event.starts_at
LIMIT $4
"#;

/// Engaged fans with no play of this kind yet.
///
/// Engaged means they have done something, not that a score said so: a paid
/// ticket or a registered interest inside the last year. The alternative — every
/// consented fan — is a mailing list with an ask attached, which is the thing
/// this play exists instead of.
///
/// The anchor moment is now, because the anchor is the fan qualifying rather
/// than a date in the future, so every rung is scheduled forward from here.
///
/// The ladder is only offered where the operator has already published the
/// tracked link it points at. A follow ask with nowhere to send people is the
/// one message in the system that is worse than silence.
const PLAY_FAN_ANCHORS_SQL: &str = r#"
SELECT
    fan.id AS anchor_id,
    $3::timestamptz AS anchor_at,
    true AS active,
    0::bigint AS hours_until
FROM fans AS fan
JOIN LATERAL (
    SELECT consent.granted
    FROM fan_consents AS consent
    WHERE consent.workspace_id = fan.workspace_id
      AND consent.fan_id = fan.id
      AND consent.purpose = 'marketing'
    ORDER BY consent.recorded_at DESC, consent.id DESC
    LIMIT 1
) AS latest_consent ON latest_consent.granted
WHERE fan.workspace_id = $1
  AND fan.status = 'active'
  AND EXISTS (
      SELECT 1
      FROM smart_links AS link
      WHERE link.workspace_id = fan.workspace_id
        AND link.slug = $5
        AND link.active
  )
  AND (
      EXISTS (
          SELECT 1
          FROM ticket_orders AS ticket_order
          WHERE ticket_order.workspace_id = fan.workspace_id
            AND ticket_order.buyer_email = fan.normalized_email
            AND ticket_order.status IN ('paid', 'partially_refunded')
            AND ticket_order.created_at > $3 - INTERVAL '1 year'
      )
      OR EXISTS (
          SELECT 1
          FROM event_interests AS interest
          WHERE interest.workspace_id = fan.workspace_id
            AND interest.fan_id = fan.id
            AND interest.created_at > $3 - INTERVAL '1 year'
      )
  )
  AND NOT EXISTS (
      SELECT 1
      FROM viryaos_plays AS play
      WHERE play.workspace_id = fan.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'fan'
        AND play.anchor_id = fan.id
  )
ORDER BY fan.id
LIMIT $4
"#;

/// Fans who were here and stopped being here.
///
/// The exact complement of the ladder's audience at the one-year line, so a fan
/// is never both at once: engaged at some point, nothing at all inside the last
/// year. Anybody who never did anything is not dormant, they are a name on a
/// list, and writing to them is the mailing-machine failure this whole context
/// exists to avoid.
///
/// Two further refusals, both of which are the difference between a revival and
/// a pestering:
///
/// * **Nothing to revive them with.** A revival message in a workspace with no
///   upcoming show is "hello, remember us". The gate is a published date, and it
///   is the same shape as the follow-ask ladder's tracked-link gate.
/// * **We already talked at them.** The weekly envelope bounds contact; it does
///   not know that this fan has just had a whole three-rung ladder. Six months
///   without any play step reaching them is what makes this a second chance
///   rather than a continuation.
const PLAY_DORMANT_ANCHORS_SQL: &str = r#"
SELECT
    fan.id AS anchor_id,
    $3::timestamptz AS anchor_at,
    true AS active,
    0::bigint AS hours_until
FROM fans AS fan
JOIN LATERAL (
    SELECT consent.granted
    FROM fan_consents AS consent
    WHERE consent.workspace_id = fan.workspace_id
      AND consent.fan_id = fan.id
      AND consent.purpose = 'marketing'
    ORDER BY consent.recorded_at DESC, consent.id DESC
    LIMIT 1
) AS latest_consent ON latest_consent.granted
WHERE fan.workspace_id = $1
  AND fan.status = 'active'
  AND EXISTS (
      SELECT 1
      FROM events AS event
      WHERE event.workspace_id = fan.workspace_id
        AND event.status = 'published'
        AND event.starts_at > $3
  )
  AND (
      EXISTS (
          SELECT 1
          FROM ticket_orders AS ticket_order
          WHERE ticket_order.workspace_id = fan.workspace_id
            AND ticket_order.buyer_email = fan.normalized_email
            AND ticket_order.status IN ('paid', 'partially_refunded')
      )
      OR EXISTS (
          SELECT 1
          FROM event_interests AS interest
          WHERE interest.workspace_id = fan.workspace_id
            AND interest.fan_id = fan.id
      )
  )
  AND NOT EXISTS (
      SELECT 1
      FROM ticket_orders AS ticket_order
      WHERE ticket_order.workspace_id = fan.workspace_id
        AND ticket_order.buyer_email = fan.normalized_email
        AND ticket_order.status IN ('paid', 'partially_refunded')
        AND ticket_order.created_at > $3 - INTERVAL '1 year'
  )
  AND NOT EXISTS (
      SELECT 1
      FROM event_interests AS interest
      WHERE interest.workspace_id = fan.workspace_id
        AND interest.fan_id = fan.id
        AND interest.created_at > $3 - INTERVAL '1 year'
  )
  AND NOT EXISTS (
      SELECT 1
      FROM viryaos_play_step_recipients AS reached
      WHERE reached.workspace_id = fan.workspace_id
        AND reached.fan_id = fan.id
        AND reached.created_at > $3 - INTERVAL '6 months'
  )
  AND NOT EXISTS (
      SELECT 1
      FROM viryaos_plays AS play
      WHERE play.workspace_id = fan.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'fan'
        AND play.anchor_id = fan.id
  )
ORDER BY fan.id
LIMIT $4
"#;

/// The statement that finds anchors for one play, and whether it reads the
/// follow-ask link slug.
///
/// Keyed on the play kind rather than the anchor kind: two plays now share the
/// fan anchor and ask completely different questions of it. A parameter the
/// statement does not mention is a protocol error, so the flag travels with the
/// statement rather than being inferred at the call site.
const fn anchor_statement(kind: PlayKind) -> (&'static str, bool) {
    match kind {
        PlayKind::TrackUsAsk | PlayKind::ListingCompletenessSweep => {
            (PLAY_EVENT_ANCHORS_SQL, false)
        }
        PlayKind::FollowAskLadder => (PLAY_FAN_ANCHORS_SQL, true),
        PlayKind::DormantRevival => (PLAY_DORMANT_ANCHORS_SQL, false),
    }
}

/// The eligible audience for the earliest unsettled step of one play.
///
/// Written as one statement so the recipient and the count can never come from
/// two different reads: a count taken separately would be a different moment's
/// answer, and the play would claim work it no longer had.
const PLAY_AUDIENCE_SQL: &str = r#"
WITH open_step AS (
    SELECT step.id, step.step_index, step.step_kind
    FROM viryaos_play_steps AS step
    WHERE step.workspace_id = $1
      AND step.play_id = $2
      AND step.settled_at IS NULL
    ORDER BY step.step_index
    LIMIT 1
),
eligible AS (
    SELECT fan.id AS fan_id
    FROM open_step
    CROSS JOIN fans AS fan
    JOIN LATERAL (
        SELECT consent.granted
        FROM fan_consents AS consent
        WHERE consent.workspace_id = fan.workspace_id
          AND consent.fan_id = fan.id
          AND consent.purpose = 'marketing'
        ORDER BY consent.recorded_at DESC, consent.id DESC
        LIMIT 1
    ) AS latest_consent ON latest_consent.granted
    WHERE fan.workspace_id = $1
      AND fan.status = 'active'
      AND (
          -- A paid ticket is the closest thing the database holds to
          -- attendance, and it is the only qualification the post-show ask
          -- accepts: thanking somebody for coming who did not come is a worse
          -- message than sending nothing.
          EXISTS (
              SELECT 1
              FROM ticket_orders AS ticket_order
              JOIN ticket_sales AS sale
                ON sale.workspace_id = ticket_order.workspace_id
               AND sale.id = ticket_order.ticket_sale_id
              WHERE ticket_order.workspace_id = fan.workspace_id
                AND ticket_order.buyer_email = fan.normalized_email
                AND ticket_order.status IN ('paid', 'partially_refunded')
                AND sale.event_id = $3
          )
          OR (
              open_step.step_kind = 'announce_ask'
              AND EXISTS (
                  SELECT 1
                  FROM event_interests AS interest
                  WHERE interest.workspace_id = fan.workspace_id
                    AND interest.fan_id = fan.id
                    AND interest.event_id = $3
              )
          )
      )
      -- Already committed to, whether or not it has been delivered. Reading
      -- only the delivered table here is what makes a play re-offer the same
      -- fan every cycle and never finish.
      AND NOT EXISTS (
          SELECT 1
          FROM viryaos_autopilot_actions AS action
          WHERE action.workspace_id = fan.workspace_id
            AND action.context = 'plays'
            AND action.action_kind = 'play.step.run'
            AND action.status <> 'cancelled'
            AND action.subject_id = fan.id
            AND action.payload->>'play_id' = $2::text
            AND (action.payload->>'step_index')::integer = open_step.step_index
      )
)
SELECT fan_id, count(*) OVER ()::bigint AS remaining
FROM eligible
ORDER BY fan_id
LIMIT 1
"#;

/// The audience of a fan-anchored play: the anchor, and nobody else.
///
/// Separate from the show query rather than a branch inside it. The show
/// version answers "which of our fans has a reason to hear about this date";
/// this one answers "is the person this campaign is about still someone we may
/// write to", and merging them would make one statement that means neither.
const PLAY_FAN_AUDIENCE_SQL: &str = r#"
WITH open_step AS (
    SELECT step.id, step.step_index
    FROM viryaos_play_steps AS step
    WHERE step.workspace_id = $1
      AND step.play_id = $2
      AND step.settled_at IS NULL
    ORDER BY step.step_index
    LIMIT 1
),
eligible AS (
    SELECT fan.id AS fan_id
    FROM open_step
    CROSS JOIN fans AS fan
    JOIN LATERAL (
        SELECT consent.granted
        FROM fan_consents AS consent
        WHERE consent.workspace_id = fan.workspace_id
          AND consent.fan_id = fan.id
          AND consent.purpose = 'marketing'
        ORDER BY consent.recorded_at DESC, consent.id DESC
        LIMIT 1
    ) AS latest_consent ON latest_consent.granted
    WHERE fan.workspace_id = $1
      AND fan.id = $3
      AND fan.status = 'active'
      -- Committed to, whether or not it has been delivered. Without this the
      -- rung re-offers the same fan every cycle and the ladder never climbs.
      AND NOT EXISTS (
          SELECT 1
          FROM viryaos_autopilot_actions AS action
          WHERE action.workspace_id = fan.workspace_id
            AND action.context = 'plays'
            AND action.action_kind = 'play.step.run'
            AND action.status <> 'cancelled'
            AND action.subject_id = fan.id
            AND action.payload->>'play_id' = $2::text
            AND (action.payload->>'step_index')::integer = open_step.step_index
      )
)
SELECT fan_id, count(*) OVER ()::bigint AS remaining
FROM eligible
LIMIT 1
"#;

impl PostgresAutopilotRepository {
    pub(super) async fn load_play_anchors_impl(
        &self,
        workspace_id: WorkspaceId,
        kind: PlayKind,
        now: OffsetDateTime,
    ) -> Result<Vec<PlayAnchor>, RepositoryError> {
        self.bounded(async {
            // The play kind decides which table is even looked at. Reading
            // shows for a fan-anchored play would start a campaign against an
            // anchor its own audience query cannot find.
            let anchor_kind = kind.anchor_kind();
            let (statement, reads_link_slug) = anchor_statement(kind);
            let mut query = sqlx::query_as::<_, PlayAnchorRow>(statement)
                .bind(workspace_id.into_uuid())
                .bind(kind.as_str())
                .bind(now)
                .bind(MAX_SNAPSHOTS_PER_CONTEXT);
            // Bound only where it is read. A parameter the statement does not
            // mention is a protocol error, not a spare argument.
            if reads_link_slug {
                query = query.bind(FOLLOW_ASK_SMART_LINK_SLUG);
            }
            let rows = query.fetch_all(&self.pool).await.map_err(map_sqlx)?;
            Ok(rows
                .into_iter()
                .map(|row| PlayAnchor {
                    anchor: anchor_ref(anchor_kind, row.anchor_id),
                    anchor_at: row.anchor_at,
                    active: row.active,
                    hours_until: row.hours_until,
                })
                .collect())
        })
        .await
    }

    pub(super) async fn start_play_impl(
        &self,
        workspace_id: WorkspaceId,
        start: &PlayStart,
    ) -> Result<bool, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let play_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_plays (
                    id, workspace_id, play_kind, anchor_kind, anchor_id, anchor_at,
                    hypothesis, success_metric_platform, success_metric_key
                )
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                ON CONFLICT (workspace_id, play_kind, anchor_kind, anchor_id) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id.into_uuid())
            .bind(start.kind.as_str())
            .bind(start.anchor.kind().as_str())
            .bind(start.anchor.id())
            .bind(start.anchor_at)
            .bind(start.hypothesis)
            .bind(start.success_metric_platform)
            .bind(start.success_metric_key)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // Another cycle got there first. Not a failure: the campaign exists
            // exactly once, which is the whole point of the unique constraint.
            let Some(play_id) = play_id else {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(false);
            };

            let mut indexes = Vec::with_capacity(start.steps.len());
            let mut kinds = Vec::with_capacity(start.steps.len());
            let mut classes = Vec::with_capacity(start.steps.len());
            let mut due = Vec::with_capacity(start.steps.len());
            let mut expiry = Vec::with_capacity(start.steps.len());
            for step in &start.steps {
                indexes.push(i32::from(step.index));
                kinds.push(step.kind.as_str());
                classes.push(step.class.as_str());
                due.push(step.due_at);
                expiry.push(step.expires_at);
            }
            // The whole schedule in one statement, in the same transaction as
            // the play. A play with a missing step would be a campaign that
            // silently skips a moment nobody could see it was meant to have.
            sqlx::query(
                r#"
                INSERT INTO viryaos_play_steps (
                    workspace_id, play_id, step_index, step_kind, action_class, due_at, expires_at
                )
                SELECT $1, $2, step_index, step_kind, action_class, due_at, expires_at
                FROM UNNEST(
                    $3::integer[], $4::text[], $5::text[],
                    $6::timestamptz[], $7::timestamptz[]
                ) AS step(step_index, step_kind, action_class, due_at, expires_at)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(play_id)
            .bind(&indexes)
            .bind(&kinds)
            .bind(&classes)
            .bind(&due)
            .bind(&expiry)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // Same transaction as the play itself. A campaign that existed
            // without a frozen baseline could never be measured honestly: the
            // window to capture one closes the moment its first step runs.
            play_outcomes::open_play_outcomes(
                &mut transaction,
                workspace_id,
                play_id,
                start,
                OffsetDateTime::now_utc(),
            )
            .await?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(true)
        })
        .await
    }

    pub(super) async fn load_play_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<Vec<PlayRunSnapshot>, RepositoryError> {
        self.bounded(async {
            let plays = sqlx::query_as::<_, PlayRow>(
                r#"
                SELECT
                    play.id,
                    play.play_kind,
                    play.anchor_kind,
                    play.anchor_id,
                    play.anchor_at,
                    -- A deleted or unpublished show is a withdrawn anchor, and
                    -- the coalesce is what makes the missing row say so instead
                    -- of dropping the play out of the read entirely.
                    --
                    -- A fan who unsubscribed or withdrew consent is the same
                    -- fact about a different anchor: the reason to act has
                    -- gone, so the remaining rungs are skipped rather than
                    -- sent. Consent is re-checked at dispatch too; this is what
                    -- stops the play spending cycles first.
                    CASE play.anchor_kind
                        WHEN 'event' THEN COALESCE(event.status = 'published', false)
                        ELSE COALESCE(fan.status = 'active', false)
                             -- The *latest* consent decision, not any consent
                             -- row: a fan who granted and then withdrew has
                             -- both, and only the second one is an answer.
                             AND COALESCE((
                                 SELECT consent.granted
                                 FROM fan_consents AS consent
                                 WHERE consent.workspace_id = play.workspace_id
                                   AND consent.fan_id = play.anchor_id
                                   AND consent.purpose = 'marketing'
                                 ORDER BY consent.recorded_at DESC, consent.id DESC
                                 LIMIT 1
                             ), false)
                    END AS anchor_active
                FROM viryaos_plays AS play
                LEFT JOIN events AS event
                  ON play.anchor_kind = 'event'
                 AND event.workspace_id = play.workspace_id
                 AND event.id = play.anchor_id
                LEFT JOIN fans AS fan
                  ON play.anchor_kind = 'fan'
                 AND fan.workspace_id = play.workspace_id
                 AND fan.id = play.anchor_id
                WHERE play.workspace_id = $1
                  AND play.state = 'running'
                ORDER BY play.anchor_at
                LIMIT $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            if plays.is_empty() {
                return Ok(Vec::new());
            }
            let play_ids: Vec<Uuid> = plays.iter().map(|play| play.id).collect();
            let steps = sqlx::query_as::<_, PlayStepRow>(
                r#"
                SELECT
                    step.play_id,
                    step.step_index,
                    step.step_kind,
                    step.action_class,
                    step.due_at,
                    step.expires_at,
                    (step.settled_at IS NOT NULL) AS settled,
                    (
                        SELECT count(*)::bigint
                        FROM viryaos_autopilot_actions AS action
                        WHERE action.workspace_id = step.workspace_id
                          AND action.context = 'plays'
                          AND action.action_kind = 'play.step.run'
                          AND action.status <> 'cancelled'
                          AND action.payload->>'play_id' = step.play_id::text
                          AND (action.payload->>'step_index')::integer = step.step_index
                    ) AS recipients_emitted
                FROM viryaos_play_steps AS step
                WHERE step.workspace_id = $1
                  AND step.play_id = ANY($2)
                ORDER BY step.play_id, step.step_index
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&play_ids)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let mut snapshots = Vec::with_capacity(plays.len());
            for play in plays {
                let kind = PlayKind::parse(&play.play_kind).ok_or(RepositoryError::Unexpected)?;
                let anchor_kind =
                    PlayAnchorKind::parse(&play.anchor_kind).ok_or(RepositoryError::Unexpected)?;
                // A stored anchor kind that disagrees with the play's own is a
                // row no code path can write, and running the wrong audience
                // query against it would contact the wrong people. Refusing the
                // whole read is the only safe answer.
                if anchor_kind != kind.anchor_kind() {
                    return Err(RepositoryError::Unexpected);
                }
                let mut play_steps = Vec::new();
                for row in steps.iter().filter(|step| step.play_id == play.id) {
                    play_steps.push(PlayStepState {
                        index: u16::try_from(row.step_index)
                            .map_err(|_| RepositoryError::Unexpected)?,
                        kind: PlayStepKind::parse(&row.step_kind)
                            .ok_or(RepositoryError::Unexpected)?,
                        class: ActionClass::parse(&row.action_class)
                            .ok_or(RepositoryError::Unexpected)?,
                        due_at: row.due_at,
                        expires_at: row.expires_at,
                        settled: row.settled,
                        recipients_emitted: u32::try_from(row.recipients_emitted)
                            .map_err(|_| RepositoryError::Unexpected)?,
                    });
                }
                // Only a play with an open step has an audience to read, and
                // asking for one otherwise is a query per finished play per
                // cycle for an answer the state machine will not look at.
                let open = play_steps.iter().find(|step| !step.settled);
                let audience = match open.map(|step| step.kind.audience()) {
                    // A step that needs nobody must not be measured against an
                    // audience: reading one would settle the only work in the
                    // system that requires no consent as having no recipients.
                    Some(StepAudience::None) => PlayAudience::NotRequired,
                    Some(StepAudience::Fans) => {
                        self.play_audience(workspace_id, play.id, anchor_kind, play.anchor_id)
                            .await?
                    }
                    None => PlayAudience::Exhausted,
                };
                snapshots.push(PlayRunSnapshot {
                    play_id: PlayId::from_uuid(play.id),
                    kind,
                    anchor: anchor_ref(anchor_kind, play.anchor_id),
                    anchor_at: play.anchor_at,
                    anchor_active: play.anchor_active,
                    steps: play_steps,
                    audience,
                });
            }
            Ok(snapshots)
        })
        .await
    }

    async fn play_audience(
        &self,
        workspace_id: WorkspaceId,
        play_id: Uuid,
        anchor_kind: PlayAnchorKind,
        anchor_id: Uuid,
    ) -> Result<PlayAudience, RepositoryError> {
        let row = sqlx::query_as::<_, PlayAudienceRow>(match anchor_kind {
            PlayAnchorKind::Event => PLAY_AUDIENCE_SQL,
            PlayAnchorKind::Fan => PLAY_FAN_AUDIENCE_SQL,
        })
        .bind(workspace_id.into_uuid())
        .bind(play_id)
        .bind(anchor_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        row.map_or(Ok(PlayAudience::Exhausted), |row| {
            Ok(PlayAudience::Next {
                fan_id: FanId::from_uuid(row.fan_id),
                remaining: u32::try_from(row.remaining).map_err(|_| RepositoryError::Unexpected)?,
            })
        })
    }

    pub(super) async fn settle_play_step_impl(
        &self,
        workspace_id: WorkspaceId,
        settlement: &PlayStepSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            // Guarded on `settled_at IS NULL` rather than on a row count: two
            // cycles racing on the same expired step must leave one recorded
            // reason, and the first one written is the true one.
            sqlx::query(
                r#"
                UPDATE viryaos_play_steps
                SET settled_at = $4, skip_reason = $5
                WHERE workspace_id = $1
                  AND play_id = $2
                  AND step_index = $3
                  AND settled_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(settlement.play_id.into_uuid())
            .bind(i32::from(settlement.step_index))
            .bind(now)
            .bind(settlement.reason.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }

    pub(super) async fn complete_play_impl(
        &self,
        workspace_id: WorkspaceId,
        play_id: PlayId,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            // The `NOT EXISTS` is the real guard. A play completed while a step
            // is still open would strand that step for ever, and the evaluator
            // is not the only thing that can settle one.
            sqlx::query(
                r#"
                UPDATE viryaos_plays AS play
                SET state = 'completed', completed_at = $3
                WHERE play.workspace_id = $1
                  AND play.id = $2
                  AND play.state = 'running'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM viryaos_play_steps AS step
                      WHERE step.workspace_id = play.workspace_id
                        AND step.play_id = play.id
                        AND step.settled_at IS NULL
                  )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(play_id.into_uuid())
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}

/// Rebuilds the typed anchor from the two columns that hold it.
pub(super) fn anchor_ref(kind: PlayAnchorKind, anchor_id: Uuid) -> PlayAnchorRef {
    match kind {
        PlayAnchorKind::Event => PlayAnchorRef::Event {
            event_id: EventId::from_uuid(anchor_id),
        },
        PlayAnchorKind::Fan => PlayAnchorRef::Fan {
            fan_id: FanId::from_uuid(anchor_id),
        },
    }
}

/// One step of one play, as the executing side reads it.
///
/// `fan_id` is absent for a step that reaches nobody. A listing sweep is work
/// on the band's own surfaces, and carrying a fan there would record a contact
/// that never happened. `event_id` is absent for a play with no show in it.
#[derive(Clone, Copy)]
pub(super) struct PlayStepDispatch<'a> {
    pub play_id: PlayId,
    pub play_kind: PlayKind,
    pub step_index: u16,
    pub step_kind: PlayStepKind,
    pub event_id: Option<EventId>,
    pub fan_id: Option<FanId>,
    pub template_key: &'a str,
}

/// Dispatches one step of a play to one fan.
///
/// Consent is re-checked here and not taken from the decision. Time passes
/// between a decision and its execution, and the one thing that must never
/// survive that gap is a message to somebody who withdrew consent in it.
pub(super) async fn execute_play_step(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    dispatch: &PlayStepDispatch<'_>,
) -> Result<(), RepositoryError> {
    let PlayStepDispatch {
        play_id,
        play_kind,
        step_index,
        step_kind,
        event_id,
        fan_id,
        template_key,
    } = *dispatch;
    // Consent is only a question when somebody is being contacted. Checking it
    // for a listing sweep would refuse the one kind of work nobody has to
    // agree to.
    if let Some(fan_id) = fan_id {
        ensure_marketing_eligible(transaction, workspace_id, fan_id).await?;
    }
    // The step must still be open, and where the play has a show that show must
    // still be on. Both can have changed since the decision, and a cancelled
    // show promoted by an action queued before the cancellation is exactly the
    // failure the anchor check exists to prevent.
    //
    // Two statements rather than an outer join, because Postgres will not take
    // `FOR SHARE` on the nullable side of one: merging them would quietly drop
    // the lock that keeps a show from being unpublished mid-dispatch.
    let (step_id, event_facts) = match event_id {
        Some(event_id) => {
            let row = sqlx::query_as::<_, (Uuid, String, String, OffsetDateTime, Option<String>)>(
                r#"
                    SELECT step.id, event.title, event.slug, event.starts_at, event.venue
                    FROM viryaos_play_steps AS step
                    JOIN viryaos_plays AS play
                      ON play.workspace_id = step.workspace_id
                     AND play.id = step.play_id
                    JOIN events AS event
                      ON event.workspace_id = play.workspace_id
                     AND event.id = play.anchor_id
                    WHERE step.workspace_id = $1
                      AND step.play_id = $2
                      AND step.step_index = $3
                      AND step.settled_at IS NULL
                      AND play.state = 'running'
                      AND event.status = 'published'
                    FOR SHARE OF step, play, event
                    "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(play_id.into_uuid())
            .bind(i32::from(step_index))
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;
            (
                row.0,
                Some(json!({
                    "id": event_id,
                    "title": row.1,
                    "slug": row.2,
                    "starts_at": row.3,
                    "venue": row.4,
                })),
            )
        }
        None => {
            let row = sqlx::query_as::<_, (Uuid,)>(
                r#"
                SELECT step.id
                FROM viryaos_play_steps AS step
                JOIN viryaos_plays AS play
                  ON play.workspace_id = step.workspace_id
                 AND play.id = step.play_id
                WHERE step.workspace_id = $1
                  AND step.play_id = $2
                  AND step.step_index = $3
                  AND step.settled_at IS NULL
                  AND play.state = 'running'
                FOR SHARE OF step, play
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(play_id.into_uuid())
            .bind(i32::from(step_index))
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;
            (row.0, None)
        }
    };

    let fan = match fan_id {
        Some(fan_id) => {
            let fan = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                "SELECT normalized_email, display_name, locale FROM fans WHERE workspace_id=$1 AND id=$2 AND status='active' FOR SHARE",
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::Conflict)?;
            // Written before the intent is emitted and in the same transaction,
            // so a dispatched send is never missing from the record of who was
            // reached.
            sqlx::query(
                r#"
                INSERT INTO viryaos_play_step_recipients (workspace_id, step_id, fan_id, action_id)
                VALUES ($1,$2,$3,$4)
                ON CONFLICT (workspace_id, step_id, fan_id) DO NOTHING
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(step_id)
            .bind(fan_id.into_uuid())
            .bind(action_id.into_uuid())
            .execute(&mut **transaction)
            .await
            .map_err(map_sqlx)?;
            Some(fan)
        }
        None => None,
    };

    // A follow ask is one call to action, and a call to action nobody tracks
    // makes the whole campaign unmeasurable. The link is the operator's, read
    // rather than invented, and its absence voids this send the same way a
    // withdrawn consent does: the world changed after the decision.
    let follow_link = match play_kind {
        PlayKind::FollowAskLadder => Some(follow_ask_link(transaction, workspace_id).await?),
        PlayKind::TrackUsAsk | PlayKind::ListingCompletenessSweep | PlayKind::DormantRevival => {
            None
        }
    };

    emit_external_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.play.step_requested",
        json!({
            "action_id": action_id,
            "play_id": play_id,
            "play_kind": play_kind.as_str(),
            "step_index": step_index,
            "step_kind": step_kind.as_str(),
            "template_key": template_key,
            "fan_id": fan_id,
            "fan": fan.map(|fan| {
                json!({
                    "email": fan.0,
                    "display_name": fan.1,
                    "locale": fan.2,
                })
            }),
            "event": event_facts,
            "call_to_action_url": follow_link,
        }),
    )
    .await
}

/// The tracked link a follow ask points at.
///
/// `Conflict` when there is none. The anchor loader will not start a ladder
/// without it, so reaching here means an operator deactivated the link while a
/// campaign was running — and a follow ask with nowhere to send people is the
/// one message in the system that is worse than silence.
async fn follow_ask_link(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<String, RepositoryError> {
    sqlx::query_scalar::<_, String>(
        "SELECT slug FROM smart_links WHERE workspace_id=$1 AND slug=$2 AND active",
    )
    .bind(workspace_id.into_uuid())
    .bind(FOLLOW_ASK_SMART_LINK_SLUG)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .map(|slug| format!("/l/{slug}"))
    .ok_or(RepositoryError::Conflict)
}
