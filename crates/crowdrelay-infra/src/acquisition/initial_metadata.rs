use super::*;

pub(super) async fn persist(
    transaction: &mut Transaction<'_, Postgres>,
    signup: &FanSignup,
    fan_id: FanId,
) -> Result<(), StoreError> {
    let Some(metadata) = signup.initial_metadata() else {
        return Ok(());
    };
    let workspace_id = signup.workspace_id().into_uuid();
    if let Some(context) = &metadata.capture_context {
        sqlx::query("INSERT INTO fan_capture_contexts(workspace_id,fan_id,context) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(workspace_id).bind(fan_id.into_uuid()).bind(serde_json::json!(context))
            .execute(&mut **transaction).await.map_err(StoreError::from_sqlx)?;
    }
    if let (Some(city), Some((enabled, radius))) = (signup.city_slug(), metadata.nearby_gigs) {
        sqlx::query(r#"
            INSERT INTO fan_location_preferences (workspace_id, fan_id, city_id, nearby_gigs_enabled, radius_km)
            SELECT $1, $2, cities.id, $4, $5 FROM cities
            JOIN fan_city_interests interest ON interest.city_id = cities.id
                AND interest.workspace_id = $1 AND interest.fan_id = $2
            WHERE cities.slug = $3
            ON CONFLICT (workspace_id, fan_id) DO NOTHING
        "#).bind(workspace_id).bind(fan_id.into_uuid()).bind(city.as_str())
            .bind(enabled).bind(radius).execute(&mut **transaction).await.map_err(StoreError::from_sqlx)?;
    }
    persist_fan_ad_attribution(
        &mut **transaction,
        workspace_id,
        fan_id.into_uuid(),
        &FanAdAttributionParams {
            meta_fbp: metadata.meta_fbp.as_deref(),
            meta_fbc: metadata.meta_fbc.as_deref(),
            google_gclid: metadata.google_gclid.as_deref(),
            bandsintown_ref: metadata.bandsintown_ref.as_deref(),
            utm_source: metadata.utm_source.as_deref(),
            utm_medium: metadata.utm_medium.as_deref(),
            utm_campaign: metadata.utm_campaign.as_deref(),
            utm_content: metadata.utm_content.as_deref(),
            utm_term: metadata.utm_term.as_deref(),
            event_source_url: metadata.event_source_url.as_deref(),
            client_ip_address: metadata.client_ip_address.as_deref(),
            client_user_agent: metadata.client_user_agent.as_deref(),
        },
    )
    .await
    .map_err(StoreError::from_sqlx)?;
    Ok(())
}
