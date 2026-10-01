// Event-network discovery (the Brain's scout for one upcoming show).
//
// Included into `autopilot.rs` beside the other executors. It lived inline in
// the dispatch `match`, which is the file the size ratchet watches: a hundred
// and twenty lines of one arm pushed `actions_execution.rs` past its
// allowance. The body is moved, not changed.

pub(super) async fn execute_beacon_discovery(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    event_id: crowdrelay_domain::EventId,
    target_count: u16,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = sqlx::query_as::<_, (
        uuid::Uuid,
        String,
        Option<String>,
        OffsetDateTime,
        String,
        String,
        Option<String>,
    )>(
        r#"
        SELECT city.id, event.title, event.venue, event.starts_at, city.name,
               city.country_code, city.region
        FROM events event
        JOIN cities city ON city.id=event.city_id
        WHERE event.workspace_id=$1 AND event.id=$2
          AND event.status='published' AND event.starts_at>$3
        FOR SHARE OF event
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(now)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    // Known bill-mates and the host venue are seed entities, not
    // conclusions. The research agent still has to find public
    // evidence and may return nothing for a seed it cannot
    // verify. Organization siblings stay on the first-party
    // roster path instead of being rediscovered as cold leads.
    let seed_acts = sqlx::query_scalar::<_, String>(
        r#"
        SELECT act.act_name
        FROM event_acts act
        JOIN workspaces own_ws ON own_ws.id = act.workspace_id
        LEFT JOIN workspaces act_ws ON act_ws.id = act.act_workspace_id
        WHERE act.workspace_id = $1 AND act.event_id = $2
          AND act.act_workspace_id IS DISTINCT FROM $1
          AND act.act_slug IS DISTINCT FROM own_ws.slug
          AND (act_ws.organization_id IS NULL
               OR act_ws.organization_id IS DISTINCT FROM own_ws.organization_id)
        GROUP BY act.act_name
        ORDER BY MIN(act.position)
        LIMIT 24
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .fetch_all(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let mut seed_entities = seed_acts
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "kind": "scene_partner",
                "role": "bill_mate",
                "evidence": "listed on the show's bill",
            })
        })
        .collect::<Vec<_>>();
    if let Some(venue) = event
        .2
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty() && !seed_acts.iter().any(|name| name == v))
    {
        seed_entities.push(json!({
            "name": venue,
            "kind": "venue",
            "role": "host_venue",
            "evidence": "hosts the show",
        }));
    }

    // This used to emit crowdrelay.beacon.discovery_requested
    // onto an executor capability with no route. The Brain made
    // a good decision and the outbox permanently 422'd it.
    // Research is exactly what the agents service already
    // executes well, so hand the Brain's event-scoped brief to
    // a premium research task instead of inventing another
    // external executor.
    let brief = json!({
        "event": {
            "id": event_id,
            "city_id": event.0,
            "title": event.1,
            "venue": event.2,
            "starts_at": crowdrelay_domain::wire_time::Wire(&event.3),
            "city": event.4,
            "country_code": event.5,
            "region": event.6,
        },
        "target_count": target_count,
        "seed_entities": seed_entities,
        "allowed_kinds": [
            "radio", "local_press", "television", "reviewer",
            "creator", "photographer", "promoter", "venue",
            "scene_partner", "patron", "community"
        ],
        "priority_source_classes": [
            "local_metal_media_and_podcasts",
            "independent_radio_and_music_programmes",
            "venue_promoter_support_band_networks",
            "record_stores_rehearsal_studios_and_music_shops",
            "tattoo_alt_fashion_and_scene_businesses",
            "student_culture_portals_and_local_event_calendars",
            "moderated_metal_communities_and_forums",
            "local_live_creators_photographers_and_reviewers",
            "local_artists_craftspeople_and_alternative_culture_nodes"
        ],
        "rules": [
            "public sources only",
            "every candidate needs a source URL that proves it exists",
            "prefer local scene trust and event relevance over generic reach",
            "never scrape private member lists or personal contact data",
            "do not contact anybody: this task only expands the reviewed local graph",
            "communities must respect their published rules",
            "a generic local business is not scene-relevant without public evidence"
        ]
    });
    let prompt = format!(
        "Build the local growth network around this upcoming show. Find real public people, organizations and communities that can credibly help attendance, local awareness or show execution. Do broad web research rather than relying on a fixed forum list. Return only evidence-backed candidates for the exact event/city in this brief.\n\n{}",
        serde_json::to_string_pretty(&brief).map_err(|_| RepositoryError::Unexpected)?
    );
    operations::execute_agent_run(
        transaction,
        workspace_id,
        action_id,
        "event-network-scout",
        &prompt,
        3,
        crowdrelay_brain::AgentTier::Premium,
        now,
    )
    .await?;
    Ok(())
}
