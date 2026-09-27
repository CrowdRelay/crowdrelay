//! Exact links the publish guard may let through beyond the tenant's own
//! origin: facts the workspace's records already publish.

use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;

/// Ticket links on the workspace's published shows that have not happened
/// yet (with a day's grace for a post about tonight). Exact URLs only — the
/// guard never approves a whole ticketing host, because a model could put
/// any path on it.
pub(crate) async fn show_ticket_links(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT btrim(ticket_url)
        FROM events
        WHERE workspace_id = $1
          AND status = 'published'
          AND ticket_url IS NOT NULL
          AND btrim(ticket_url) <> ''
          AND starts_at > now() - INTERVAL '1 day'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
}
