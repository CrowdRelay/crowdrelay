-- Fanbase scout + strategy consult.
--
-- Three capabilities land together:
--
-- 1. Community targets stop being Reddit-only. The scout finds communities on
--    Discord, Telegram, forums and elsewhere; `platform` + `community_url`
--    give a non-Reddit community its identity, and dedup keys on the
--    normalized URL the way 0334 keyed Reddit on the normalized subreddit.
--
-- 2. `strategy_proposals` joins the outcome vocabulary. Each proposal inside
--    an item is evaluated by the deterministic brain and lands a verdict row —
--    accepted and implemented, or rejected with the reason. The consultant's
--    next run reads the verdicts back, which is what makes rejection teach
--    instead of repeat.
--
-- 3. Two bounded implementation channels: scan queries the consultant asks
--    the scout to add (`agent_scan_query_queue`), and one-shot rescan requests
--    that let a proposal pull a template's next run forward without touching
--    its cadence (`agent_template_rescan_requests`, consumed by the dispatch
--    it enabled).

-- 1. The agents service writes kind='strategy_proposals'; the CHECK must
--    know it or the insert dies before the worker can even reject it.
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
        'opportunity_findings',
        'strategy_proposals'
    ));

-- 2. Community identity beyond Reddit. `platform` is the place_kind
--    vocabulary (discord, telegram, forum, ...); `community_url` is the
--    joinable address. Both stay nullable — personal-contact kinds never
--    carry them and a Reddit community identifies by `subreddit` instead.
ALTER TABLE agent_outreach_targets
    ADD COLUMN IF NOT EXISTS platform TEXT
    CHECK (platform IS NULL OR (btrim(platform) <> '' AND char_length(platform) <= 64));

ALTER TABLE agent_outreach_targets
    ADD COLUMN IF NOT EXISTS community_url TEXT
    CHECK (community_url IS NULL OR char_length(community_url) <= 512);

-- 3. Community identity for non-Reddit platforms. Scheme, leading www. and
--    trailing slash are presentation, not identity — 'Discord.gg/Metal' and
--    'https://www.discord.gg/metal/' are the same room. IMMUTABLE so it can
--    sit in the dedup index.
CREATE OR REPLACE FUNCTION normalize_community_url(raw text)
RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE
-- Scheme, 'www.' and host are presentation — fold case there. The path is
-- not: discord.gg/AbC and discord.gg/abc are two different rooms (invite
-- codes are case-sensitive), so only the part before the first '/' folds.
-- Fragments never reach a server, so '#rules' is noise in an identity key;
-- query strings stay — a forum board id lives there.
AS $$
    SELECT btrim(regexp_replace(
        CASE
            WHEN s.rest LIKE '%/%'
                THEN lower(split_part(s.rest, '/', 1)) || substr(s.rest, position('/' in s.rest))
            ELSE lower(s.rest)
        END,
        '/+$', ''))
    FROM (
        SELECT regexp_replace(
            regexp_replace(
                regexp_replace(btrim(raw), '#.*$', ''),
                '^https?://', '', 'i'),
            '^www\.', '', 'i') AS rest
    ) AS s;
$$;

-- One community is one row, generalized: Reddit dedupes on the normalized
-- subreddit (0334); every other platform dedupes on the normalized URL.
CREATE UNIQUE INDEX IF NOT EXISTS agent_outreach_targets_community_url_dedup
    ON agent_outreach_targets (workspace_id, normalize_community_url(community_url))
    WHERE target_kind = 'community'
      AND community_url IS NOT NULL
      AND btrim(community_url) <> '';

-- A community's identity is its subreddit or URL — never its name, and the
-- same display name across two real communities is normal ("Metal Heads"
-- exists on every platform). The pre-existing name key stayed total, so the
-- second community to share a name violated it on insert and sank the whole
-- outcome with a database error. Identity-carrying community rows leave the
-- name key; it still covers personal-contact kinds and communities with no
-- joinable identity at all (the refused 'no joinable address' rows), so a
-- repeated dead proposal still dedupes instead of piling up.
--
-- The merge ahead of the index is defensive: the at-risk set is community
-- rows with no subreddit and no URL sharing a (workspace, name) pair. Such
-- rows were never joinable and carry no posts, so the loser is simply
-- dropped in favour of the oldest row. Rows that DO carry identity are
-- untouched — the partial index does not index them.
DELETE FROM agent_outreach_targets loser
USING agent_outreach_targets survivor
WHERE loser.workspace_id = survivor.workspace_id
  AND loser.display_name = survivor.display_name
  AND loser.target_kind = 'community'
  AND survivor.target_kind = 'community'
  AND COALESCE(normalize_subreddit(loser.subreddit), '') = ''
  AND COALESCE(normalize_subreddit(survivor.subreddit), '') = ''
  AND (loser.community_url IS NULL OR btrim(loser.community_url) = '')
  AND (survivor.community_url IS NULL OR btrim(survivor.community_url) = '')
  AND (loser.created_at, loser.id) > (survivor.created_at, survivor.id)
  AND NOT EXISTS (
      SELECT 1 FROM community_posts cp WHERE cp.target_id = loser.id);

ALTER TABLE agent_outreach_targets
    DROP CONSTRAINT IF EXISTS agent_outreach_targets_workspace_name_kind_uk;

CREATE UNIQUE INDEX IF NOT EXISTS agent_outreach_targets_workspace_name_kind_uk
    ON agent_outreach_targets (workspace_id, display_name, target_kind)
    WHERE target_kind <> 'community'
       OR (COALESCE(normalize_subreddit(subreddit), '') = ''
           AND COALESCE(normalize_community_url(community_url), '') = '');

-- 4. The verdict log the brain writes for every proposal it evaluates.
--    `proposal` carries the model's words verbatim; `implemented_as` says
--    what accepting actually did (target row id, queue row id, policy key,
--    'surfaced' for operator-only proposals). Rejected rows carry the reason
--    the consultant reads back next week.
CREATE TABLE IF NOT EXISTS agent_strategy_proposal_verdicts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    outcome_id      UUID NOT NULL REFERENCES agent_outcomes(id) ON DELETE CASCADE,
    proposal_index  INT NOT NULL,
    action          TEXT NOT NULL CHECK (btrim(action) <> '' AND char_length(action) <= 64),
    verdict         TEXT NOT NULL CHECK (verdict IN ('accepted', 'rejected')),
    reason          TEXT NOT NULL DEFAULT '',
    proposal        JSONB NOT NULL CHECK (jsonb_typeof(proposal) = 'object'),
    implemented_as  TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (outcome_id, proposal_index)
);

CREATE INDEX IF NOT EXISTS agent_strategy_verdicts_ws_idx
    ON agent_strategy_proposal_verdicts (workspace_id, created_at DESC);

-- 5. Queries a strategy consult asks the fanbase scout to add to its next
--    run. The agents service reads unconsumed rows inside
--    discover_fanbase_candidates; the scout's own derived queries never land
--    here — this table is only for proposals the brain accepted.
CREATE TABLE IF NOT EXISTS agent_scan_query_queue (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    query           TEXT NOT NULL CHECK (btrim(query) <> '' AND char_length(query) <= 200),
    reason          TEXT NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    consumed_at     TIMESTAMPTZ
);

-- One pending row per query text; a consumed query may be re-queued (the
-- scout may legitimately re-run it later), and 'Deathcore Forum' is the
-- same ask as 'deathcore forum'.
CREATE UNIQUE INDEX IF NOT EXISTS agent_scan_query_queue_pending_uniq
    ON agent_scan_query_queue (workspace_id, lower(btrim(query)))
    WHERE consumed_at IS NULL;

-- 6. A rescan proposal is "run this template once, soon" — not a cadence
--    change. One pending request per template at a time: a second ask while
--    one is queued is a duplicate, not a stronger ask. The dispatch that the
--    request enabled consumes it at execution, so an accepted rescan fires
--    exactly once.
CREATE TABLE IF NOT EXISTS agent_template_rescan_requests (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    template_id     TEXT NOT NULL CHECK (btrim(template_id) <> '' AND char_length(template_id) <= 64),
    reason          TEXT NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    consumed_at     TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS agent_template_rescan_pending_uniq
    ON agent_template_rescan_requests (workspace_id, template_id)
    WHERE consumed_at IS NULL;

-- 7. Two refusal reasons the non-Reddit community path needs and the
--    screening enum cannot produce: 'no_joinable_address' (the proposal
--    named a community but carried no address the system can act on) and
--    'unsupported_platform' (the URL arrived without a place_kind the graph
--    files). Both are structural refusals — the community was never
--    screenable — which is why they are not RefusalReason variants.
ALTER TABLE agent_outreach_targets
    DROP CONSTRAINT IF EXISTS agent_outreach_targets_refusal_reason_check,
    ADD CONSTRAINT agent_outreach_targets_refusal_reason_check
    CHECK (refusal_reason IS NULL OR refusal_reason = ANY (ARRAY[
        'route_inferred', 'evidence_missing', 'paid_placement',
        'sells_placement', 'implausible_engagement', 'indiscriminate_churn',
        'poor_fit', 'too_small', 'previously_refused', 'off_topic',
        'duplicate', 'no_joinable_address', 'unsupported_platform'
    ]));
