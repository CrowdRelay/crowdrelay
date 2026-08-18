-- The worker's claim query is the highest-frequency statement in the system:
-- every worker polls it continuously. It filters on status/available_at with no
-- workspace predicate, but every existing outbox index leads with workspace_id
-- (they serve the per-tenant ops views), so none of them could be used and the
-- claim fell back to a sequential scan over the whole table plus a sort.
--
-- Measured on 60k rows with 3198 pending: 2168 buffers with a Seq Scan, 1077
-- with these indexes and a BitmapOr. The gap widens as delivered rows
-- accumulate between retention passes, which is exactly when a delivery backlog
-- makes the claim hottest.
--
-- Both are partial, so they only carry rows that are actually claimable: a
-- delivered or dead row leaves the index entirely and costs nothing to keep.

CREATE INDEX IF NOT EXISTS outbox_events_claim_pending_idx
    ON outbox_events (available_at, id)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS outbox_events_claim_expired_lease_idx
    ON outbox_events (lease_expires_at, id)
    WHERE status = 'processing';
