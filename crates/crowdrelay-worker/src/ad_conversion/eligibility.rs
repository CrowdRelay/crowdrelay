use super::{AdConversionError, AdConversionWorker, Uuid};

impl AdConversionWorker {
    pub(super) async fn ensure_recipient_eligible(
        &self,
        fan_id: Option<Uuid>,
    ) -> Result<(), AdConversionError> {
        let Some(fan_id) = fan_id else {
            return Err(AdConversionError::Ineligible);
        };
        let eligible = sqlx::query_scalar::<_, bool>(
            r#"SELECT EXISTS (
                SELECT 1 FROM fans AS fan
                WHERE fan.workspace_id = $1 AND fan.id = $2 AND fan.status = 'active'
                  AND COALESCE((
                      SELECT consent.granted FROM fan_consents AS consent
                      WHERE consent.workspace_id = fan.workspace_id AND consent.fan_id = fan.id
                        AND consent.purpose = 'marketing'
                      ORDER BY consent.recorded_at DESC, consent.id DESC LIMIT 1
                  ), false)
            )"#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_one(&self.pool)
        .await?;
        if eligible {
            Ok(())
        } else {
            Err(AdConversionError::Ineligible)
        }
    }
}

#[cfg(test)]
mod tests;
