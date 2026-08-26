-- Per-kind outreach outcomes, so wave composition can learn.
--
-- `viryaos_play_learning` holds per-play-kind standings, but outreach has no
-- per-kind outcome to learn from — a reply is recorded, an effect is not.
-- This table closes that loop: after a wave of a given target kind has run
-- its course (every pitch sent, every reply classified, every placement
-- verified or rejected), the settled outcome is folded into a standing that
-- scales future wave sizing for that kind.
--
-- Same discipline as play learning: counts not a score, a record that may
-- only narrow, retirement is a stated fact, and authority is untouched. The
-- weight scales one number — how many pitches a wave of this kind may
-- carry — and only downward.

CREATE TABLE viryaos_outreach_kind_learning (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- One of the OutreachTargetKind values: playlist, radio, press, etc.
    target_kind text NOT NULL CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator',
        'support_slot', 'endorsement', 'media_patronage'
    )),

    improved_count integer NOT NULL DEFAULT 0 CHECK (improved_count >= 0),
    neutral_count integer NOT NULL DEFAULT 0 CHECK (neutral_count >= 0),
    worsened_count integer NOT NULL DEFAULT 0 CHECK (worsened_count >= 0),
    insufficient_count integer NOT NULL DEFAULT 0 CHECK (insufficient_count >= 0),
    consecutive_worsened integer NOT NULL DEFAULT 0 CHECK (consecutive_worsened >= 0),

    -- `10000` is the operator's full configured wave size for this kind.
    weight_basis_points integer NOT NULL DEFAULT 10000
        CHECK (weight_basis_points BETWEEN 0 AND 10000),
    retired_at timestamptz,
    retired_reason text CHECK (retired_reason IS NULL OR retired_reason IN (
        'repeatedly_worsened', 'operator_retired'
    )),

    updated_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (workspace_id, target_kind),
    CONSTRAINT viryaos_outreach_kind_learning_retirement_is_stated
        CHECK ((retired_at IS NULL) = (retired_reason IS NULL)),
    CONSTRAINT viryaos_outreach_kind_learning_weight_matches_retirement
        CHECK ((weight_basis_points = 0) = (retired_at IS NOT NULL))
);

CREATE TRIGGER viryaos_outreach_kind_learning_set_updated_at
BEFORE UPDATE ON viryaos_outreach_kind_learning
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();
