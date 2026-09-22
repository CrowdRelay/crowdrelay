//! Workspace secrets against a real database: the seal/open round trip, the
//! tenant boundary, key rotation, and the masked list. These run against a
//! disposable database — `CROWDRELAY_EVENT_TEST_DATABASE_URL`, the same env
//! the events suite uses.

use crowdrelay_domain::{WorkspaceId, WorkspaceSlug};
use crowdrelay_infra::{
    sensitive_response::SensitiveResponseKey,
    workspace_secrets::{
        KEY_DERIVATION_DOMAIN, SECRET_STRIPE_SECRET_KEY, WorkspaceSecretsRepository,
        stripe_masked_hint,
    },
};
use sqlx::{PgPool, postgres::PgPoolOptions};

const SECRET: &[u8] = b"workspace-secrets-test-secret-0001";

fn repository(pool: &PgPool) -> WorkspaceSecretsRepository {
    WorkspaceSecretsRepository::new(
        pool.clone(),
        SensitiveResponseKey::derive_for_domain(KEY_DERIVATION_DOMAIN, SECRET),
        None,
    )
}

async fn seed_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    slug: &WorkspaceSlug,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(slug.as_str())
        .bind("Secrets E2E")
        .execute(pool)
        .await?;
    Ok(())
}

async fn fresh_pool() -> Result<PgPool, Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_EVENT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_EVENT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok(pool)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn seals_reveals_lists_and_deletes() -> Result<(), Box<dyn std::error::Error>> {
    let pool = fresh_pool().await?;
    let workspace_id = WorkspaceId::new();
    let slug = WorkspaceSlug::parse(format!("secrets-{}", workspace_id.into_uuid().simple()))?;
    seed_workspace(&pool, workspace_id, &slug).await?;
    let repo = repository(&pool);

    assert!(
        repo.reveal(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
            .await?
            .is_none()
    );

    let plaintext = "sk_live_f4ke-k3y-n0t-r34l".as_bytes().to_vec();
    let hint = stripe_masked_hint("sk_live_f4ke-k3y-n0t-r34l");
    let stored = repo
        .set(
            workspace_id.into_uuid(),
            SECRET_STRIPE_SECRET_KEY,
            plaintext.clone(),
            &hint,
        )
        .await?;
    assert_eq!(stored.masked_hint, "sk_live_…r34l");

    // The row holds ciphertext, not the key — read the column raw to prove it.
    let raw: Vec<u8> = sqlx::query_scalar(
        "SELECT ciphertext FROM workspace_secrets WHERE workspace_id = $1 AND name = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(SECRET_STRIPE_SECRET_KEY)
    .fetch_one(&pool)
    .await?;
    assert!(
        !raw.windows(plaintext.len())
            .any(|window| window == plaintext.as_slice()),
        "ciphertext must not contain the plaintext"
    );

    let revealed = repo
        .reveal(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
        .await?
        .ok_or("stored secret must reveal")?;
    assert_eq!(revealed, plaintext);

    let masked = repo.list_masked(workspace_id.into_uuid()).await?;
    assert_eq!(masked.len(), 1);
    assert_eq!(masked[0].name, SECRET_STRIPE_SECRET_KEY);
    assert_eq!(masked[0].masked_hint, "sk_live_…r34l");
    let serialized = serde_json::to_string(&masked[0].masked_hint)?;
    assert!(!serialized.contains("4eC39HqLyjWDarjtT1zdp"));

    assert!(
        repo.delete(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
            .await?
    );
    assert!(
        repo.reveal(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
            .await?
            .is_none()
    );
    // Deleting an absent name is a no-op, not an error.
    assert!(
        !repo
            .delete(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
            .await?
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn secrets_do_not_cross_workspaces() -> Result<(), Box<dyn std::error::Error>> {
    let pool = fresh_pool().await?;
    let repo = repository(&pool);
    let first = WorkspaceId::new();
    let second = WorkspaceId::new();
    for (workspace_id, name) in [(first, "secrets-a"), (second, "secrets-b")] {
        let slug = WorkspaceSlug::parse(format!("{}-{}", name, workspace_id.into_uuid().simple()))?;
        seed_workspace(&pool, workspace_id, &slug).await?;
    }

    repo.set(
        first.into_uuid(),
        SECRET_STRIPE_SECRET_KEY,
        b"sk_test_firstworkspace".to_vec(),
        "sk_test_…pace",
    )
    .await?;

    // The other workspace sees neither the value nor the row.
    assert!(
        repo.reveal(second.into_uuid(), SECRET_STRIPE_SECRET_KEY)
            .await?
            .is_none()
    );
    assert!(repo.list_masked(second.into_uuid()).await?.is_empty());

    // And the ciphertext is bound to the workspace it was sealed under — a row
    // copied across workspaces cannot be opened.
    sqlx::query(
        "INSERT INTO workspace_secrets (workspace_id, name, ciphertext, masked_hint) \
         SELECT $2, name, ciphertext, masked_hint FROM workspace_secrets \
         WHERE workspace_id = $1",
    )
    .bind(first.into_uuid())
    .bind(second.into_uuid())
    .execute(&pool)
    .await?;
    assert!(
        repo.reveal(second.into_uuid(), SECRET_STRIPE_SECRET_KEY)
            .await
            .is_err(),
        "a row copied to another workspace must not open"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn previous_key_opens_rows_the_outgoing_key_wrote() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = fresh_pool().await?;
    let workspace_id = WorkspaceId::new();
    let slug = WorkspaceSlug::parse(format!("secrets-{}", workspace_id.into_uuid().simple()))?;
    seed_workspace(&pool, workspace_id, &slug).await?;

    // The outgoing key writes; the rotated deployment opens with previous set.
    let outgoing = WorkspaceSecretsRepository::new(
        pool.clone(),
        SensitiveResponseKey::derive_for_domain(KEY_DERIVATION_DOMAIN, b"outgoing-secret"),
        None,
    );
    outgoing
        .set(
            workspace_id.into_uuid(),
            SECRET_STRIPE_SECRET_KEY,
            b"sk_test_rotation".to_vec(),
            "sk_test_…tion",
        )
        .await?;

    let rotated = WorkspaceSecretsRepository::new(
        pool.clone(),
        SensitiveResponseKey::derive_for_domain(KEY_DERIVATION_DOMAIN, b"current-secret"),
        Some(SensitiveResponseKey::derive_for_domain(
            KEY_DERIVATION_DOMAIN,
            b"outgoing-secret",
        )),
    );
    let revealed = rotated
        .reveal(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
        .await?
        .ok_or("previous key must open the row it wrote")?;
    assert_eq!(revealed, b"sk_test_rotation".to_vec());

    // A repo without the previous key cannot open it — the boundary is real.
    let no_previous = WorkspaceSecretsRepository::new(
        pool.clone(),
        SensitiveResponseKey::derive_for_domain(KEY_DERIVATION_DOMAIN, b"current-secret"),
        None,
    );
    assert!(
        no_previous
            .reveal(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn rewrite_replaces_the_value_and_refreshes_the_mask()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = fresh_pool().await?;
    let workspace_id = WorkspaceId::new();
    let slug = WorkspaceSlug::parse(format!("secrets-{}", workspace_id.into_uuid().simple()))?;
    seed_workspace(&pool, workspace_id, &slug).await?;
    let repo = repository(&pool);

    repo.set(
        workspace_id.into_uuid(),
        SECRET_STRIPE_SECRET_KEY,
        b"sk_test_firstkey0000".to_vec(),
        "sk_test_…0000",
    )
    .await?;
    repo.set(
        workspace_id.into_uuid(),
        SECRET_STRIPE_SECRET_KEY,
        b"sk_live_secondkey99".to_vec(),
        "sk_live_…ey99",
    )
    .await?;

    let revealed = repo
        .reveal(workspace_id.into_uuid(), SECRET_STRIPE_SECRET_KEY)
        .await?
        .ok_or("rewritten secret must reveal")?;
    assert_eq!(revealed, b"sk_live_secondkey99".to_vec());
    let masked = repo.list_masked(workspace_id.into_uuid()).await?;
    assert_eq!(masked.len(), 1);
    assert_eq!(masked[0].masked_hint, "sk_live_…ey99");
    Ok(())
}
