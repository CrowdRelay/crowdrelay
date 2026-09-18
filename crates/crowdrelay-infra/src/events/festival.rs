// The festival edition writer, split out of `events.rs` to keep that file
// under the modularity contract's 1000-line parent cap. Included rather than
// declared as a module: it is one `impl PostgresEventRepository` block, and a
// module would need the whole struct's private surface re-exported to reach
// the same pool.

impl PostgresEventRepository {
    async fn set_event_festival_inner(
        &self,
        command: &SetEventFestivalCommand,
    ) -> Result<(), EventStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(EventStoreError::from_sqlx)?;
        let workspace_id =
            trusted_workspace_id_in_transaction(&mut transaction, &self.workspace_slug).await?;
        if workspace_id != command.workspace_id {
            return Err(EventStoreError::NotFound);
        }

        // Clearing the mark while a festival-scale bill sits on the event
        // would leave stored acts that `PublicEvent::validate` refuses on
        // the way back out — the bill must shrink to club size first.
        if command.festival_name.is_none() {
            let billed = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT count(*) FROM event_acts AS act
                JOIN events AS event
                  ON event.workspace_id = act.workspace_id
                 AND event.id = act.event_id
                WHERE event.workspace_id = $1 AND event.slug = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.event_slug.as_str())
            .fetch_one(&mut *transaction)
            .await
            .map_err(EventStoreError::from_sqlx)?;
            if billed > MAX_EVENT_ACTS_PER_EVENT as i64 {
                return Err(EventStoreError::Conflict);
            }
        }

        let updated = sqlx::query(
            r#"
            UPDATE events
            SET festival_name = $3,
                updated_at = now()
            WHERE workspace_id = $1
                AND slug = $2
                -- Same status rule as the counterparty: a slot's festival
                -- identity is often only confirmed around show day, so
                -- 'completed' stays editable; 'cancelled' is history.
                AND status IN ('draft', 'published', 'completed')
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.event_slug.as_str())
        .bind(command.festival_name.as_deref())
        .execute(&mut *transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?;
        if updated.rows_affected() == 0 {
            return Err(EventStoreError::NotFound);
        }

        transaction
            .commit()
            .await
            .map_err(EventStoreError::from_sqlx)
    }
}
