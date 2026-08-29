-- Experiment assignments — first-class experiment units replacing
-- workspace-wide holdout. The experimental unit is explicitly defined
-- (audience, community, campaign, cohort, city, or workspace) and
-- contamination is tracked. When interference is not controllable,
-- the assignment is recorded as a matched quasi-experiment.
CREATE TABLE IF NOT EXISTS viryaos_experiment_assignments (
    id text PRIMARY KEY,
    workspace_id uuid NOT NULL,
    unit_id text NOT NULL,
    unit_kind text NOT NULL,
    arm text NOT NULL CHECK (arm IN ('treatment', 'control')),
    assigned_at timestamptz NOT NULL DEFAULT now(),
    propensity double precision NOT NULL,
    intended_template_id text NOT NULL,
    context jsonb NOT NULL DEFAULT '{}',
    prediction jsonb NOT NULL DEFAULT '{}',
    action_id uuid REFERENCES viryaos_autopilot_actions(id) ON DELETE SET NULL,
    strategy text,
    experiment_kind text NOT NULL DEFAULT 'randomized_holdout',
    contamination_estimate double precision NOT NULL DEFAULT 0.0,
    is_interference_controllable boolean NOT NULL DEFAULT true
);

CREATE INDEX idx_experiment_assignments_workspace
    ON viryaos_experiment_assignments (workspace_id, assigned_at);
CREATE INDEX idx_experiment_assignments_control
    ON viryaos_experiment_assignments (workspace_id, arm, unit_id)
    WHERE arm = 'control';
