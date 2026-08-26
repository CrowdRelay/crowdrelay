-- Wave outcome settlement: fold a completed wave's reply record into the
-- per-kind learning table.
--
-- A play outcome measures a metric series against a baseline. A wave outcome
-- measures something simpler and more direct: did the targets reply, and what
-- did they say? A wave of press pitches that got five positive replies and no
-- declines is a kind that works. One that got three `do_not_contact` replies is
-- a kind that harms. One that got no replies at all is insufficient — the
-- measurement failed, not the kind.
--
-- Same lifecycle as play outcomes: pending → processing → succeeded/failed,
-- with a window that closes after the last pitch could reasonably have received
-- a reply. The settlement worker counts replies by disposition and folds the
-- verdict into viryaos_outreach_kind_learning.

CREATE TABLE viryaos_outreach_wave_outcomes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    wave_id uuid NOT NULL,
    target_kind text NOT NULL CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator',
        'support_slot', 'endorsement', 'media_patronage'
    )),

    -- The window: from when the wave was approved (pitches released) to when
    -- the last reply could reasonably arrive. 21 days matches the reply
    -- horizon the growth-debt detector already uses for stale interactions.
    window_start timestamptz NOT NULL,
    window_end timestamptz NOT NULL,

    -- How many pitches were released. A wave of 3 and a wave of 30 measure
    -- the same kind at different confidence.
    pitches_sent integer NOT NULL CHECK (pitches_sent >= 0),

    status text NOT NULL DEFAULT 'pending' CHECK (status IN (
        'pending', 'processing', 'succeeded', 'failed'
    )),
    available_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    finished_at timestamptz,

    -- The verdict, written only on success.
    observed_at timestamptz,
    positive_replies integer CHECK (positive_replies IS NULL OR positive_replies >= 0),
    declined_replies integer CHECK (declined_replies IS NULL OR declined_replies >= 0),
    do_not_contact_replies integer CHECK (do_not_contact_replies IS NULL OR do_not_contact_replies >= 0),
    total_replies integer CHECK (total_replies IS NULL OR total_replies >= 0),
    -- 'measured' or 'insufficient'. A wave with no replies is insufficient,
    -- not bad: being ignored is a reason to fix the pitch, not to retire the
    -- kind.
    evidence text CHECK (evidence IS NULL OR evidence IN ('measured', 'insufficient')),
    -- 'improved', 'neutral', 'worsened' when measured; NULL when insufficient.
    effect_assessment text CHECK (effect_assessment IS NULL OR effect_assessment IN (
        'improved', 'neutral', 'worsened'
    )),

    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error_kind text,
    last_error_retryable boolean,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- One outcome per wave, for ever. A wave that settles twice would fold its
    -- replies into the learning record twice.
    UNIQUE (workspace_id, wave_id),
    -- The wave reference is loose, matching the play outcomes: a record of what
    -- the agent did must not block deletion of the wave it was about.
    CHECK (window_end > window_start),
    CHECK ((evidence IS NULL) = (status <> 'succeeded')),
    CHECK ((observed_at IS NULL) = (status <> 'succeeded')),
    -- Evidence and assessment are consistent: insufficient has no assessment.
    CHECK (evidence <> 'insufficient' OR effect_assessment IS NULL),
    -- Measured must have an assessment.
    CHECK (evidence <> 'measured' OR effect_assessment IS NOT NULL)
);

CREATE INDEX viryaos_outreach_wave_outcomes_due_idx
    ON viryaos_outreach_wave_outcomes (workspace_id, available_at)
    WHERE status = 'pending';

CREATE TRIGGER viryaos_outreach_wave_outcomes_set_updated_at
BEFORE UPDATE ON viryaos_outreach_wave_outcomes
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();
