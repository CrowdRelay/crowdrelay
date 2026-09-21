-- One community is one row: dedupe agent_outreach_targets on the normalized
-- subreddit, and make the shape enforceable.
--
-- The table's only uniqueness was (workspace_id, display_name, target_kind),
-- but a community's identity is its subreddit — "r/deathcore" and
-- "/r/Deathcore - news, reviews & discussion" are the same place under two
-- display names. Both writers hit the hole: the scanner outcome upsert keys
-- on display_name, and promote_community_places checks only whether a target
-- already carries its place_id. Production measured eleven subreddits held
-- twice, every one of them promoted + admitted — so each relay wave drafted
-- the same post for the same subreddit twice and every draft became its own
-- approval card and assignment e-mail.
--
-- Three moves: a 'duplicate' refusal reason so a merged-away row still says
-- why it died, the merge itself, and a partial unique index so the merge
-- cannot quietly regrow. The index is keyed on the normalized subreddit —
-- lowercase, leading r/ or /r/ stripped — which is also the join key the
-- outcome upsert now conflicts on.

-- 1. The canonical community identity. Every dedupe, upsert-arbiter and
--    cooldown comparison goes through this function — hand-rolled copies of
--    the expression have already drifted twice: a case-sensitive prefix
--    strip let '/R/Deathcore' miss 'deathcore', and a payload-side strip
--    compared against an unstripped stored value never matched, so the
--    post-cooldown joins silently failed. IMMUTABLE so it can sit in the
--    index expression; if the body ever changes the index must be rebuilt.
CREATE OR REPLACE FUNCTION normalize_subreddit(raw text)
RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE
RETURN regexp_replace(lower(btrim(raw)), '^/?r/', '');

-- 2. 'duplicate' joins the refusal vocabulary. The Rust RefusalReason enum
--    only ever writes its own variants and never parses the column back, so
--    extending the check cannot break a reader.
ALTER TABLE agent_outreach_targets
    DROP CONSTRAINT IF EXISTS agent_outreach_targets_refusal_reason_check;
ALTER TABLE agent_outreach_targets
    ADD CONSTRAINT agent_outreach_targets_refusal_reason_check
    CHECK (refusal_reason IS NULL OR refusal_reason = ANY (ARRAY[
        'route_inferred', 'evidence_missing', 'paid_placement',
        'sells_placement', 'implausible_engagement', 'indiscriminate_churn',
        'poor_fit', 'too_small', 'previously_refused', 'off_topic',
        'duplicate'
    ]));

-- 3. Merge the dupes. The survivor is the row an operator would keep: the
--    one with community_posts history first (its drafts and receipts stay
--    attached to a live target), then one linked into the audience graph,
--    then the oldest — a promoted row that predates the dupe is the one the
--    screening trail was written against. Losers leave the pool as
--    discarded AND release the subreddit — the index below covers
--    discarded rows on purpose (a re-proposal must hit the refusal trail,
--    not dodge it), so a loser that kept its subreddit would collide with
--    the survivor. The display name still records which community the row
--    meant; an existing refusal reason is preserved over the new
--    'duplicate' marker.
WITH usage AS (
    SELECT target_id,
           count(*) AS posts,
           bool_or(status IN ('pending', 'awaiting_manual_post')) AS has_live
    FROM community_posts
    GROUP BY target_id
),
ranked AS (
    SELECT t.id,
           ROW_NUMBER() OVER (
               PARTITION BY t.workspace_id,
                            normalize_subreddit(t.subreddit)
               ORDER BY u.has_live DESC NULLS LAST,
                        u.posts DESC NULLS LAST,
                        (t.place_id IS NOT NULL) DESC,
                        t.created_at ASC,
                        t.id ASC
           ) AS rn
    FROM agent_outreach_targets t
    LEFT JOIN usage u ON u.target_id = t.id
    WHERE t.target_kind = 'community'
      AND t.subreddit IS NOT NULL
      AND btrim(t.subreddit) <> ''
)
UPDATE agent_outreach_targets t
SET status = 'discarded',
    subreddit = NULL,
    refusal_reason = COALESCE(t.refusal_reason, 'duplicate'),
    updated_at = now()
FROM ranked
WHERE ranked.id = t.id
  AND ranked.rn > 1;

-- 4. Canonicalize the survivors. The stored value becomes the identity
--    itself — readers like `format!("r/{}", subreddit)` produce
--    "r/r/deathcore" off a prefixed value, and every comparison goes
--    through normalize_subreddit anyway. Display names keep the pretty
--    form; this column does not.
UPDATE agent_outreach_targets
SET subreddit = normalize_subreddit(subreddit),
    updated_at = now()
WHERE target_kind = 'community'
  AND subreddit IS NOT NULL
  AND subreddit IS DISTINCT FROM normalize_subreddit(subreddit);

-- 5. The guardrail: one live-or-dead row per subreddit per workspace. The
--    index deliberately covers discarded rows — a re-proposal of a refused
--    community should land on the existing row and be re-screened there
--    (the upsert keeps status sticky), not spawn a fresh row that dodges the
--    recorded refusal.
CREATE UNIQUE INDEX IF NOT EXISTS agent_outreach_targets_community_subreddit_uk
    ON agent_outreach_targets (
        workspace_id,
        normalize_subreddit(subreddit)
    )
    WHERE target_kind = 'community'
      AND subreddit IS NOT NULL
      AND normalize_subreddit(subreddit) <> '';
