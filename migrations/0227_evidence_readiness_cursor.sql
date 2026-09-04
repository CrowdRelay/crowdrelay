-- Indexes for the two reads that now decide when the brain learns.
--
-- `resolved_at` became the delta cursor for the causal model. It used to be
-- `timestamp` (dispatch time), which meant a measurement resolving on day 14
-- carried a cursor value from day 0 — always behind a checkpoint that advances
-- every cycle, so the delta replay never saw an outcome at all. The learner now
-- orders and filters on `resolved_at`, and this index is what makes that cheap.
CREATE INDEX IF NOT EXISTS viryaos_growth_evidence_resolved_at_idx
    ON viryaos_growth_evidence (workspace_id, resolved_at)
    WHERE resolved_at IS NOT NULL;

-- Evidence readiness asks the measurement queue whether anything for this
-- action is still outstanding. It runs on every completed and every terminally
-- failed measurement, so it is on the hot path of the whole measurement spine.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_measurements_action_status_idx
    ON viryaos_autopilot_measurements (workspace_id, action_id, status);

-- Community-level outcomes and their counterfactual both read the provenance
-- ledger filtered by community and event kind over a date window.
CREATE INDEX IF NOT EXISTS fan_provenance_community_kind_idx
    ON fan_provenance_events (workspace_id, community, event_kind, occurred_at)
    WHERE community IS NOT NULL AND fan_id IS NOT NULL;

-- Conversion attribution looks up a visitor's most recent community-tagged
-- click at signup time, inside the signup transaction.
CREATE INDEX IF NOT EXISTS click_events_visitor_occurred_idx
    ON click_events (workspace_id, anonymous_visitor_id, occurred_at DESC)
    WHERE anonymous_visitor_id IS NOT NULL;
