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
            .await
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
        //   → community_posts.smart_link = '/l/' || slug → community_posts.action_id
        // The LATERAL join picks the most recently *posted* community_posts
        // row for the slug — the row whose Reddit post was live when the fan
        // clicked is the one that caused the exposure. A pending/failed row
        // with the same slug never reached the community. When no
        // community_posts row exists (manually created smart links, or links
        // dropped by the executor), action_id is NULL — unattributable rather
        // than fabricated.
        sqlx::query(
            r#"
            INSERT INTO fan_provenance_events (
                workspace_id, fan_id, event_kind, channel, source_target,
                community, campaign_id, action_id, attribution_method,
                attribution_confidence, occurred_at
            )
            SELECT $1, $2, 'conversion',
                   COALESCE(link.channel_source, 'smart_link'),
                   link.slug, link.channel_community, click.campaign_id,
                   post.action_id,
                   'last_community_click', 1.0, fan.created_at
            FROM click_events AS click
            JOIN smart_links AS link
              ON link.workspace_id = click.workspace_id
             AND link.id = click.smart_link_id
            JOIN fans AS fan
              ON fan.workspace_id = $1
             AND fan.id = $2
            LEFT JOIN LATERAL (
                SELECT post.action_id
                FROM community_posts AS post
                WHERE post.workspace_id = $1
                  AND post.smart_link = '/l/' || link.slug
                ORDER BY post.posted_at DESC NULLS LAST,
                         post.created_at DESC
                LIMIT 1
            ) AS post ON true
            WHERE click.workspace_id = $1
              AND click.anonymous_visitor_id = $3
              AND link.channel_community IS NOT NULL
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
        Ok(())
    }

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
    Ok(())
}
