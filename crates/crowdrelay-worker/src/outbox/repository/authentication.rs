use super::{PgOutboxStore, StoreError, Uuid};
use std::collections::HashSet;

type AuthenticationRecipient = (Uuid, Uuid, [u8; 32]);

impl PgOutboxStore {
    pub(crate) async fn eligible_authentication_tokens(
        &self, recipients: &[AuthenticationRecipient],
    ) -> Result<HashSet<AuthenticationRecipient>, StoreError> {
        if recipients.is_empty() { return Ok(HashSet::new()); }
        let workspaces: Vec<Uuid> = recipients.iter().map(|row| row.0).collect();
        let fans: Vec<Uuid> = recipients.iter().map(|row| row.1).collect();
        let hashes: Vec<Vec<u8>> = recipients.iter().map(|row| row.2.to_vec()).collect();
        let rows = sqlx::query_as::<_, (Uuid, Uuid, Vec<u8>)>(r#"
            SELECT fan.workspace_id, fan.id, token.token_hash
            FROM unnest($1::uuid[], $2::uuid[], $3::bytea[]) AS recipient(workspace_id, fan_id, token_hash)
            JOIN fans AS fan ON fan.workspace_id=recipient.workspace_id AND fan.id=recipient.fan_id
            JOIN fan_action_tokens AS token ON token.workspace_id=fan.workspace_id AND token.fan_id=fan.id
                AND token.token_hash=recipient.token_hash
            WHERE fan.deleted_at IS NULL AND fan.status IN ('pending','active','unsubscribed')
              AND token.purpose IN ('confirm','session') AND token.consumed_at IS NULL AND token.expires_at > now()
        "#).bind(workspaces).bind(fans).bind(hashes).fetch_all(&self.pool).await.map_err(StoreError::Database)?;
        rows.into_iter().map(|(workspace,fan,hash)| {
            Ok((workspace,fan,hash.try_into().map_err(|_| StoreError::InvalidValue)?))
        }).collect()
    }
}
