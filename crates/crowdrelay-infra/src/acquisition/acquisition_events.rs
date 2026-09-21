// Provenance writes — how a fan arrived, recorded in the same
// transaction that created the fan row.
//
// These three sit together because they share one contract: an
// acquisition event asserts how a fan *arrived*, so it may only be
// written for a fan row the transaction actually created, and it must
// commit or roll back with that row. Split out of
// `persistence_methods.rs` when that chunk crossed the modularity
// contract's 1000-line ceiling.

impl PostgresAcquisitionRepository {
    async fn insert_acquisition_event(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        signup: &FanSignup,
        request_id: &str,
        referral: Option<&ReferralOwnerRow>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO fan_acquisition_events (
                workspace_id,
                fan_id,
                campaign_id,
                anonymous_visitor_id,
                source,
                request_id,
                referral_code_id,
                referrer_fan_id,
                occurred_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(signup.campaign_id().map(Into::<Uuid>::into))
        .bind(signup.visitor_id().map(Into::<Uuid>::into))
        .bind(signup.consent().source())
        .bind(request_id)
        .bind(referral.as_ref().map(|row| row.id))
        .bind(referral.map(|row| row.fan_id))
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        self.record_community_conversion(transaction, workspace_id, fan_id, signup)
            .await?;
        if let Some(referral) = referral {
            self.record_referral_conversion(transaction, workspace_id, fan_id, referral)
                .await?;
        }
        Ok(())
    }

    /// Records a referral conversion beside the click-attributed one, when
    /// the signup claimed a referral code.
    ///
    /// Both rows can be true at once: a fan who clicked a community link and
    /// then arrived on a friend's referral carries two honest attributions —
    /// `last_tracked_click` names the channel that produced the click, this
    /// one names the fan who sent them. Channel-level counts may therefore
    /// overlap; per-fan truth stays single because each row asserts a
    /// different channel, and the measurement layer dedupes within one.
    ///
    /// `source_target` names the referrer, not the code — the durable fact is
    /// that fan sent this fan, and the code id is already on the acquisition
    /// row for whoever needs the join.
    async fn record_referral_conversion(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        referral: &ReferralOwnerRow,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO fan_provenance_events (
                workspace_id, fan_id, event_kind, channel, source_target,
                attribution_method, attribution_confidence, occurred_at
            )
            SELECT $1, $2, 'conversion', 'referral', 'fan:' || $3::text,
                   'referral_code', 1.0, fan.created_at
            FROM fans AS fan
            WHERE fan.workspace_id = $1
              AND fan.id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(referral.fan_id)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    /// Links a signup back to the community that sent it, when the visitor
    /// arrived through a community-tagged smart link.
    ///
    /// This is the conversion half of the provenance chain the ledger was
    /// built for. Exposure was already being recorded at dispatch; nothing
    /// wrote conversion, so every community-level outcome query matched
    /// nothing — and `COUNT` reports nothing as zero, which reads exactly like
    /// a community that converted no one. The two facts are not the same and
    /// the measurement layer cannot tell them apart on its own.
    ///
    /// Attribution is the visitor's most recent community-tagged click inside
    /// thirty days, recorded as such. Naming the method is the point: this
    /// records an observed click, not a causal claim, and the causal layer is
    /// still the only thing entitled to make one.
    ///
    /// # Two timestamps, two meanings
    ///
    /// `occurred_at` is when the conversion happened — the signup — and
    /// `created_at` defaults to when the row was written. They were the same
    /// expression: `occurred_at` was bound to `now()`, which is observation
    /// time wearing an occurrence-time name.
    ///
    /// Identical in the happy path, because this runs in the signup's own
    /// transaction. Not identical on a backfill or a replayed signup, where
    /// `now()` is the replay and the conversion would land in whichever
    /// fourteen-day measurement window the replay happened to fall in rather
    /// than the one it belongs to. The fan's own `created_at` is the
    /// occurrence, and it is already durable.
    ///
    /// # `attribution_confidence` is the rule's weight, not a probability
    ///
    /// `1.0` here means "the last-community-click rule assigns this
    /// conversion wholly to that community", not "this attribution is certain".
    /// Last-touch over thirty days is a heuristic and can be wrong — a fan who
    /// clicked on day one and arrived through a friend on day twenty-nine is
    /// attributed entirely to the click.
    ///
    /// It does not reach learning as evidence strength: the measurement query
    /// counts `DISTINCT fan_id` and ignores this column, and evidence quality
    /// is decided by `measured_evidence_quality` from whether the outcome was
    /// read at the level the randomisation was performed at. Said here because
    /// a column called confidence holding 1.0 invites exactly the reading it
    /// must not be given.
    async fn record_community_conversion(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        signup: &FanSignup,
    ) -> Result<(), StoreError> {
        let Some(visitor_id) = signup.visitor_id() else {
            return Ok(());
        };
        // occurred_at must be the fan's created_at — the actual conversion
        // time — not now() (write time). If the fan does not exist, the JOIN
        // produces no rows and no conversion is written: fail-closed rather
        // than fabricating a timestamp for an event we cannot anchor.
        //
        // action_id is recovered via the smart_link chain:
        //   click_events.smart_link_id → smart_links.slug
        //   → post.smart_link = '/l/' || slug → post.action_id
        // where `post` is whichever executor ledger owns the link —
        // community_posts, social_posts, telegram_posts or discord_posts all
        // store the same '/l/{slug}' text. The UNION picks the most recently
        // *posted* row across all four — the post that was live when the fan
        // clicked is the one that caused the exposure. A pending/failed row
        // with the same slug never reached the channel. When no post row
        // exists at all (manually created links, or links dropped by the
        // executor), action_id is NULL — unattributable rather than
        // fabricated.
        //
        // The gate is `channel_source IS NOT NULL`, not `channel_community`:
        // a community is a fact only Reddit-style posts have. A Telegram
        // post's link carries its channel name, an Instagram post's carries
        // no community at all — and both are still attributable at the
        // channel level. Gating on community made every non-Reddit signup
        // anonymous to the ledger while the channels that were actually
        // switched on taught the brain nothing.
        //
        // format_key continues one hop further along the same chain:
        //   action → payload.source_id → content_sources.format_key
        // — the catalogue format the promoted artifact was declared in. The
        // outcome-ingest gate means every posted thread names a live source,
        // so a NULL here says "the source was filed without a format", which
        // is an honest unrecorded, not a join failure.
        sqlx::query(
            r#"
            INSERT INTO fan_provenance_events (
                workspace_id, fan_id, event_kind, channel, source_target,
                community, campaign_id, action_id, attribution_method,
                attribution_confidence, occurred_at, format_key
            )
            SELECT $1, $2, 'conversion',
                   COALESCE(link.channel_source, 'smart_link'),
                   link.slug, link.channel_community, click.campaign_id,
                   post.action_id,
                   'last_tracked_click', 1.0, fan.created_at,
                   post.format_key
            FROM click_events AS click
            JOIN smart_links AS link
              ON link.workspace_id = click.workspace_id
             AND link.id = click.smart_link_id
            JOIN fans AS fan
              ON fan.workspace_id = $1
             AND fan.id = $2
            LEFT JOIN LATERAL (
                SELECT post.action_id, source.format_key
                FROM (
                    -- community_posts stores only the text form of the link.
                    SELECT action_id, posted_at, created_at
                    FROM community_posts
                    WHERE workspace_id = $1 AND smart_link = '/l/' || link.slug
                    UNION ALL
                    -- The other three carry the link's id too; matching it
                    -- survives a NULL or rewritten text column.
                    SELECT action_id, posted_at, created_at
                    FROM social_posts
                    WHERE workspace_id = $1
                      AND (smart_link = '/l/' || link.slug OR smart_link_id = link.id)
                    UNION ALL
                    SELECT action_id, posted_at, created_at
                    FROM telegram_posts
                    WHERE workspace_id = $1
                      AND (smart_link = '/l/' || link.slug OR smart_link_id = link.id)
                    UNION ALL
                    SELECT action_id, posted_at, created_at
                    FROM discord_posts
                    WHERE workspace_id = $1
                      AND (smart_link = '/l/' || link.slug OR smart_link_id = link.id)
                ) AS post
                LEFT JOIN autopilot_actions AS act
                  ON act.workspace_id = $1
                 AND act.id = post.action_id
                LEFT JOIN content_sources AS source
                  ON source.workspace_id = $1
                 AND source.id::text = lower(act.payload->>'source_id')
                ORDER BY post.posted_at DESC NULLS LAST,
                         post.created_at DESC
                LIMIT 1
            ) AS post ON true
            WHERE click.workspace_id = $1
              AND click.anonymous_visitor_id = $3
              AND link.channel_source IS NOT NULL
              AND click.occurred_at >= now() - INTERVAL '30 days'
            ORDER BY click.occurred_at DESC
            LIMIT 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(Into::<Uuid>::into(visitor_id))
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        // Link the visitor's anonymous history to the fan they just became.
        // Interaction rows are written at click time with fan_id NULL — this
        // is the "once the fan converts, the fan_id is linked" half the
        // schema promised. Only unlinked rows move: an earlier fan's history
        // is never re-pointed, and a shared device keeps both trails honest.
        sqlx::query(
            r#"
            UPDATE fan_provenance_events
            SET fan_id = $2
            WHERE workspace_id = $1
              AND anonymous_visitor_id = $3
              AND fan_id IS NULL
              AND event_kind IN ('exposure', 'interaction')
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(Into::<Uuid>::into(visitor_id))
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

}

/// What the arrival's call site knows beyond the source string — carried as
/// a struct so each caller passes only what it honestly has. Empty is a
/// complete answer: the provenance row still records the channel.
#[derive(Clone, Debug, Default)]
pub struct ArrivalContext {
    /// The thing the arrival came through — an event slug, a sale reference,
    /// an import batch label. `None` when the path has no finer target.
    pub source_target: Option<String>,
    /// The campaign the arrival belongs to, when the caller holds one.
    pub campaign_id: Option<Uuid>,
}

/// Records a fan's arrival from a path that is not `POST /v1/fans` — a QR
/// check-in, an import, a fanbase ingestion — in the same transaction that
/// created the fan row.
///
/// `insert_acquisition_event` (above) only runs on the public signup path, so
/// every other way into the fanbase landed in `fans` with no provenance row —
/// and a channel ROI readout that counts acquisition events reports those
/// arrivals as absent, not zero. The instrument has to precede the data, or
/// the readout measures `public_signup` forever.
///
/// The provenance write is what makes the arrival visible to the brain: a
/// `fan_acquisition_events` row is the coarse ledger, `fan_provenance_events`
/// is the one ranking and measurement read. A show that converts twenty
/// people in the room only teaches the brain that shows convert if the
/// arrival lands in the ledger it reads — with `channel` set to the arrival
/// path and `community`/`action_id` honestly NULL, because a QR code is not
/// a community and was never an autopilot action.
///
/// Caller contract: invoke only when the fan INSERT actually created a row —
/// an `ON CONFLICT` hit means the fan already has provenance from wherever
/// they first arrived, and a second row would fabricate a second arrival.
/// `source` names the path (`concert_qr`, `fan_import:csv`, `fanbase_ingest`,
/// `ticket_purchase`, `synesthesia_claim`); `request_id` correlates to the
/// operation that created the fan, the same role the signup request's
/// correlation id plays on the signup path.
pub async fn record_fan_arrival(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
    source: &str,
    request_id: &str,
    context: &ArrivalContext,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO fan_acquisition_events (
            workspace_id, fan_id, source, request_id, occurred_at
        )
        VALUES ($1, $2, $3, $4, now())
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(source)
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    // `occurred_at` anchors on the fan's own created_at — the arrival — for
    // the same reason the click conversion does: a replayed import writes at
    // replay time but arrived when the fan did. The JOIN fails closed: no
    // fan row, no provenance.
    sqlx::query(
        r#"
        INSERT INTO fan_provenance_events (
            workspace_id, fan_id, event_kind, channel, source_target,
            campaign_id, attribution_method, attribution_confidence,
            occurred_at
        )
        SELECT $1, $2, 'conversion', $3, $4, $5,
               'direct_arrival', 1.0, fan.created_at
        FROM fans AS fan
        WHERE fan.workspace_id = $1
          AND fan.id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(source)
    .bind(&context.source_target)
    .bind(context.campaign_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}
