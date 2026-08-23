-- Predicted show cost against settled show cost.
--
-- Phase 7 built a model good enough to refuse a gig with, and nothing ever
-- checked it. A rate that was wrong stayed wrong and kept deciding shows on a
-- number nobody had tested.
--
-- One row per event, filled in two steps that must happen in that order:
--
-- 1. the **prediction** is frozen while the show is still ahead, with the whole
--    tour policy it was computed from. Recomputing an estimate at settlement
--    time would score today's model against itself, which always passes.
-- 2. the **settlement** records what actually left and arrived, and the
--    variance is derived in the same transaction.
--
-- A settlement without a prediction is refused rather than backfilled. There is
-- no honest way to score a model against a show it was never asked about, and
-- the row that says so is more useful than a number that was invented.
--
-- Every verdict constraint uses `IS NOT DISTINCT FROM`. A CHECK whose
-- expression evaluates to NULL passes, so on a row that is still only a
-- prediction a plain `=` would wave through exactly the shapes below that are
-- meant to be impossible.

CREATE TABLE viryaos_show_cost_ledger (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL,
    event_id uuid NOT NULL,

    -- ---- the prediction, frozen ----
    predicted_at timestamptz NOT NULL,
    -- The rates the estimate came from, kept whole. Without it an operator
    -- reading a variance a year later cannot tell whether the model was wrong
    -- or merely old.
    tour_policy_snapshot jsonb NOT NULL
        CHECK (jsonb_typeof(tour_policy_snapshot) = 'object'),
    -- The logistics the estimate was computed against, likewise frozen.
    distance_km integer CHECK (distance_km IS NULL OR distance_km >= 0),
    offered_fee_minor bigint NOT NULL,
    application_fee_minor bigint NOT NULL DEFAULT 0,
    -- Null on every line when the estimate itself was an honest refusal; the
    -- missing input is named instead.
    predicted_transport_minor bigint,
    predicted_accommodation_minor bigint,
    predicted_per_diem_minor bigint,
    predicted_overhead_minor bigint,
    predicted_total_cost_minor bigint,
    predicted_net_margin_minor bigint,
    predicted_round_trip_km integer,
    prediction_missing_input text CHECK (prediction_missing_input IS NULL OR prediction_missing_input IN (
        'distance_km', 'transport_minor_per_100km_round_trip', 'vehicle_capacity',
        'crew_size', 'accommodation_minor_per_room_night'
    )),
    -- A complete estimate has every line; an incomplete one has none and names
    -- what was missing. Half a prediction is not a prediction.
    CONSTRAINT viryaos_show_cost_ledger_prediction_is_whole
        CHECK ((prediction_missing_input IS NULL) = (predicted_total_cost_minor IS NOT NULL)),

    -- ---- the settlement, reported ----
    settled_at timestamptz,
    -- Who said so. A settlement is somebody's account of what happened, and an
    -- unattributed one cannot be questioned.
    settled_by text CHECK (settled_by IS NULL OR (btrim(settled_by) <> '' AND char_length(settled_by) <= 120)),
    settled_transport_minor bigint,
    settled_accommodation_minor bigint,
    settled_per_diem_minor bigint,
    settled_overhead_minor bigint,
    -- Money the model has no line for. Kept separate rather than folded into
    -- overhead: when this is the largest miss, the finding is that the model is
    -- missing a cost, not that a rate is off.
    settled_other_minor bigint,
    fee_received_minor bigint,
    settled_total_cost_minor bigint,
    settled_net_margin_minor bigint,

    -- ---- the derived verdict ----
    accuracy text CHECK (accuracy IS NULL OR accuracy IN ('calibrated', 'drifting', 'insufficient')),
    accuracy_reason text CHECK (accuracy_reason IS NULL OR accuracy_reason IN (
        'no_prediction', 'prediction_incomplete', 'no_settlement'
    )),
    total_variance_basis_points integer,
    worst_line text CHECK (worst_line IS NULL OR worst_line IN (
        'transport', 'accommodation', 'per_diem', 'overhead', 'unmodelled', 'fee'
    )),
    worst_line_delta_minor bigint,
    -- What this show says the road actually costs. Evidence for an operator to
    -- act on, never applied: a rate is the band's declaration about their own
    -- van, and one show is a data point rather than a policy.
    implied_transport_rate_minor_per_100km bigint,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    UNIQUE (workspace_id, id),
    -- One ledger row per show. A second would let the same gig be scored twice
    -- with two different answers.
    UNIQUE (workspace_id, event_id),
    CONSTRAINT viryaos_show_cost_ledger_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,

    -- A settlement arrives whole or not at all.
    CONSTRAINT viryaos_show_cost_ledger_settlement_is_whole
        CHECK ((settled_at IS NULL) = (settled_total_cost_minor IS NULL)),
    -- An insufficient verdict names its reason, and only an insufficient one
    -- has one.
    CONSTRAINT viryaos_show_cost_ledger_reason_matches_accuracy
        CHECK ((accuracy IS NOT DISTINCT FROM 'insufficient') = (accuracy_reason IS NOT NULL)),
    -- A named worst line requires a drifting verdict. Nothing else may point an
    -- operator at a rate to change.
    CONSTRAINT viryaos_show_cost_ledger_worst_line_requires_drift
        CHECK (worst_line IS NULL OR accuracy IS NOT DISTINCT FROM 'drifting'),
    CONSTRAINT viryaos_show_cost_ledger_delta_requires_worst_line
        CHECK (worst_line_delta_minor IS NULL OR worst_line IS NOT NULL),
    -- A verdict exists exactly when a settlement does.
    CONSTRAINT viryaos_show_cost_ledger_verdict_follows_settlement
        CHECK ((accuracy IS NOT NULL) = (settled_at IS NOT NULL))
);

CREATE TRIGGER viryaos_show_cost_ledger_set_updated_at
BEFORE UPDATE ON viryaos_show_cost_ledger
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The operator read: shows awaiting a settlement, newest prediction first.
CREATE INDEX viryaos_show_cost_ledger_open_idx
    ON viryaos_show_cost_ledger (workspace_id, predicted_at DESC)
    WHERE settled_at IS NULL;

CREATE INDEX viryaos_show_cost_ledger_drift_idx
    ON viryaos_show_cost_ledger (workspace_id, settled_at DESC)
    WHERE accuracy = 'drifting';

-- The tolerance the verdict uses, kept with the rates it judges. An operator
-- who disagrees that a show drifted is already looking at this table.
ALTER TABLE viryaos_tour_economics
    ADD COLUMN settlement_policy jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(settlement_policy) = 'object');
