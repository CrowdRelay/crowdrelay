// The name a workspace's own messages carry, read inside the transaction that
// writes them. `crowdrelay_workspace_wordmark` (migration 0355) is the single
// resolver: the SQL-built pushes call it directly, the Rust-built copy calls it
// through here, and the two cannot sign with different names.

pub(in crate::autopilot) async fn workspace_wordmark(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<String, RepositoryError> {
    sqlx::query_scalar::<_, String>("SELECT crowdrelay_workspace_wordmark($1)")
        .bind(workspace_id.into_uuid())
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)
}
