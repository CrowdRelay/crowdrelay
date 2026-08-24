-- What the band is trying to achieve.
--
-- Every context before this reacts: a series stalled, a step is due, a pipeline
-- is empty. None of them could say whether the work added up to anything,
-- because nothing declared what "anything" would be.
--
-- An objective is an operator's target on a measured series: a value, a
-- deadline and a scope. Nothing about its progress is stored. State is derived
-- on read from the series, the same way growth-metric trends are, because a
-- stored "on track" is a claim that goes stale silently and a derived one
-- cannot.
--
-- The baseline *is* stored, and frozen. Progress measured from a baseline that
-- moves is not progress.

CREATE TABLE viryaos_growth_objectives (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    platform text NOT NULL CHECK (platform IN (
        'spotify', 'youtube', 'bandsintown', 'social', 'website', 'ticketing', 'signal', 'merch'
    )),
    metric_key text NOT NULL CHECK (btrim(metric_key) <> '' AND char_length(metric_key) <= 64),

    -- A workspace target and a city target are different promises. Merging them
    -- would let a national number hide a city where nothing is happening.
    scope_kind text NOT NULL CHECK (scope_kind IN ('workspace', 'city', 'event', 'release_plan')),
    -- Loose reference on purpose, like the metric series it judges: an
    -- objective is a record of what was promised and must not block deletion of
    -- the business row it was about.
    scope_id uuid,
    CONSTRAINT viryaos_growth_objectives_scope_is_consistent
        CHECK ((scope_kind = 'workspace') = (scope_id IS NULL)),

    direction text NOT NULL DEFAULT 'higher_is_better'
        CHECK (direction IN ('higher_is_better', 'lower_is_better')),
    -- Where the series stood when the target was declared. Frozen.
    baseline_value bigint NOT NULL,
    target_value bigint NOT NULL,

    declared_at timestamptz NOT NULL DEFAULT now(),
    deadline timestamptz NOT NULL,
    -- Who promised it. An objective nobody owns is a wish.
    declared_by text NOT NULL CHECK (btrim(declared_by) <> '' AND char_length(declared_by) <= 120),
    -- An operator may retire a target. It is never deleted: a target that was
    -- declared and then removed is exactly the thing a later review needs to
    -- see, and a missing row cannot be reviewed.
    retired_at timestamptz,

    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    UNIQUE (workspace_id, id),
    -- One live target per series per scope. Two would let a report pick the
    -- friendlier one.
    UNIQUE NULLS NOT DISTINCT (workspace_id, platform, metric_key, scope_kind, scope_id),
    CHECK (deadline > declared_at)
);

CREATE TRIGGER viryaos_growth_objectives_set_updated_at
BEFORE UPDATE ON viryaos_growth_objectives
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_growth_objectives_live_idx
    ON viryaos_growth_objectives (workspace_id, deadline)
    WHERE retired_at IS NULL;
