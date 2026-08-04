-- Drop an index created ahead of a retention query that was never added.
--
-- Cancelled webhook deliveries are already reclaimed via cascade delete when
-- their parent outbox_events row ages out (see delete_old_terminal_outbox_events
-- in crowdrelay-worker). Deleting webhook_deliveries rows directly, independent
-- of their parent outbox event, is intentionally avoided elsewhere in the
-- codebase because it can race with outbox materialization idempotency while
-- the parent event is still retained. The index therefore has no query to
-- serve and only adds write overhead to every webhook_deliveries mutation.

DROP INDEX IF EXISTS webhook_deliveries_cancelled_retention_idx;
