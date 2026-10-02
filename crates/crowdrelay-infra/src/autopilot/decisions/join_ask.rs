macro_rules! decision_join_ask_reads {
    () => {
        /// The join-ask snapshot for one cycle (§5).
        ///
        /// The reads live in `crate::join_ask` so the attention board and this
        /// cycle answer from one assembly rather than two that drift. Always a
        /// snapshot: a tenant who never wrote variants resolves to the
        /// unconfigured default and is reported as held, because a silently
        /// skipped context is what made every cold tenant invisible.
        async fn load_join_ask_snapshot_impl(
            &self,
            workspace_id: WorkspaceId,
        ) -> Result<crowdrelay_domain::join_ask::JoinAskSnapshot, RepositoryError> {
            self.bounded(async {
                crate::join_ask::load_join_ask_snapshot(&self.pool, workspace_id.into_uuid())
                    .await
                    .map_err(map_sqlx)
            })
            .await
        }
    };
}
