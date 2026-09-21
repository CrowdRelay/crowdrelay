-- Give play steps a real in-process delivery leg.
--
-- `play.step` is the capability behind every play step, and until now no
-- executor advertised it: the action ledger row and the signed outbox intent
-- were written and then the action parked forever. Fan-facing steps now
-- write `fan_push_deliveries` rows for the fan's active endpoints inside the
-- same transaction as the recipient ledger and the emit, so the push worker
-- delivers the ask without needing n8n to be listening.
--
-- `viryaos_play_steps.result` is where a step with no audience — a listing
-- sweep, a pre-save surface check — records what it found. The emit stays
-- the audit; the column is the durable per-step fact the operator surfaces
-- can read back. NULL means the step ran and reported nothing, not that the
-- check did not happen.
--
-- Idempotent: the constraint drop+add is safe to re-run; the column add is
-- guarded.

ALTER TABLE fan_push_deliveries
    DROP CONSTRAINT IF EXISTS fan_push_deliveries_source_kind_check;

ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_source_kind_check CHECK (
        source_kind IN (
            'nearby_concert',
            'communication_campaign',
            'show_checklist',
            'beacon_nearby_concert',
            'agent_signal_push',
            'event_announcement',
            'play_step'
        )
    );

ALTER TABLE viryaos_play_steps
    ADD COLUMN IF NOT EXISTS result jsonb;
