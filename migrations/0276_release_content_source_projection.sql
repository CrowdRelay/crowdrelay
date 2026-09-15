-- Release plans project into the content supply chain (CROWDRELAY_LEVERAGE_PLAN
-- §4i-6: at R-0 Signal receives the release before platforms).
--
-- `viryaos_content_sources` already knows the `release` kind and its ordered
-- artifact chain — signal_push first, then social_feed, social_story,
-- newsletter_block and press_hook one at a time — but the only writer of
-- release rows was `/v1/internal/releases/announce`, which projects
-- Spotify-detected drops under `spotify:{source_release_id}` keys. A plan the
-- operator entered by hand never became a source at all, so the plan-driven
-- chain could not fire. This trigger is the same fact projection the events
-- table already has: it never chooses copy, channel or an executable action;
-- those decisions remain in deterministic Rust.
--
-- `occurred_at = release_at` is the load-bearing line. The supply evaluator
-- holds a source whose `occurred_at` is still in the future, so the chain
-- stays dormant until release day and the first thing it owes is the Signal
-- push — Signal first, platforms second, by construction. `expires_at`
-- extends the source forty-five days past release so the R+14 second wave and
-- the R+30 catalogue transition still have material to work from.
--
-- The plan's own switches are projected as facts in `metadata`, not read
-- here: the evaluator drops the fan-facing artifacts when the operator turned
-- communication off, drops the press hook when press is off or the tier is
-- filler, and a filler release is still real material the filler calendar
-- posts. Spend-bearing paths (waves, runway plays, milestone sends) gate on
-- `viryaos_release_plans.tier` at their own queries.
--
-- Known seam: a release entered both ways — detected by the Spotify watcher
-- (`spotify:` key) and planned by hand (`release:` key) — projects twice and
-- produces two artifact chains for one album. Approval-gated artifacts get
-- human dedup for free; the honest bridge is a plan ↔ spotify-release link,
-- which does not exist yet and is deliberately not guessed here.

CREATE OR REPLACE FUNCTION viryaos_project_release_content_sources()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- The same forty-five-day window the backfill applies: a plan whose
    -- release window already closed (reactivated history, a date pushed far
    -- into the past) would project a source the evaluator can only ever hold
    -- as stale, so it projects nothing at all.
    IF NEW.active AND NEW.release_at > now() - INTERVAL '45 days' THEN
        INSERT INTO viryaos_content_sources(
            id,workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
        ) VALUES(
            NEW.id,NEW.workspace_id,'release','release:' || NEW.id::text,NEW.title,NEW.release_at,
            NEW.release_at + INTERVAL '45 days',
            jsonb_build_object('release_plan_id',NEW.id,'source_key',NEW.source_key,
                'release_at',NEW.release_at,'listen_url',NEW.listen_url,
                'assets_ready',NEW.assets_ready,'tier',NEW.tier,
                'communication_enabled',NEW.communication_enabled,
                'press_enabled',NEW.press_enabled),true
        )
        ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
            title=EXCLUDED.title,
            occurred_at=EXCLUDED.occurred_at,
            expires_at=EXCLUDED.expires_at,
            metadata=EXCLUDED.metadata,
            active=true,version=viryaos_content_sources.version+1
        WHERE viryaos_content_sources.title IS DISTINCT FROM EXCLUDED.title
           OR viryaos_content_sources.occurred_at IS DISTINCT FROM EXCLUDED.occurred_at
           OR viryaos_content_sources.expires_at IS DISTINCT FROM EXCLUDED.expires_at
           OR viryaos_content_sources.metadata IS DISTINCT FROM EXCLUDED.metadata
           OR viryaos_content_sources.active IS DISTINCT FROM true;
    ELSE
        UPDATE viryaos_content_sources
        SET active=false,version=version+1
        WHERE workspace_id=NEW.workspace_id AND source_kind='release'
          AND source_key='release:' || NEW.id::text AND active;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER viryaos_release_plans_project_content_sources
AFTER INSERT OR UPDATE OF title, release_at, listen_url, source_key, assets_ready,
    communication_enabled, press_enabled, tier, active
ON viryaos_release_plans
FOR EACH ROW EXECUTE FUNCTION viryaos_project_release_content_sources();

-- Backfill the plans whose release window is still open; a plan whose
-- release date passed more than forty-five days ago would project an
-- already-stale source the evaluator can only ever hold.
INSERT INTO viryaos_content_sources(
    id,workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
)
SELECT plan.id,plan.workspace_id,'release','release:' || plan.id::text,plan.title,plan.release_at,
       plan.release_at + INTERVAL '45 days',
       jsonb_build_object('release_plan_id',plan.id,'source_key',plan.source_key,
           'release_at',plan.release_at,'listen_url',plan.listen_url,
           'assets_ready',plan.assets_ready,'tier',plan.tier,
           'communication_enabled',plan.communication_enabled,
           'press_enabled',plan.press_enabled),true
FROM viryaos_release_plans AS plan
WHERE plan.active
  AND plan.release_at > now() - INTERVAL '45 days'
ON CONFLICT(workspace_id,source_kind,source_key) DO NOTHING;
