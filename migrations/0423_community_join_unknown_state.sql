-- A community join has a non-transactional provider side effect.
--
-- joining means only that CrowdRelay claimed local work; no external request
-- has been intentionally released yet. join_dispatched is stamped immediately
-- before the HTTP call and means the provider may have observed the operation.
-- Losing the response from that point is UNKNOWN, never failure/retry:
-- retrying could duplicate an invite redemption or otherwise mutate provider
-- state twice. join_unknown is terminal for the automatic claimant until a
-- read-only reconciliation or operator evidence resolves it.
ALTER TABLE discovery_places
    DROP CONSTRAINT IF EXISTS discovery_places_membership_state_check;

ALTER TABLE discovery_places
    ADD CONSTRAINT discovery_places_membership_state_check
    CHECK (membership_state IN (
        'not_joined',
        'joining',
        'join_dispatched',
        'join_unknown',
        'joined',
        'rejected',
        'not_a_fit'
    ));

COMMENT ON COLUMN discovery_places.membership_state IS
    'Community relationship truth: not_joined; joining (local claim only); join_dispatched (external side effect may be in flight); join_unknown (response lost/ambiguous, do not retry); joined (provider-confirmed); rejected (provider refused); not_a_fit (we declined).';
