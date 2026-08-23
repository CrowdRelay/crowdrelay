-- What a play did, and what kind of claim the number supports.
--
-- The unit of measurement is the play. Measuring one send would credit a whole
-- campaign to whichever message happened to be last, and reading the series
-- without a frozen pre-play baseline would compare a play against a number the
-- play has already moved. So the baseline is captured when the play starts, in
-- the same transaction that creates it, and never recomputed.
--
-- One row per claim, not one row per play. Two claims exist and they must never
-- collapse into each other:
--
-- * 'attributed' is first-party. Our own rows join the outcome to the action —
--   a link the play minted, a click recorded against it. Nothing mints such a
--   link today, so this claim resolves 'insufficient' with the reason
--   'no_attribution_key'. That is the point of opening it: an absent row is
--   invisible, a row that says the claim cannot be made is a fact somebody can
--   act on.
-- * 'correlational' is the success metric moving over the play's window. The
--   movement and the campaign share a period; nothing joins them, and the API
--   says so on every number it returns.
--
-- The CHECK constraints are the real content of this file. They make it
-- impossible to store a verdict without evidence, evidence of insufficiency
-- without a reason, or a reason on a claim that succeeded. A gap here is how a
-- correlation becomes a cause three read models downstream.

CREATE TABLE viryaos_play_outcomes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL,
    play_id uuid NOT NULL,
    claim text NOT NULL CHECK (claim IN ('attributed', 'correlational')),

    -- The series the play named as its own success metric when it started.
    -- Copied rather than referenced so a later measurement cannot quietly pick
    -- a friendlier one.
    success_metric_platform text NOT NULL,
    success_metric_key text NOT NULL,

    -- Frozen at play start. A null value means the series had no usable trend
    -- then, which resolves as 'no_baseline' rather than as zero: a series
    -- nobody was reading is not a series that was standing still.
    baseline_captured_at timestamptz NOT NULL,
    baseline_value bigint,
    baseline_milli_per_day bigint,

    window_start timestamptz NOT NULL,
    -- Derived from the play's last step expiry plus the settle period. A
    -- tracker count does not move the hour a message lands.
    window_end timestamptz NOT NULL CHECK (window_end > window_start),

    observed_at timestamptz,
    observed_value bigint,
    observed_milli_per_day bigint,
    -- How many fans the play actually reached. Carried so every number has a
    -- denominator: an effect over zero recipients is not a null result, it is
    -- a campaign that did not run.
    recipients_reached integer CHECK (recipients_reached IS NULL OR recipients_reached >= 0),

    evidence text CHECK (evidence IS NULL OR evidence IN ('measured', 'insufficient')),
    evidence_reason text CHECK (evidence_reason IS NULL OR evidence_reason IN (
        'no_baseline', 'no_observation', 'window_not_closed',
        'no_attribution_key', 'nothing_delivered', 'ambiguous_series'
    )),
    effect_assessment text CHECK (effect_assessment IS NULL OR effect_assessment IN (
        'improved', 'neutral', 'worsened'
    )),
    -- Null on a measured outcome whose pre-play rate was too flat to divide by.
    -- The verdict still stands on absolute movement; the percentage is withheld
    -- because inventing one is how a rounding error becomes a growth claim.
    delta_basis_points integer,

    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'processing', 'succeeded', 'failed')),
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    available_at timestamptz NOT NULL,
    started_at timestamptz,
    finished_at timestamptz,
    last_error_kind text CHECK (last_error_kind IS NULL OR char_length(last_error_kind) <= 96),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    UNIQUE (workspace_id, id),
    -- One outcome per play per claim, for ever. A second row would let the same
    -- campaign be reported twice with two different answers.
    UNIQUE (workspace_id, play_id, claim),
    CONSTRAINT viryaos_play_outcomes_play_fk
        FOREIGN KEY (workspace_id, play_id)
        REFERENCES viryaos_plays (workspace_id, id)
        ON DELETE CASCADE,

    -- An insufficient outcome names its reason, and only an insufficient
    -- outcome has one. Without both halves, "we could not tell" and "we did not
    -- look" are the same row.
    --
    -- `IS NOT DISTINCT FROM` rather than `=` throughout, and that is not style.
    -- A CHECK whose expression evaluates to NULL passes, so on an unsettled row
    -- — where `evidence` is still NULL — `evidence = 'measured'` is NULL and the
    -- constraint waves through exactly the shape it exists to forbid: a verdict
    -- with nothing behind it.
    CONSTRAINT viryaos_play_outcomes_reason_matches_evidence
        CHECK ((evidence IS NOT DISTINCT FROM 'insufficient') = (evidence_reason IS NOT NULL)),
    -- A verdict requires a measurement. This is the constraint that stops an
    -- unanswerable question from being reported as a null result.
    CONSTRAINT viryaos_play_outcomes_verdict_requires_evidence
        CHECK (effect_assessment IS NULL OR evidence IS NOT DISTINCT FROM 'measured'),
    CONSTRAINT viryaos_play_outcomes_delta_requires_verdict
        CHECK (delta_basis_points IS NULL OR effect_assessment IS NOT NULL),
    -- A settled outcome has settled. Evidence and status cannot disagree.
    CONSTRAINT viryaos_play_outcomes_settled_together
        CHECK ((status = 'succeeded') = (evidence IS NOT NULL))
);

CREATE TRIGGER viryaos_play_outcomes_set_updated_at
BEFORE UPDATE ON viryaos_play_outcomes
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The only query the worker runs: what is due now.
CREATE INDEX viryaos_play_outcomes_due_idx
    ON viryaos_play_outcomes (workspace_id, available_at)
    WHERE status IN ('pending', 'processing');

CREATE INDEX viryaos_play_outcomes_play_idx
    ON viryaos_play_outcomes (workspace_id, play_id);
