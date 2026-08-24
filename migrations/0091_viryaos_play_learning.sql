-- What the agent has learned about its own plays.
--
-- Everything before this decides from the present: a series moved, a step is
-- due, a pipeline is empty. Nothing carried a memory of whether a kind of
-- campaign has ever worked, so a play that measured `worsened` three times
-- running was proposed exactly as often as one that measured `improved`.
--
-- Counts, not a score. A score cannot be argued with; an operator who disagrees
-- with a standing can read the outcomes that produced it and say which one is
-- wrong. `insufficient_count` is stored and deliberately absent from every
-- calculation: an outcome nobody could measure is not an outcome that failed,
-- and letting it count would retire the plays the agent cannot see rather than
-- the ones that do not work.
--
-- Nothing here is authority. The weight scales one number — how many recipients
-- a step of this play may reach — and only downward. The context ladder, the
-- class ceiling and the growth envelope are untouched by this table and move
-- only when a human moves them, however good a record looks.

CREATE TABLE viryaos_play_learning (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    play_kind text NOT NULL CHECK (play_kind IN ('track_us_ask')),

    improved_count integer NOT NULL DEFAULT 0 CHECK (improved_count >= 0),
    neutral_count integer NOT NULL DEFAULT 0 CHECK (neutral_count >= 0),
    worsened_count integer NOT NULL DEFAULT 0 CHECK (worsened_count >= 0),
    insufficient_count integer NOT NULL DEFAULT 0 CHECK (insufficient_count >= 0),
    -- Measured `worsened` outcomes since the last one that was not. Reset by any
    -- measured result that is not `worsened`; untouched by an unmeasurable one,
    -- which neither breaks a run of failures nor extends it.
    consecutive_worsened integer NOT NULL DEFAULT 0 CHECK (consecutive_worsened >= 0),

    -- Derived from the counts above and stored so the read model does not have
    -- to recompute a judgement. `10000` is the operator's full configured reach.
    weight_basis_points integer NOT NULL DEFAULT 10000
        CHECK (weight_basis_points BETWEEN 0 AND 10000),
    retired_at timestamptz,
    -- `operator_retired` is the only value the agent never writes. Keeping the
    -- two reasons apart stops the agent presenting a human's decision as its
    -- own conclusion.
    retired_reason text CHECK (retired_reason IS NULL OR retired_reason IN (
        'repeatedly_worsened', 'operator_retired'
    )),

    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (workspace_id, play_kind),
    -- A retirement is a stated fact with a reason, or it is not a retirement.
    CONSTRAINT viryaos_play_learning_retirement_is_stated
        CHECK ((retired_at IS NULL) = (retired_reason IS NULL)),
    -- A retired play reaches nobody, and a running one reaches somebody. A zero
    -- weight without a retirement would be a silent stop that no read model
    -- could explain.
    CONSTRAINT viryaos_play_learning_weight_matches_retirement
        CHECK ((weight_basis_points = 0) = (retired_at IS NOT NULL))
);

CREATE TRIGGER viryaos_play_learning_set_updated_at
BEFORE UPDATE ON viryaos_play_learning
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();
