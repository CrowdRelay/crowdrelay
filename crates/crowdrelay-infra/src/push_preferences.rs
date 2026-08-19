use sqlx::PgPool;
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
pub struct FanPushPreferencesUpdate {
    pub shows_enabled: bool,
    pub releases_enabled: bool,
    pub community_enabled: bool,
    pub merch_enabled: bool,
    pub quiet_hours_enabled: bool,
    pub quiet_start_minute: i16,
    pub quiet_end_minute: i16,
}

pub async fn upsert_fan_push_preferences(
    pool: &PgPool,
    workspace_id: Uuid,
    fan_id: Uuid,
    value: FanPushPreferencesUpdate,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO fan_push_preferences (
            workspace_id, fan_id, shows_enabled, releases_enabled,
            community_enabled, merch_enabled, quiet_hours_enabled,
            quiet_start_minute, quiet_end_minute
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
        ON CONFLICT (workspace_id, fan_id) DO UPDATE SET
            shows_enabled = EXCLUDED.shows_enabled,
            releases_enabled = EXCLUDED.releases_enabled,
            community_enabled = EXCLUDED.community_enabled,
            merch_enabled = EXCLUDED.merch_enabled,
            quiet_hours_enabled = EXCLUDED.quiet_hours_enabled,
            quiet_start_minute = EXCLUDED.quiet_start_minute,
            quiet_end_minute = EXCLUDED.quiet_end_minute,
            updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(value.shows_enabled)
    .bind(value.releases_enabled)
    .bind(value.community_enabled)
    .bind(value.merch_enabled)
    .bind(value.quiet_hours_enabled)
    .bind(value.quiet_start_minute)
    .bind(value.quiet_end_minute)
    .execute(pool)
    .await
    .map(|_| ())
}
