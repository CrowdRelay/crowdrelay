macro_rules! decision_booking_reads {
    () => {
    async fn load_booking_target_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<Vec<BookingTargetSnapshot>, RepositoryError> {
        self.bounded(async {
            // The requesting tenant's own genre set, one scalar up front —
            // the "mine" half of every comparable-acts test, resolved through
            // the shared alias map the same way `city_venues` does it.
            let my_genres = sqlx::query_scalar::<_, Vec<String>>(
                r#"
                SELECT COALESCE(array_agg(DISTINCT lower(btrim(tag))), '{}')
                FROM band_listings AS listing, unnest(listing.genre_tags) AS tag
                WHERE listing.workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;
            sqlx::query_as::<_, BookingTargetRow>(
                r#"
                SELECT target.id AS target_id, target.city_id, target.target_kind,
                       target.display_name, target.capacity, target.version, target.active, target.accepts_booking, target.priority,
                       target.relationship_score,
                       EXISTS (
                           SELECT 1
                           FROM autopilot_actions AS action
                           WHERE action.workspace_id = target.workspace_id
                             AND action.action_kind = 'booking.outreach.request'
                             AND action.status IN ('awaiting_approval', 'queued', 'processing')
                             AND action.payload ->> 'target_id' = target.id::text
                       ) AS outreach_in_flight,
                       target.last_outreach_at,
                       COALESCE((SELECT count(*)::integer FROM booking_interactions interaction
                         WHERE interaction.workspace_id=target.workspace_id AND interaction.target_id=target.id
                           AND interaction.direction='outbound' AND interaction.phase='followup'),0) AS followup_count,
                       COALESCE((SELECT interaction.disposition FROM booking_interactions interaction
                         WHERE interaction.workspace_id=target.workspace_id AND interaction.target_id=target.id
                           AND interaction.direction='inbound' AND interaction.phase='reply'
                         ORDER BY interaction.occurred_at DESC,interaction.id DESC LIMIT 1),'none') AS last_reply_disposition,
                       target.venue_id,
                       COALESCE(room_evidence.shows_last_12m, 0)::bigint AS shows_last_12m,
                       room_evidence.days_since_last_event,
                       COALESCE(comparable.comparable_acts, 0)::bigint AS comparable_acts,
                       genres_fact.value AS venue_genres,
                       capacity_fact.value AS venue_capacity,
                       contact_age.booking_contact_days,
                       -- §12-5 entity 5: the soonest closing application
                       -- window of the festival's editions. CEIL turns a
                       -- partially elapsed day into a full one — a window
                       -- shutting in two hours reads as "1 day", which is
                       -- the honest answer for "is there still time". The
                       -- HAVING keeps a target with no open window at NULL:
                       -- GREATEST(0, NULL) would answer 0 — "closes today" —
                       -- exactly backwards.
                       (SELECT GREATEST(0, CEIL(EXTRACT(EPOCH FROM
                               (MIN(edition.application_closes_at) - now())) / 86400.0))::bigint
                        FROM festival_editions AS edition
                        WHERE edition.workspace_id = target.workspace_id
                          AND edition.target_id = target.id
                          AND edition.application_closes_at >= now()
                        HAVING MIN(edition.application_closes_at) IS NOT NULL)
                           AS days_until_application_close,
                       -- The same window as a timestamp: the countdown tells
                       -- a reader how long is left, the timestamp tells a
                       -- decision which edition it is deciding on. Both read
                       -- the same MIN so they can never disagree.
                       (SELECT MIN(edition.application_closes_at)
                        FROM festival_editions AS edition
                        WHERE edition.workspace_id = target.workspace_id
                          AND edition.target_id = target.id
                          AND edition.application_closes_at >= now())
                           AS next_application_closes_at,
                       -- §12-5 entity 6: a promoter's rooms are the union of
                       -- the primary venue_id and every edge row. UNION
                       -- dedupes a room that is both.
                       ARRAY(
                           SELECT edge_or_primary.venue_id
                           FROM (
                               SELECT edge.venue_id
                               FROM booking_target_venues AS edge
                               WHERE edge.workspace_id = target.workspace_id
                                 AND edge.target_id = target.id
                               UNION
                               SELECT target.venue_id
                           ) AS edge_or_primary
                           WHERE edge_or_primary.venue_id IS NOT NULL
                       ) AS linked_venue_ids
                FROM booking_targets AS target
                -- §12-6 evidence: the room's own recent history. Past shows
                -- only — a booked future night is not played yet — and the
                -- mark/event join keys on both id and workspace, so the
                -- count is the shared registry's aggregate, not a peek at
                -- another tenant's mark rows.
                LEFT JOIN LATERAL (
                    SELECT count(*)::bigint AS shows_last_12m,
                           EXTRACT(day FROM now() - max(mark_event.starts_at))::bigint
                               AS days_since_last_event
                    FROM place_venue_marks AS mark
                    JOIN events AS mark_event
                      ON mark_event.id = mark.event_id
                     AND mark_event.workspace_id = mark.workspace_id
                    WHERE mark.venue_id = target.venue_id
                      AND mark_event.starts_at <= now()
                      AND mark_event.starts_at > now() - interval '365 days'
                ) AS room_evidence ON target.venue_id IS NOT NULL
                -- Distinct bill acts at the room whose genres intersect the
                -- tenant's, both sides canonicalised through
                -- place_genre_aliases — the same shape city_venues uses.
                -- The requesting workspace's own acts never count.
                LEFT JOIN LATERAL (
                    SELECT count(DISTINCT COALESCE(
                               act.act_workspace_id::text, act.peer_act_id::text))::bigint
                           AS comparable_acts
                    FROM place_venue_marks AS mark
                    JOIN event_acts AS act
                      ON act.event_id = mark.event_id
                     AND act.workspace_id = mark.workspace_id
                    WHERE mark.venue_id = target.venue_id
                      AND (act.act_workspace_id IS NULL OR act.act_workspace_id <> $1)
                      AND EXISTS (
                          SELECT 1
                          FROM (
                              SELECT COALESCE(mine_alias.canonical, mine_tag.genre) AS genre
                              FROM unnest($2::text[]) AS mine_tag(genre)
                              LEFT JOIN place_genre_aliases AS mine_alias
                                ON mine_alias.alias = mine_tag.genre
                          ) AS mine
                          JOIN (
                              SELECT COALESCE(their_alias.canonical,
                                              lower(btrim(their_genre.genre))) AS genre
                              FROM (
                                  SELECT unnest(listing.genre_tags) AS genre
                                  FROM band_listings AS listing
                                  WHERE listing.workspace_id = act.act_workspace_id
                                  UNION ALL
                                  SELECT peer_genre.genre_tag
                                  FROM place_peer_act_genres AS peer_genre
                                  WHERE peer_genre.peer_act_id = act.peer_act_id
                              ) AS their_genre
                              LEFT JOIN place_genre_aliases AS their_alias
                                ON their_alias.alias = lower(btrim(their_genre.genre))
                          ) AS theirs ON theirs.genre = mine.genre
                      )
                ) AS comparable ON target.venue_id IS NOT NULL
                -- The winning global fact per attribute: workspace_id IS NULL
                -- is load-bearing — a private fact never surfaces here.
                LEFT JOIN LATERAL (
                    SELECT fact.value
                    FROM place_venue_facts AS fact
                    WHERE fact.venue_id = target.venue_id
                      AND fact.workspace_id IS NULL
                      AND fact.attribute = 'genres'
                      AND (fact.expires_at IS NULL OR fact.expires_at > now())
                    ORDER BY CASE fact.provenance
                                 WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                                 WHEN 'event_evidence' THEN 2
                                 WHEN 'open_directory' THEN 3 ELSE 4 END,
                             fact.observed_at DESC
                    LIMIT 1
                ) AS genres_fact ON target.venue_id IS NOT NULL
                LEFT JOIN LATERAL (
                    SELECT fact.value
                    FROM place_venue_facts AS fact
                    WHERE fact.venue_id = target.venue_id
                      AND fact.workspace_id IS NULL
                      AND fact.attribute = 'capacity'
                      AND (fact.expires_at IS NULL OR fact.expires_at > now())
                    ORDER BY CASE fact.provenance
                                 WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                                 WHEN 'event_evidence' THEN 2
                                 WHEN 'open_directory' THEN 3 ELSE 4 END,
                             fact.observed_at DESC
                    LIMIT 1
                ) AS capacity_fact ON target.venue_id IS NOT NULL
                -- The tenant's own freshest booking_email fact — age only.
                -- The value never leaves the facts table; another tenant's
                -- fact can never appear because workspace_id is bound to $1.
                LEFT JOIN LATERAL (
                    SELECT EXTRACT(day FROM now() - max(fact.observed_at))::bigint
                           AS booking_contact_days
                    FROM place_venue_facts AS fact
                    WHERE fact.workspace_id = $1
                      AND fact.venue_id = target.venue_id
                      AND fact.attribute = 'booking_email'
                ) AS contact_age ON target.venue_id IS NOT NULL
                WHERE target.workspace_id = $1
                  -- A venue-kind target IS its room: when the room's
                  -- *resolved* status fact is 'closed', the target is dead
                  -- and pitching it is the dead-venue mistake this filter
                  -- exists to prevent. Resolved, not merely present — a
                  -- newer 'active' claim lifts the exclusion, the same
                  -- ladder `best_venue` applies. Promoter/agent targets keep
                  -- their venue links as evidence: the room may be dead, the
                  -- booker is not.
                  AND NOT (
                      target.target_kind = 'venue'
                      AND EXISTS (
                          SELECT 1
                          FROM (
                              SELECT target.venue_id AS linked_venue_id
                              UNION
                              SELECT edge.venue_id
                              FROM booking_target_venues AS edge
                              WHERE edge.workspace_id = target.workspace_id
                                AND edge.target_id = target.id
                          ) AS linked
                          WHERE COALESCE((
                              SELECT lower(btrim(status_fact.value))
                              FROM place_venue_facts AS status_fact
                              WHERE status_fact.venue_id = linked.linked_venue_id
                                AND status_fact.attribute = 'status'
                                AND (status_fact.workspace_id IS NULL
                                     OR status_fact.workspace_id = $1)
                                AND (status_fact.expires_at IS NULL
                                     OR status_fact.expires_at > now())
                              ORDER BY CASE status_fact.provenance
                                           WHEN 'played' THEN 0
                                           WHEN 'researched' THEN 1
                                           WHEN 'event_evidence' THEN 2
                                           WHEN 'open_directory' THEN 3
                                           ELSE 4 END,
                                       status_fact.observed_at DESC
                              LIMIT 1
                          ), '') = 'closed'
                      )
                  )
                ORDER BY target.city_id, target.priority DESC,
                         target.relationship_score DESC, target.id
                LIMIT 2000
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&my_genres)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(booking_target_snapshot)
            .collect()
        })
        .await
    }

    async fn load_booking_window_inputs_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<BookingWindowInputSet, RepositoryError> {
        self.bounded(async {
            // The tenant's own calendar — loaded once, shared across every
            // target. Published and draft shows both block the window; only
            // a published one can ever be named an adjacent night. Cancelled
            // and completed events are neither.
            let own_rows = sqlx::query_as::<_, BookingWindowOwnShowRow>(
                r#"
                SELECT event.starts_at, event.slug, event.status,
                       city.latitude, city.longitude
                FROM events AS event
                LEFT JOIN cities AS city
                  ON city.id = event.city_id
                WHERE event.workspace_id = $1
                  AND event.status IN ('draft', 'published')
                ORDER BY event.starts_at, event.id
                LIMIT 500
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let own_shows = own_rows
                .into_iter()
                .map(|row| BookingWindowOwnShow {
                    starts_at: row.starts_at,
                    slug: row.slug,
                    coords: row.latitude.zip(row.longitude),
                    confirmed: row.status == "published",
                })
                .collect();

            // Each venue-linked target's room history — the newest 40 past
            // shows per room — plus the room's coordinates, which exist only
            // through the venue's city. A city with no coordinates yields
            // `venue_coords: None` and the adjacent-show basis never fires:
            // a missing coordinate stays missing rather than inventing a
            // distance.
            let room_rows = sqlx::query_as::<_, BookingWindowRoomRow>(
                r#"
                WITH room_shows AS (
                    SELECT mark.venue_id, mark_event.starts_at, mark_event.created_at,
                           row_number() OVER (
                               PARTITION BY mark.venue_id
                               ORDER BY mark_event.starts_at DESC, mark_event.id
                           ) AS rn
                    FROM place_venue_marks AS mark
                    JOIN events AS mark_event
                      ON mark_event.id = mark.event_id
                     AND mark_event.workspace_id = mark.workspace_id
                    WHERE mark_event.starts_at <= $2
                )
                SELECT target.id AS target_id,
                       room_shows.starts_at, room_shows.created_at,
                       city.latitude, city.longitude
                FROM booking_targets AS target
                LEFT JOIN place_venues AS venue
                  ON venue.id = target.venue_id
                LEFT JOIN cities AS city
                  ON city.id = venue.city_id
                LEFT JOIN room_shows
                  ON room_shows.venue_id = target.venue_id
                 AND room_shows.rn <= 40
                WHERE target.workspace_id = $1
                  AND target.venue_id IS NOT NULL
                ORDER BY target.id, room_shows.starts_at DESC
                LIMIT 84000
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            // Rows arrive grouped by target (ORDER BY target.id); fold each
            // run into one target's input set.
            let mut targets: Vec<BookingWindowTargetInputs> = Vec::new();
            for row in room_rows {
                let needs_new = targets
                    .last()
                    .is_none_or(|input| input.target_id.into_uuid() != row.target_id);
                if needs_new {
                    targets.push(BookingWindowTargetInputs {
                        target_id: BookingTargetId::from_uuid(row.target_id),
                        room_shows: Vec::new(),
                        venue_coords: row.latitude.zip(row.longitude),
                    });
                }
                if let (Some(starts_at), Some(created_at)) = (row.starts_at, row.created_at)
                    && let Some(input) = targets.last_mut()
                {
                    input.room_shows.push((starts_at, created_at));
                }
            }
            Ok(BookingWindowInputSet { own_shows, targets })
        })
        .await
    }
    }
}
