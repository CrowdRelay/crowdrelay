-- Whether each growth component will actually do its work, readable.
--
-- `GrowthReadiness` names fourteen components and answers, for each, "is this
-- switched on". Its only output is one `tracing::info!` at worker startup. So the
-- answer exists, in a container's log, written once, and every surface an
-- operator has — `/metrics`, `ops/attention`, `ops/summary` — is silent about it.
--
-- Measured 2026-09-13: the operator set the publishing switches, approved every
-- brain suggestion, and asked why nothing had published. Answering it needed the
-- worker's log, and the answer at the time was wrong anyway —
-- `community_executor_enabled` reported `community_executor.is_some()`, which is
-- true in manual mode, so the field said the Reddit executor was on while it
-- would never post.
--
-- Configuration is state. A component that is off is as much a fact about this
-- workspace as a fan row is, and the brain's own North Star depends on several of
-- these being on. Putting it in Postgres is what makes it answerable without a
-- shell on the deploy host.
CREATE TABLE growth_component_state (
  workspace_id UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  -- The stable identifier from `GrowthReadiness::components()`. Not a display
  -- name: it is a key an operator surface and a gate both match on.
  component    TEXT        NOT NULL CHECK (component ~ '^[a-z][a-z0-9_]*$'),
  -- Will this component do its work? Not "was it constructed" — that distinction
  -- is the whole reason this table exists.
  enabled      BOOLEAN     NOT NULL,
  -- Why not, when the worker knows: the name of the switch that is missing. Null
  -- when enabled, and null when the reason is not a single switch.
  missing_switch TEXT      CHECK (missing_switch IS NULL
                                 OR missing_switch ~ '^[A-Z][A-Z0-9_]*$'),
  -- When the worker last reported this. A stale row is a worker that has not
  -- started since the last change, which is itself worth being able to see:
  -- these switches are read at startup, so an operator who edits the env file
  -- and does not restart has changed nothing.
  observed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, component)
);

-- The operator question is "what is off", which reads the disabled set for one
-- workspace. Partial, because that set is the small one and the only one anybody
-- reads a list of.
CREATE INDEX growth_component_state_disabled_idx
  ON growth_component_state (workspace_id, component)
  WHERE NOT enabled;

COMMENT ON TABLE growth_component_state IS
  'One row per growth component, reported by the worker at startup. Answers "will this component do its work", not "was it constructed".';
