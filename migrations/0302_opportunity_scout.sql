-- Sprint 3 — opportunity scout: widen the opportunity vocabulary, keep the
-- observation honest, and remember why a row closed.
--
-- Scout findings are review-only: they land on the operator's shortlist and
-- never enter the live-application evaluator until a human advances them.
-- Rows without dated source evidence are not considered observed — the link
-- is the finding. Lost and dismissed rows carry the reason they closed so a
-- refusal teaches the pipeline instead of disappearing.

ALTER TABLE viryaos_team_opportunities
    DROP CONSTRAINT IF EXISTS viryaos_team_opportunities_opportunity_kind_check;
ALTER TABLE viryaos_team_opportunities
    ADD CONSTRAINT viryaos_team_opportunities_opportunity_kind_check
    CHECK (opportunity_kind IN (
        'festival','showcase','review_contest','support_slot','funding',
        'booking','press','interview','sync'
    ));

ALTER TABLE viryaos_team_opportunities
    ADD COLUMN IF NOT EXISTS source_observed_at timestamptz;

-- A row that already names a destination was observed when it was written;
-- rows with no link stay unobserved — they are not dated source evidence.
UPDATE viryaos_team_opportunities
SET source_observed_at = created_at
WHERE source_observed_at IS NULL AND destination_url IS NOT NULL;

ALTER TABLE viryaos_team_opportunities
    ADD COLUMN IF NOT EXISTS status_reason text
    CHECK (status_reason IS NULL OR (
        char_length(btrim(status_reason)) BETWEEN 1 AND 240
    ));

-- The agents service writes one agent_outcomes row per finding; the kind
-- must exist in the table's vocabulary or the insert dies on the CHECK.
ALTER TABLE agent_outcomes
    DROP CONSTRAINT IF EXISTS agent_outcomes_kind_check,
    ADD CONSTRAINT agent_outcomes_kind_check CHECK (kind IN (
        'press_pitch',
        'social_post',
        'signal_push',
        'audience_segments',
        'outreach_targets',
        'campaign_insight',
        'release_plan_note',
        'generic_insight',
        'opportunity_findings'
    ));
