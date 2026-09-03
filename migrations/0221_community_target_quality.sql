-- Community targets carry their quality, and the brain learns per target.
--
-- Two gaps this closes.
--
-- First: a community target proposed by an agent went straight to
-- `promoted` with nothing recorded about whether it was worth engaging.
-- `crowdrelay_domain::target_discovery::screen_candidate` existed and was
-- never called on this path, so the screening policy — fit, size, plausible
-- engagement, paid placement, indiscriminate churn — had no effect on which
-- communities the growth loop posted to. The verdict now lives on the row,
-- so a refusal is durable and the next scan does not re-propose what was
-- already refused.
--
-- Second: the candidate loader read `agent_outreach_targets` alone, ordered
-- by `created_at DESC LIMIT 20`. The audience graph already holds
-- `member_count`, `activity_bp`, `genres` and the community's self-promo
-- rules, and none of it reached the brain. `place_id` is the join that lets
-- it, so the loader can order by what a target is worth rather than by when
-- somebody found it.
--
-- `target_key` on the evidence rows is the third piece: the causal model
-- pools per template and per subreddit type, so every community in the same
-- genre bucket predicted identically. Recording which target an outcome came
-- from is what makes a per-target posterior possible on replay.

-- ── agent_outreach_targets: screening verdict and audience-graph link ──

ALTER TABLE agent_outreach_targets
    ADD COLUMN IF NOT EXISTS place_id uuid REFERENCES discovery_places(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS screening_verdict text
        CHECK (screening_verdict IS NULL OR screening_verdict IN ('admitted', 'refused')),
    ADD COLUMN IF NOT EXISTS refusal_reason text
        CHECK (refusal_reason IS NULL OR refusal_reason IN (
            'route_inferred', 'evidence_missing', 'paid_placement',
            'sells_placement', 'implausible_engagement', 'indiscriminate_churn',
            'poor_fit', 'too_small'
        )),
    ADD COLUMN IF NOT EXISTS screened_at timestamptz;

COMMENT ON COLUMN agent_outreach_targets.place_id IS
    'The audience-graph place this target refers to, where one is known. Carries member_count, activity_bp, genres and the self-promo rules into candidate generation.';
COMMENT ON COLUMN agent_outreach_targets.screening_verdict IS
    'Result of target_discovery::screen_candidate at ingest. NULL means the row predates screening and is treated as unscreened, not as admitted.';
COMMENT ON COLUMN agent_outreach_targets.refusal_reason IS
    'Why the target will never be engaged. Set only when screening_verdict = refused.';

-- Link existing community targets to their audience-graph place by the
-- subreddit slug in the place URL. Reddit place URLs are
-- `https://www.reddit.com/r/<name>/`, and `agent_outreach_targets.subreddit`
-- holds the same name without the `r/` prefix, so the slug is the join key.
UPDATE agent_outreach_targets AS t
SET place_id = p.id
FROM discovery_places AS p
WHERE t.place_id IS NULL
  AND t.subreddit IS NOT NULL
  AND p.workspace_id = t.workspace_id
  AND p.place_kind = 'subreddit'
  AND lower(substring(p.url from '/r/([^/?#]+)')) = lower(btrim(t.subreddit));

-- Refuse the legacy community targets the screening policy would have
-- refused on the way in. Only rows with a linked place are judged: without
-- a member count there is no evidence to refuse on, and an absent number is
-- not the same as a bad one. The thresholds match
-- `TargetDiscoveryPolicy::default()` — minimum_follower_count = 250,
-- minimum_engagement_basis_points = 100.
UPDATE agent_outreach_targets AS t
SET screening_verdict = 'refused',
    refusal_reason = 'too_small',
    screened_at = now()
FROM discovery_places AS p
WHERE t.screening_verdict IS NULL
  AND t.target_kind = 'community'
  AND t.place_id = p.id
  AND p.member_count IS NOT NULL
  AND p.member_count < 250;

UPDATE agent_outreach_targets AS t
SET screening_verdict = 'refused',
    refusal_reason = 'implausible_engagement',
    screened_at = now()
FROM discovery_places AS p
WHERE t.screening_verdict IS NULL
  AND t.target_kind = 'community'
  AND t.place_id = p.id
  AND p.member_count IS NOT NULL
  AND p.member_count >= 5000
  AND p.activity_bp IS NOT NULL
  AND p.activity_bp < 100;

-- A community our own operators marked as blocked, rejected or not-a-fit is
-- not a growth candidate whatever its size says.
UPDATE agent_outreach_targets AS t
SET screening_verdict = 'refused',
    refusal_reason = 'poor_fit',
    screened_at = now()
FROM discovery_places AS p
WHERE t.screening_verdict IS NULL
  AND t.target_kind = 'community'
  AND t.place_id = p.id
  AND (p.status = 'blocked' OR p.membership_state IN ('rejected', 'not_a_fit'));

-- The candidate loader reads promoted, non-refused community targets and
-- orders them by what the linked place is worth, so the index matches that
-- read rather than the old `created_at DESC`.
CREATE INDEX IF NOT EXISTS agent_outreach_targets_community_admitted_idx
    ON agent_outreach_targets (workspace_id, place_id)
    WHERE target_kind = 'community'
      AND status = 'promoted'
      AND subreddit IS NOT NULL
      AND screening_verdict IS DISTINCT FROM 'refused';

-- ── viryaos_growth_evidence: which target the outcome came from ──

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS target_key text
        CHECK (target_key IS NULL OR (btrim(target_key) <> '' AND char_length(target_key) <= 200));

COMMENT ON COLUMN viryaos_growth_evidence.target_key IS
    'Stable target identity for the dispatch, e.g. community:<target_id>. The per-target level of the causal hierarchy is rebuilt from this on replay; NULL means the dispatch had no target (workspace-wide templates) or predates the column.';

CREATE INDEX IF NOT EXISTS viryaos_growth_evidence_target_key_idx
    ON viryaos_growth_evidence (workspace_id, target_key, timestamp DESC)
    WHERE target_key IS NOT NULL;
