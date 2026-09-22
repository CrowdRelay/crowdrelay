//! Per-workspace encrypted secret store.
//!
//! Tenants configure credentials (Stripe keys first) through the control
//! plane instead of redeploying env vars. Values are sealed with
//! XChaCha20-Poly1305 under a key derived from the configured response
//! secret on its own domain (`derive_workspace_secrets_key` in config), and
//! the workspace id plus secret name are authenticated as associated data —
//! a row copied to another workspace or renamed cannot be opened.
//!
//! The store is deliberately boring: a write is a sealed upsert, a reveal
//! happens only inside a purpose-scoped handler, and the operator read path
//! returns `masked_hint` — the key prefix and last four computed at write
//! time — never the plaintext.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::sensitive_response::{
    SensitiveResponseError, SensitiveResponseKey, decrypt_value, encrypt_value,
};

const ASSOCIATED_DATA_DOMAIN: &[u8] = b"crowdrelay.workspace-secret.v1\0";

/// Domain separator the secrets key is derived under — public because the
/// derivation happens in config and tests need the same key, not because the
/// value is sensitive. It is a label, not a secret.
pub const KEY_DERIVATION_DOMAIN: &[u8] = b"crowdrelay.workspace-secrets.key.v1\0";

/// One secret's operator-visible state: enough to answer "is it set, and
/// which key is it" without revealing it.
#[derive(Clone, Debug)]
pub struct MaskedSecret {
    pub name: String,
    pub masked_hint: String,
    pub updated_at: OffsetDateTime,
}

/// Failure while sealing or opening a workspace secret.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceSecretsError {
    /// The database could not complete the operation.
    #[error("workspace secrets store failed")]
    Database(#[source] sqlx::Error),
    /// The stored row could not be opened — the workspace or name it was
    /// sealed for changed, or neither configured key is the writer's.
    #[error("workspace secret failed to open")]
    CannotOpen,
}

impl From<sqlx::Error> for WorkspaceSecretsError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SensitiveResponseError> for WorkspaceSecretsError {
    fn from(_: SensitiveResponseError) -> Self {
        Self::CannotOpen
    }
}

/// The names this store will hold. An allowlist rather than a free-form name
/// keeps the write surface honest: a route that accepts any name is a generic
/// secret write, and nothing here is generic.
pub const KNOWN_SECRET_NAMES: [&str; 2] = [SECRET_STRIPE_SECRET_KEY, SECRET_STRIPE_WEBHOOK_SECRET];

/// The Stripe secret key the tenant's checkout runs against.
pub const SECRET_STRIPE_SECRET_KEY: &str = "stripe_secret_key";
/// The Stripe webhook signing secret the tenant's webhook verifies with.
pub const SECRET_STRIPE_WEBHOOK_SECRET: &str = "stripe_webhook_secret";

#[derive(Clone)]
pub struct WorkspaceSecretsRepository {
    pool: PgPool,
    key: SensitiveResponseKey,
    previous_key: Option<SensitiveResponseKey>,
}

impl WorkspaceSecretsRepository {
    #[must_use]
    pub fn new(
        pool: PgPool,
        key: SensitiveResponseKey,
        previous_key: Option<SensitiveResponseKey>,
    ) -> Self {
        Self {
            pool,
            key,
            previous_key,
        }
    }

    /// The associated data a secret is sealed under. The name is
    /// length-prefixed so `"a" || "bc"` and `"ab" || "c"` cannot collide.
    fn associated_data(workspace_id: Uuid, name: &str) -> Vec<u8> {
        let mut data =
            Vec::with_capacity(ASSOCIATED_DATA_DOMAIN.len() + 16 + size_of::<u64>() + name.len());
        data.extend_from_slice(ASSOCIATED_DATA_DOMAIN);
        data.extend_from_slice(workspace_id.as_bytes());
        data.extend_from_slice(&(name.len() as u64).to_be_bytes());
        data.extend_from_slice(name.as_bytes());
        data
    }

    /// Seals and stores one secret. A rewrite is a new ciphertext under a
    /// fresh nonce; `masked_hint` is the safe-to-show remainder.
    ///
    /// # Errors
    ///
    /// Propagates database and sealing failures. The plaintext is consumed
    /// and zeroized either way.
    pub async fn set(
        &self,
        workspace_id: Uuid,
        name: &str,
        mut plaintext: Vec<u8>,
        masked_hint: &str,
    ) -> Result<MaskedSecret, WorkspaceSecretsError> {
        let associated_data = Self::associated_data(workspace_id, name);
        let sealed = encrypt_value(&plaintext, &self.key, &associated_data);
        plaintext.zeroize();
        let ciphertext = sealed?;
        let row: (OffsetDateTime,) = sqlx::query_as(
            r#"
            INSERT INTO workspace_secrets (workspace_id, name, ciphertext, masked_hint)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (workspace_id, name) DO UPDATE SET
                ciphertext = EXCLUDED.ciphertext,
                masked_hint = EXCLUDED.masked_hint,
                updated_at = now()
            RETURNING updated_at
            "#,
        )
        .bind(workspace_id)
        .bind(name)
        .bind(ciphertext)
        .bind(masked_hint)
        .fetch_one(&self.pool)
        .await?;
        Ok(MaskedSecret {
            name: name.to_owned(),
            masked_hint: masked_hint.to_owned(),
            updated_at: row.0,
        })
    }

    /// Opens one secret. `None` means the tenant never set it; a row that
    /// exists but opens under neither key is `CannotOpen`, not `None` — a
    /// corrupted or re-keyed row is a different failure than an absent one.
    ///
    /// # Errors
    ///
    /// Propagates database failures and reports `CannotOpen` when the row
    /// exists but opens under neither configured key.
    pub async fn reveal(
        &self,
        workspace_id: Uuid,
        name: &str,
    ) -> Result<Option<Vec<u8>>, WorkspaceSecretsError> {
        let stored: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT ciphertext FROM workspace_secrets WHERE workspace_id = $1 AND name = $2",
        )
        .bind(workspace_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        let Some(ciphertext) = stored else {
            return Ok(None);
        };
        let associated_data = Self::associated_data(workspace_id, name);
        if let Ok(mut plaintext) = decrypt_value(&ciphertext, &self.key, &associated_data) {
            return Ok(Some(std::mem::take(&mut plaintext)));
        }
        let plaintext = self
            .previous_key
            .as_ref()
            .and_then(|key| decrypt_value(&ciphertext, key, &associated_data).ok())
            .ok_or(WorkspaceSecretsError::CannotOpen)?;
        Ok(Some(plaintext))
    }

    /// The masked inventory for the operator panel. Never reads ciphertext.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn list_masked(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<MaskedSecret>, WorkspaceSecretsError> {
        let rows: Vec<(String, String, OffsetDateTime)> = sqlx::query_as(
            r#"
            SELECT name, masked_hint, updated_at
            FROM workspace_secrets
            WHERE workspace_id = $1
            ORDER BY name
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, masked_hint, updated_at)| MaskedSecret {
                name,
                masked_hint,
                updated_at,
            })
            .collect())
    }

    /// Removes one secret. Deleting a row that never existed succeeds — the
    /// operator asked for "not set" and "not set" is where it lands.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn delete(
        &self,
        workspace_id: Uuid,
        name: &str,
    ) -> Result<bool, WorkspaceSecretsError> {
        let result =
            sqlx::query("DELETE FROM workspace_secrets WHERE workspace_id = $1 AND name = $2")
                .bind(workspace_id)
                .bind(name)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// The masked hint a Stripe-shaped value leaves: the mode prefix plus the
/// last four characters, e.g. `sk_live_…wxyz`. The prefix is operational
/// information (live vs test, secret vs restricted, webhook vs key) and the
/// suffix is how an operator confirms which key was pasted; the middle is
/// the secret and it never leaves.
pub fn stripe_masked_hint(value: &str) -> String {
    let value = value.trim();
    // `sk_live_x` and `whsec_x` want different cuts: the mode segment is the
    // useful part of a key prefix, while a webhook secret's only segment is
    // `whsec_` itself. Take through the second underscore when one exists,
    // through the first otherwise.
    let mut underscores = value.match_indices('_').map(|(index, _)| index);
    let first = underscores.next();
    let prefix_len = underscores
        .next()
        .map(|second| second + 1)
        .or_else(|| first.map(|index| index + 1))
        .unwrap_or(8)
        .min(12);
    // The hint must always leave a hidden middle: a value short enough that
    // prefix + suffix would reveal all but a sliver degrades to the scheme
    // alone — `sk_live_…` still says which kind was stored.
    if value.len() <= prefix_len + 8 {
        let prefix: String = value.chars().take(prefix_len).collect();
        return format!("{prefix}…");
    }
    let prefix: String = value.chars().take(prefix_len).collect();
    let suffix: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_hint_keeps_prefix_and_last_four() {
        assert_eq!(
            stripe_masked_hint("sk_live_f4ke-k3y-n0t-r34l"),
            "sk_live_…r34l"
        );
        assert_eq!(stripe_masked_hint("whsec_f4ke-s3cret9END"), "whsec_…9END");
    }

    #[test]
    fn masked_hint_bounds_short_values() {
        let hint = stripe_masked_hint("sk_x");
        assert!(hint.len() <= 20);
        // A value too short for prefix + suffix to both be partial degrades
        // to the scheme alone — the hint never carries the whole secret.
        assert_eq!(stripe_masked_hint("whsec_ab"), "whsec_…");
        assert_eq!(stripe_masked_hint("sk_live_"), "sk_live_…");
        assert_eq!(stripe_masked_hint("whsec_12345678"), "whsec_…");
        assert_eq!(stripe_masked_hint("whsec_1234567890123"), "whsec_…0123");
    }

    #[test]
    fn associated_data_binds_workspace_and_name() {
        // Fixed u128s rather than new_v4 — infra's uuid feature set does not
        // include v4 when the lib test builds alone, and distinct constants
        // are the more deterministic binding check anyway.
        let workspace = Uuid::from_u128(0x05ec_4e75_57ac_4f3c_9a1d_2b8e_6f01_a2c3);
        let first = WorkspaceSecretsRepository::associated_data(workspace, "stripe_secret_key");
        let second = WorkspaceSecretsRepository::associated_data(workspace, "stripe_secret_key");
        assert_eq!(first, second);
        assert_ne!(
            first,
            WorkspaceSecretsRepository::associated_data(workspace, "stripe_webhook_secret")
        );
        assert_ne!(
            first,
            WorkspaceSecretsRepository::associated_data(
                Uuid::from_u128(0x0f7e6d5c_4b3a_2198_7654_3210fedcba98),
                "stripe_secret_key",
            )
        );
    }
}
