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
//! * **`play_step_recipients` still means *reached*.** It is written
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
      FROM plays AS play
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
      FROM plays AS play
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
      FROM play_step_recipients AS reached
      WHERE reached.workspace_id = fan.workspace_id
        AND reached.fan_id = fan.id
        AND reached.created_at > $3 - INTERVAL '6 months'
  )
  AND NOT EXISTS (
      SELECT 1
      FROM plays AS play
      WHERE play.workspace_id = fan.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'fan'
        AND play.anchor_id = fan.id
  )
ORDER BY fan.id
LIMIT $4
"#;

/// Active release plans with no release runway play yet.
///
/// A release is the anchor when it is ahead and active. The anchor moment is
/// `release_at`, so steps schedule relative to the release date: pre-save
/// four weeks before, announce two weeks before, curator wave one week
/// before, release-day push at zero, sustain ask two weeks after.
const PLAY_RELEASE_ANCHORS_SQL: &str = r#"
SELECT
    plan.id AS anchor_id,
    plan.release_at AS anchor_at,
    plan.active AS active,
    FLOOR(EXTRACT(EPOCH FROM (plan.release_at - $3)) / 3600)::bigint AS hours_until
FROM release_plans AS plan
WHERE plan.workspace_id = $1
  AND plan.active
  -- A filler release is posted into a quiet week, not run through the
  -- runway — the tier is the band's call, not a play that failed to open.
  AND plan.tier <> 'filler'
  AND plan.release_at > $3
  AND NOT EXISTS (
      SELECT 1
      FROM plays AS play
      WHERE play.workspace_id = plan.workspace_id
        AND play.play_kind = $2
        AND play.anchor_kind = 'release'
        AND play.anchor_id = plan.id
  )
ORDER BY plan.release_at
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
        PlayKind::ReleaseRunway => (PLAY_RELEASE_ANCHORS_SQL, false),
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
    FROM play_steps AS step
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
          (
              open_step.step_kind = 'announce_ask'
              AND (
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
                  OR EXISTS (
                      SELECT 1
                      FROM event_interests AS interest
                      WHERE interest.workspace_id = fan.workspace_id
                        AND interest.fan_id = fan.id
                        AND interest.event_id = $3
                  )
              )
          )
          OR (
              -- Compatibility only: new TrackUsAsk plays no longer create this
              -- rung. If an old persisted play reaches it, attendance must be
              -- observed rather than inferred from a purchase.
              open_step.step_kind = 'post_show_ask'
              AND (
                  EXISTS (
                      SELECT 1
                      FROM concert_checkins AS checkin
                      WHERE checkin.workspace_id = fan.workspace_id
                        AND checkin.fan_id = fan.id
                        AND checkin.event_id = $3
                  )
                  OR EXISTS (
                      SELECT 1
                      FROM admission_passes AS pass
                      WHERE pass.workspace_id = fan.workspace_id
                        AND pass.fan_id = fan.id
                        AND pass.event_id = $3
                        AND pass.status = 'redeemed'
                  )
              )
              -- Show Growth owns the observed room whenever it is enabled:
              -- recap, merch and follow ask share one cadence there. Leaving
              -- this legacy rung live in parallel would create a second owner
              -- of the same T+ moment.
              AND NOT EXISTS (
                  SELECT 1
                  FROM autopilot_policies AS policy
                  WHERE policy.workspace_id = fan.workspace_id
                    AND policy.context = 'show_growth'
                    AND policy.enabled
              )
          )
      )
      -- Already committed to, whether or not it has been delivered. Reading
      -- only the delivered table here is what makes a play re-offer the same
      -- fan every cycle and never finish.
      AND NOT EXISTS (
          SELECT 1
          FROM autopilot_actions AS action
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
    FROM play_steps AS step
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
          FROM autopilot_actions AS action
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

/// The audience of a release-anchored play: every consented fan, because a
/// release is the one thing the whole list should hear about.
///
/// Unlike a fan-anchored play (one person) or an event-anchored play (fans
/// near one show), a release has no geographic or per-fan filter. The only
/// gate is consent and the step's own eligibility — the same action-ledger
/// check that stops any other play re-offering the same fan.
const PLAY_RELEASE_AUDIENCE_SQL: &str = r#"
WITH open_step AS (
    SELECT step.id, step.step_index
    FROM play_steps AS step
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
      AND NOT EXISTS (
          SELECT 1
          FROM autopilot_actions AS action
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
                INSERT INTO plays (
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
                INSERT INTO play_steps (
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
                FROM plays AS play
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
                        FROM autopilot_actions AS action
                        WHERE action.workspace_id = step.workspace_id
                          AND action.context = 'plays'
                          AND action.action_kind = 'play.step.run'
                          AND action.status <> 'cancelled'
                          AND action.payload->>'play_id' = step.play_id::text
                          AND (action.payload->>'step_index')::integer = step.step_index
                    ) AS recipients_emitted
                FROM play_steps AS step
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
            PlayAnchorKind::Release => PLAY_RELEASE_AUDIENCE_SQL,
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
                UPDATE play_steps
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
            .bind(settlement.reason.map(StepSkipReason::as_str))
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
                UPDATE plays AS play
                SET state = 'completed', completed_at = $3
                WHERE play.workspace_id = $1
                  AND play.id = $2
                  AND play.state = 'running'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM play_steps AS step
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
        PlayAnchorKind::Release => PlayAnchorRef::Release {
            release_plan_id: ReleasePlanId::from_uuid(anchor_id),
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

/// The show an event-anchored step locks onto, with the listing fields a
/// `listing_sweep` step is there to check.
#[derive(sqlx::FromRow)]
struct PlayStepEventRow {
    step_id: Uuid,
    title: String,
    slug: String,
    starts_at: OffsetDateTime,
    timezone: String,
    venue: Option<String>,
    ticket_url: Option<String>,
    external_event_url: Option<String>,
    source_provider: Option<String>,
    city_id: Option<Uuid>,
}

/// The release a release-anchored step locks onto, with the tracked link its
/// fan-facing steps point at.
#[derive(sqlx::FromRow)]
struct PlayStepReleaseRow {
    title: String,
    release_at: OffsetDateTime,
    listen_url: Option<String>,
    link_slug: Option<String>,
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
    let (step_id, anchor_kind, anchor_id, event_row) = match event_id {
        Some(event_id) => {
            let row = sqlx::query_as::<_, PlayStepEventRow>(
                r#"
                    SELECT step.id AS step_id, event.title, event.slug, event.starts_at,
                           event.timezone, event.venue, event.ticket_url, event.external_event_url,
                           event.source_provider, event.city_id
                    FROM play_steps AS step
                    JOIN plays AS play
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
                row.step_id,
                "event".to_owned(),
                event_id.into_uuid(),
                Some(row),
            )
        }
        None => {
            let row = sqlx::query_as::<_, (Uuid, String, Uuid)>(
                r#"
                SELECT step.id, play.anchor_kind, play.anchor_id
                FROM play_steps AS step
                JOIN plays AS play
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
            (row.0, row.1, row.2, None)
        }
    };

    // A release-anchored play's anchor is the plan, and it must still be live
    // the way a show must still be on: a plan deactivated while its sends sat
    // in the queue is the same promotion-after-cancellation failure with a
    // different anchor type.
    let release_row = if anchor_kind == "release" {
        let row = sqlx::query_as::<_, PlayStepReleaseRow>(
            r#"
            SELECT plan.title, plan.release_at, plan.listen_url,
                   link.slug AS link_slug
            FROM release_plans AS plan
            LEFT JOIN campaigns AS campaign
              ON campaign.workspace_id = plan.workspace_id
             AND campaign.release_plan_id = plan.id
            LEFT JOIN smart_links AS link
              ON link.workspace_id = campaign.workspace_id
             AND link.campaign_id = campaign.id
             AND link.active
            WHERE plan.workspace_id = $1
              AND plan.id = $2
              AND plan.active
            ORDER BY link.slug
            LIMIT 1
            FOR SHARE OF plan
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(anchor_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::Conflict)?;
        Some(row)
    } else {
        None
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
            Some(fan)
        }
        None => None,
    };

    // A follow ask is one call to action, and a call to action nobody tracks
    // makes the whole campaign unmeasurable. The link is the operator's, read
    // rather than invented, and its absence voids this send the same way a
    // withdrawn consent does: the world changed after the decision.
    let follow_link = match play_kind {
        PlayKind::FollowAskLadder => Some(
            follow_ask_link(transaction, workspace_id, action_id).await?
        ),
        PlayKind::TrackUsAsk
        | PlayKind::ListingCompletenessSweep
        | PlayKind::DormantRevival
        | PlayKind::ReleaseRunway => None,
    };

    // The in-process delivery leg. A fan-facing step writes the push delivery
    // itself, in the same transaction as the recipient ledger row, so the
    // record of who was reached and the queue that reaches them cannot
    // disagree. A step with no audience records what it checked on the step
    // row instead — the listing sweep's findings are the work, not a side
    // effect of it.
    let polish = fan
        .as_ref()
        .and_then(|fan| fan.2.as_deref())
        .is_none_or(|locale| locale.to_lowercase().starts_with("pl"));
    let event_date = event_row.as_ref().map(|event| {
        crate::regional::at_event_timezone(event.starts_at, &event.timezone)
            .date()
            .to_string()
    });
    let event_path = event_row.as_ref().map(|event| {
        if polish {
            format!("/pl/my-signal/?event={}", event.slug)
        } else {
            format!("/my-signal/?event={}", event.slug)
        }
    });
    let release_date = release_row
        .as_ref()
        .map(|release| release.release_at.date().to_string());
    let release_link = release_row.as_ref().and_then(|release| {
        release
            .link_slug
            .as_deref()
            .map(|slug| format!("/l/{slug}"))
    });
    let mut push_deliveries = 0_i64;
    let mut step_result = match step_kind {
        PlayStepKind::ListingSweep => {
            match event_row.as_ref() {
                Some(event) => {
                    let mut report = listing_report(event);
                    // The one gap a sweep can close on its own: the event's
                    // own sale is live and the listing carries no ticket link,
                    // so the link is a fact we already hold, not an invention.
                    // What the workspace's posture decides is whether it is
                    // applied or proposed — the fix is its own action either
                    // way, so the ledger records who let it through.
                    if event.ticket_url.is_none()
                        && let Some(ticket_url) =
                            live_sale_url(transaction, workspace_id, anchor_id, &event.slug).await?
                    {
                        let status = propose_ticket_url_fix(
                            transaction,
                            workspace_id,
                            action_id,
                            play_id,
                            step_index,
                            anchor_id,
                            &ticket_url,
                        )
                        .await?;
                        if let Some(object) = report.as_object_mut() {
                            object.insert(
                                "proposed_fix".to_owned(),
                                json!({
                                    "action_kind": "event.ticket_url.set",
                                    "ticket_url": ticket_url,
                                    "status": status,
                                }),
                            );
                        }
                    }
                    Some(report)
                }
                None => None,
            }
        }
        PlayStepKind::ReleasePresaveLive => release_row.as_ref().map(presave_report),
        _ => None,
    };
    // The act's own name for the copy, read in the transaction that writes
    // the delivery — `crowdrelay_workspace_wordmark` is the one resolver the
    // SQL-built campaign pushes use too.
    let wordmark: String = sqlx::query_scalar("SELECT crowdrelay_workspace_wordmark($1)")
        .bind(workspace_id.into_uuid())
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    if let Some(fan_id) = fan_id
        && let Some(copy) = step_kind.push_copy(&PlayStepPushFacts {
            wordmark: &wordmark,
            polish,
            event_title: event_row.as_ref().map(|event| event.title.as_str()),
            event_date: event_date.as_deref(),
            event_venue: event_row.as_ref().and_then(|event| event.venue.as_deref()),
            event_path: event_path.as_deref(),
            follow_link: follow_link.as_deref(),
            release_title: release_row.as_ref().map(|release| release.title.as_str()),
            release_date: release_date.as_deref(),
            release_link: release_link.as_deref(),
        })
    {
        // One row per endpoint the fan holds, deduped on the action: a retried
        // claim re-runs this insert and the conflict target makes it a no-op,
        // which is what lets a crashed attempt resume without double-sending.
        push_deliveries = sqlx::query(
            r#"
            INSERT INTO fan_push_deliveries
                (workspace_id, fan_id, endpoint_id, source_kind, source_id,
                 category, title, body, target_path, collapse_key)
            SELECT $1, $2, endpoint.id, 'play_step', $3, $4, $5, $6, $7, $8
            FROM fan_push_endpoints AS endpoint
            WHERE endpoint.workspace_id = $1
              AND endpoint.fan_id = $2
              AND endpoint.active
              AND endpoint.invalidated_at IS NULL
            ON CONFLICT (workspace_id, source_kind, source_id, endpoint_id) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(action_id.into_uuid())
        .bind(copy.category)
        .bind(&copy.title)
        .bind(&copy.body)
        .bind(&copy.target_path)
        .bind(format!("play:{play_id}:step:{step_index}"))
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .rows_affected() as i64;
        step_result = Some(json!({ "push_deliveries": push_deliveries }));
    }
    // "Reached" means a delivery exists: a fan with no live endpoint got no
    // ledger row here — the alternative is counting a contact that could not
    // have happened, which inflates the reach denominator the play's verdict
    // is judged against and burns the fan out of the audience for a step
    // that never touched them.
    if fan_id.is_some() && push_deliveries > 0 {
        sqlx::query(
            r#"
            INSERT INTO play_step_recipients (workspace_id, step_id, fan_id, action_id)
            VALUES ($1,$2,$3,$4)
            ON CONFLICT (workspace_id, step_id, fan_id) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(step_id)
        .bind(fan_id.map(Uuid::from))
        .bind(action_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    if let Some(result) = &step_result {
        sqlx::query(
            "UPDATE play_steps SET result = $3 WHERE workspace_id = $1 AND id = $2 AND settled_at IS NULL",
        )
        .bind(workspace_id.into_uuid())
        .bind(step_id)
        .bind(result)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }

    // The step's own class decides whether this emit is an outward send — a
    // follow ask to a fan carries evidence, a listing sweep of our own pages
    // does not.
    let mut step_payload = json!({
        "action_id": action_id,
        "play_id": play_id,
        "play_kind": play_kind.as_str(),
        "step_index": step_index,
        "step_kind": step_kind.as_str(),
        "template_key": template_key,
        "fan_id": fan_id,
        "fan": fan.as_ref().map(|fan| {
            json!({
                "email": fan.0,
                "display_name": fan.1,
                "locale": fan.2,
            })
        }),
        "event": event_row.as_ref().map(|event| {
            json!({
                "id": event_id,
                "title": event.title,
                "slug": event.slug,
                "starts_at": crowdrelay_domain::wire_time::Wire(&event.starts_at),
                "venue": event.venue,
            })
        }),
        "release": release_row.as_ref().map(|release| {
            json!({
                "id": anchor_id,
                "title": release.title,
                "release_at": crowdrelay_domain::wire_time::Wire(&release.release_at),
                "listen_url": release.listen_url,
                "link": release_link,
            })
        }),
        "call_to_action_url": follow_link,
        "push_deliveries": push_deliveries,
        "report": step_result,
    });
    if step_kind.action_class().is_outward()
        && let Some(map) = step_payload.as_object_mut()
    {
        map.insert(
            "send_evidence".to_owned(),
            send_evidence(
                format!("play:{play_id}:step:{step_index}"),
                format!(
                    "{} step of a {} play",
                    step_kind.as_str(),
                    play_kind.as_str()
                ),
            )?,
        );
    }
    emit_external_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.play.step_requested",
        step_payload,
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
    action_id: AutopilotActionId,
) -> Result<String, RepositoryError> {
    let destination = sqlx::query_scalar::<_, String>(
        "SELECT destination_url
         FROM smart_links
         WHERE workspace_id=$1 AND slug=$2 AND active",
    )
    .bind(workspace_id.into_uuid())
    .bind(FOLLOW_ASK_SMART_LINK_SLUG)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    // Never hand two sends the same click identity. The operator-owned base
    // follow link remains destination configuration; each outward action wraps
    // it in its own deterministic redirect, so click -> action causality is
    // exact and crash retries are idempotent.
    let slug = format!("play-follow-{}", action_id.into_uuid().simple());
    sqlx::query(
        r#"
        INSERT INTO smart_links (
            workspace_id, slug, destination_url, active, channel_source, action_id
        )
        VALUES ($1,$2,$3,true,'play_follow',$4)
        ON CONFLICT (workspace_id,slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            active = true,
            action_id = COALESCE(smart_links.action_id, EXCLUDED.action_id)
        WHERE smart_links.action_id IS NULL
           OR smart_links.action_id = EXCLUDED.action_id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&slug)
    .bind(destination)
    .bind(action_id.into_uuid())
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;

    let owner = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT action_id
         FROM smart_links
         WHERE workspace_id=$1 AND slug=$2 AND active",
    )
    .bind(workspace_id.into_uuid())
    .bind(&slug)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .flatten();
    if owner != Some(action_id.into_uuid()) {
        return Err(RepositoryError::Conflict);
    }
    Ok(format!("/l/{slug}"))
}

/// What a listing sweep found on the anchor's own record.
///
/// The sweep verifies what our own database can prove — the show is published
/// (the anchor lock already guarantees it), it has a ticket link, it carries
/// an external listing reference, the venue and city are named. Everything it
/// cannot fix becomes a manual step, because inventing a Bandsintown listing
/// is work only a human logged into Bandsintown can do.
fn listing_report(event: &PlayStepEventRow) -> Value {
    let mut missing = Vec::new();
    let mut manual_steps = Vec::new();
    if event.ticket_url.is_none() {
        missing.push("ticket_url");
        manual_steps.push("add the ticket link so the listing can sell");
    }
    // A synced listing (`source_provider`) or a recorded listing URL both mean
    // the date exists somewhere outside our page; neither does not prove it is
    // unlisted, but it proves nothing says it is.
    if event.external_event_url.is_none() && event.source_provider.is_none() {
        missing.push("external_listing");
        manual_steps.push("list the date where fans look for it — Bandsintown first");
    }
    if event.venue.is_none() {
        missing.push("venue");
        manual_steps.push("name the venue");
    }
    if event.city_id.is_none() {
        missing.push("city");
        manual_steps.push("pin the city so nearby fans can find the show");
    }
    json!({
        "checked": ["published", "slug", "ticket_url", "external_listing", "venue", "city"],
        "missing": missing,
        "manual_steps": manual_steps,
    })
}

/// What the pre-save check found: the release's tracked link, or the fact
/// that there is none to point the announce step at.
fn presave_report(release: &PlayStepReleaseRow) -> Value {
    match release.link_slug.as_deref() {
        Some(slug) => json!({
            "checked": ["release_active", "presave_link"],
            "missing": [],
            "presave_link": format!("/l/{slug}"),
        }),
        None => json!({
            "checked": ["release_active", "presave_link"],
            "missing": ["presave_link"],
            "manual_steps": ["create the release's tracked link before the announce step points at it"],
        }),
    }
}

/// The canonical ticket link for an event whose own sale is live.
///
/// `None` when no sale is open — a link to a closed sale is a worse listing
/// than the gap it would fill — or when the workspace has no site of its own.
/// The URL is the workspace's own site base plus the event's public path,
/// both read rather than invented.
async fn live_sale_url(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_id: Uuid,
    event_slug: &str,
) -> Result<Option<String>, RepositoryError> {
    let sale_open = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM ticket_sales
            WHERE workspace_id = $1
              AND event_id = $2
              AND active
              AND sales_open_at <= now()
              AND sales_close_at > now()
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if !sale_open {
        return Ok(None);
    }
    // The tenant's own site or no link: a shipped default here was the first
    // tenant's site, which for anyone else is another band's show page.
    let Some(base) = sqlx::query_scalar::<_, Option<String>>(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .flatten()
    .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{}/live/{event_slug}",
        base.trim().trim_end_matches('/')
    )))
}

/// Turns the sweep's one safe finding into its own action.
///
/// The fix is proposed, never applied here: under `bounded_auto` the row
/// queues and the dispatcher applies it next pass; under anything weaker it
/// waits for the operator's click like every other approval. Either way the
/// write to `events` happens inside an action's own attempt, so the ledger —
/// not the sweep's result blob — is the record that it happened.
///
/// The idempotency key names the finding, not the run: one ticket-link fix
/// per event, so a second sweep of the same listing never queues a second
/// proposal. The returned status is what the sweep's report records.
async fn propose_ticket_url_fix(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    play_id: PlayId,
    step_index: u16,
    event_id: Uuid,
    ticket_url: &str,
) -> Result<&'static str, RepositoryError> {
    let autonomy = sqlx::query_scalar::<_, Option<String>>(
        "SELECT autonomy_level FROM autopilot_policies WHERE workspace_id = $1 AND context = 'plays'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .flatten();
    // A missing policy row is the safer read — ask — never the looser one.
    let status = if autonomy.as_deref() == Some("bounded_auto") {
        "queued"
    } else {
        "awaiting_approval"
    };
    let (decision_id, trace_id) = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT decision_id, trace_id FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let payload = serde_json::to_value(AutopilotActionPayload::SetEventTicketUrl {
        event_id: EventId::from_uuid(event_id),
        ticket_url: ticket_url.to_owned(),
        play_id,
        step_index,
    })
    .map_err(|_| RepositoryError::Unexpected)?;
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO autopilot_actions (
            workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            approved_at, approved_by, approval_expires_at,
            trace_id, causation_id
        )
        VALUES (
            $1, $2, 'plays', 'event.ticket_url.set', 'event', $3, $4, $5, $6,
            'first_party_reversible',
            CASE WHEN $6 = 'queued' THEN now() END,
            CASE WHEN $6 = 'queued' THEN 'policy:bounded_auto' END,
            CASE WHEN $6 = 'awaiting_approval' THEN now() + INTERVAL '72 hours' END,
            $7, $8
        )
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(event_id)
    .bind(format!("action:listing-fix:{event_id}:ticket_url"))
    .bind(payload)
    .bind(status)
    .bind(trace_id)
    // The causation is the sweep action itself — the trace shows the fix
    // hanging off the run that found the gap.
    .bind(action_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(if inserted.is_some() {
        status
    } else {
        // The inflight index or the idempotency key already holds this
        // proposal — a second row would be a second promise to do it.
        "already_proposed"
    })
}

/// Applies the one listing fix a sweep may propose: the ticket link that is
/// the show's own sale page.
///
/// The world is re-checked rather than the proposal trusted — a sale that
/// closed while the fix waited for approval would leave the link pointing at
/// a page that can no longer sell, which is worse than the gap it filled.
/// `Ok` when the field already holds exactly this URL: a retried execution
/// of a fix that already landed is a no-op, not a conflict.
pub(super) async fn apply_event_ticket_url(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_id: EventId,
    ticket_url: &str,
) -> Result<(), RepositoryError> {
    let applied = sqlx::query(
        r#"
        UPDATE events
        SET ticket_url = $3
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'published'
          AND ticket_url IS NULL
          AND EXISTS (
              SELECT 1 FROM ticket_sales AS sale
              WHERE sale.workspace_id = events.workspace_id
                AND sale.event_id = events.id
                AND sale.active
                AND sale.sales_open_at <= now()
                AND sale.sales_close_at > now()
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(ticket_url)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if applied.rows_affected() == 1 {
        return Ok(());
    }
    // The re-check separates "already done" from "no longer true": the same
    // URL already sitting on the row is a retried execution, anything else —
    // a link somebody else wrote, a sale that closed, a show unpublished — is
    // a world that changed since the proposal and the action fails on it.
    let already = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM events WHERE workspace_id = $1 AND id = $2 AND ticket_url = $3)",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(ticket_url)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if already {
        return Ok(());
    }
    Err(RepositoryError::Conflict)
}
