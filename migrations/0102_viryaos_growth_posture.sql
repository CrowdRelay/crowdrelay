-- The one dial.
--
-- Every authority surface in this system is a row an operator can set:
-- twenty-one policy rows, four class ceilings, one envelope. Correct, and
-- also twenty-six switches — which is how "the agent does everything it can
-- for free" decays into an afternoon of endpoint calls every time the posture
-- should move. This table records which posture the operator chose, and the
-- write path behind it applies all three surfaces in one transaction.
--
-- What this deliberately is not: automatic widening. Authority never widens
-- by itself — setting a posture IS the human decision, recorded here and in
-- the operator ledger. Individual knobs stay editable afterwards; applying a
-- posture again reapplies the template.

CREATE TABLE viryaos_growth_posture (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    -- grounded  = sees everything, touches nobody (dry run).
    -- working   = first-party work runs alone; outward contact drafts for
    --             one-click approval.
    -- full_send = owned audience sends within budget/cooldown/deliverability;
    --             free third-party pitching runs unattended. Money never does.
    posture text NOT NULL CHECK (posture IN ('grounded', 'working', 'full_send')),
    expected_version bigint NOT NULL DEFAULT 1 CHECK (expected_version > 0),
    set_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER viryaos_growth_posture_set_updated_at
BEFORE UPDATE ON viryaos_growth_posture
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();
