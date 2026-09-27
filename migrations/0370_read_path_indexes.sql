-- Indexes for read paths that had none.
--
-- Found by planning every row-returning query in the workspace with
-- `EXPLAIN (GENERIC_PLAN)` under `enable_seqscan = off`, then checking each
-- scan's equality predicates against the leading columns of the relation's
-- indexes (PostgreSQL 18+ skip scan counts: an index whose leading column is
-- a low-cardinality one still serves a lookup on the next). What remained is
-- below, each with the query it serves. Local tables are too small for
-- timings to mean anything; what these fix is a scan that grows with the
-- table instead of with the answer.
--
-- Foreign keys whose parent the code deletes are included where the child
-- column had no index: every parent delete (retention sweeps above all)
-- otherwise scans the whole child table once per deleted row to check the
-- RESTRICT or run the CASCADE.

-- The outreach snapshot the evaluator reads every cycle asks, per active
-- opportunity, for its last outbound, its follow-up count and its last reply
-- (`autopilot/operations/snapshots.rs`, OUTREACH_SNAPSHOT_SQL). The only index
-- led (workspace_id, target_id), so each opportunity scanned the workspace's
-- whole interaction history.
CREATE INDEX IF NOT EXISTS outreach_interactions_opportunity_idx
    ON outreach_interactions (workspace_id, opportunity_id, direction, occurred_at DESC)
    WHERE opportunity_id IS NOT NULL;

-- Everything the machine did about one subject — a show, an opportunity, a
-- negotiation — whatever its status (show timeline facts, show ladder,
-- negotiations, the snapshot's in-flight probe). The existing subject indexes
-- are partial on succeeded or on outward classes.
CREATE INDEX IF NOT EXISTS autopilot_actions_subject_idx
    ON autopilot_actions (workspace_id, subject_id, created_at DESC);

-- One contact's letters in the outreach drawer (`autopilot/outreach_contacts.rs`):
-- the target lives in the payload, not a column.
CREATE INDEX IF NOT EXISTS autopilot_actions_outreach_target_idx
    ON autopilot_actions (workspace_id, (payload ->> 'target_id'), created_at DESC)
    WHERE context = 'outreach';

-- The newest cycle and the newest decision, per workspace. `ops/summary`
-- reports the fleet-wide ages (it walks workspaces and takes one probe each),
-- and the scorecard and learning loop read one workspace's newest first.
-- Every index on both tables led with workspace_id and something else, so
-- `max(finished_at)` / `max(evaluated_at)` read the tables end to end. A
-- global index on the timestamp alone was tried and rejected: the planner
-- then walks it for per-workspace reads, stepping over every other
-- workspace's rows.
CREATE INDEX IF NOT EXISTS autopilot_cycle_runs_finished_idx
    ON autopilot_cycle_runs (workspace_id, finished_at DESC)
    WHERE finished_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS autopilot_decisions_evaluated_idx
    ON autopilot_decisions (workspace_id, evaluated_at DESC);

-- A show's passes (door report, fan context, control-plane show facts).
-- Also the child side of events → admission_passes.
CREATE INDEX IF NOT EXISTS admission_passes_event_idx
    ON admission_passes (workspace_id, event_id, status);

-- Posts made for one community target (process runs, measurement readiness).
CREATE INDEX IF NOT EXISTS community_posts_target_idx
    ON community_posts (workspace_id, target_id)
    WHERE target_id IS NOT NULL;

-- A run found by its token on the public reward path.
CREATE INDEX IF NOT EXISTS synesthesia_runs_token_idx
    ON synesthesia_runs (workspace_id, run_token_hash);

-- The consultant's rescan rate bound: was one requested for this template
-- lately. The unique index covers only pending (unconsumed) rows.
CREATE INDEX IF NOT EXISTS agent_template_rescan_requests_recent_idx
    ON agent_template_rescan_requests (workspace_id, template_id, created_at DESC);

-- City resolution by typed name (`beacon_seed`, `gdrive`, `peer_act_seed`,
-- `venue_seed`): `slug = lower(btrim($1)) OR lower(btrim(name)) = lower(btrim($1))`.
-- The slug half had an index; the name half did not.
CREATE INDEX IF NOT EXISTS cities_name_lookup_idx
    ON cities (lower(btrim(name)));

-- Push retention deletes terminal deliveries oldest first by
-- COALESCE(completed_at, updated_at) (`retention/steps.rs`); without this
-- every batch sorted the whole terminal set.
CREATE INDEX IF NOT EXISTS fan_push_deliveries_terminal_retention_idx
    ON fan_push_deliveries ((COALESCE(completed_at, updated_at)), id)
    WHERE status IN ('delivered', 'failed', 'ambiguous');

-- Foreign keys whose parent is deleted by code, child side unindexed.
-- outbox_events is swept by retention in batches, and the sweep's own
-- NOT EXISTS probes read these same columns.
CREATE INDEX IF NOT EXISTS autopilot_action_emissions_outbox_idx
    ON autopilot_action_emissions (workspace_id, outbox_event_id)
    WHERE outbox_event_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS show_notification_emissions_outbox_idx
    ON show_notification_emissions (workspace_id, outbox_event_id);
CREATE INDEX IF NOT EXISTS communication_campaigns_dispatch_event_idx
    ON communication_campaigns (workspace_id, dispatch_event_id)
    WHERE dispatch_event_id IS NOT NULL;
-- fan_push_endpoints retention cascades into deliveries, the largest push table.
CREATE INDEX IF NOT EXISTS fan_push_deliveries_endpoint_idx
    ON fan_push_deliveries (workspace_id, endpoint_id);
CREATE INDEX IF NOT EXISTS inventory_ledger_reservation_idx
    ON inventory_ledger (workspace_id, reservation_id)
    WHERE reservation_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS synesthesia_reward_entries_run_idx
    ON synesthesia_reward_entries (workspace_id, run_id);
