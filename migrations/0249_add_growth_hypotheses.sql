-- Add hypothesis lifecycle persistence.
--
-- The brain's hypothesis lifecycle was hardcoded to Active for all
-- templates. This table persists the lifecycle state per
-- (workspace_id, template_id) so the brain can track which templates
-- are earning trust (Testing → Paper → MicroLive → Active) and which
-- have lost their edge (Degraded → Retired).
--
-- Ported from Kern's hypothesis lifecycle, which uses the same
-- progression: a hypothesis earns trust through stages before getting
-- full dispatch budget, and gets retired when it stops working.

CREATE TABLE IF NOT EXISTS viryaos_growth_hypotheses (
    workspace_id uuid NOT NULL,
    template_id text NOT NULL,
    state text NOT NULL DEFAULT 'active'
        CHECK (state IN ('discovered', 'testing', 'paper', 'micro_live',
                         'active', 'degraded', 'retired')),
    posterior_samples integer NOT NULL DEFAULT 0,
    posterior_mean double precision NOT NULL DEFAULT 0.0,
    posterior_std double precision NOT NULL DEFAULT 0.0,
    last_transition_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, template_id)
);

-- Index for efficient lookup by workspace
CREATE INDEX IF NOT EXISTS idx_growth_hypotheses_workspace
    ON viryaos_growth_hypotheses (workspace_id);
