-- A finished show's content source is dated by the night, not by the import.
--
-- `project_event_content_sources` registered the `show_completed` source with
-- `occurred_at = now()` and `expires_at = now() + 45 days`. The Rust helper
-- that registers the same source (`ensure_show_completed_source`) has used
-- `starts_at` from the start, because the supply policy counts both the
-- harvest window and the 45-day age limit from `occurred_at`, and what is
-- being harvested is the night.
--
-- The difference mattered the day old shows arrived. On 2026-09-24 past
-- shows were imported as `completed`, the trigger stamped every one of them
-- as having happened that day, and the harvest chain drafted post-show
-- recaps for nights in July and August as if they were last weekend's.
--
-- Two changes:
--
-- 1. The trigger's `show_completed` branch dates the source by
--    `NEW.starts_at`, the same as the helper. A show that completed long ago
--    is born stale, and the evaluator holds it — which is the truth.
-- 2. Existing trigger-made rows are re-dated to their show. `version` is
--    left alone: nothing an artifact renders from moved, and a bump would
--    fail every in-flight artifact pinned to the current version.
--
-- The `event` branch is unchanged. Its source is read as a held moment by
-- the cadence register, and its pre-show artifacts are gated at the read
-- (`load_content_supply_snapshots`) once the show is over.

CREATE OR REPLACE FUNCTION project_event_content_sources()
 RETURNS trigger
 LANGUAGE plpgsql
AS $function$
BEGIN
    IF NEW.status IN ('published','completed') THEN
        INSERT INTO content_sources(
            id,workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
        ) VALUES(
            NEW.id,NEW.workspace_id,'event','event:' || NEW.id::text,NEW.title,now(),
            GREATEST(NEW.starts_at + INTERVAL '14 days', now() + INTERVAL '7 days'),
            jsonb_build_object('event_id',NEW.id,'slug',NEW.slug,'venue',NEW.venue,'starts_at',NEW.starts_at,'city_id',NEW.city_id),true
        )
        ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
            title=EXCLUDED.title,
            expires_at=GREATEST(content_sources.expires_at, NEW.starts_at + INTERVAL '14 days'),
            metadata=EXCLUDED.metadata,
            active=true,version=content_sources.version+1
        WHERE content_sources.title IS DISTINCT FROM EXCLUDED.title
           OR content_sources.metadata IS DISTINCT FROM EXCLUDED.metadata
           OR content_sources.active IS DISTINCT FROM true
           OR content_sources.expires_at IS DISTINCT FROM
              GREATEST(content_sources.expires_at, NEW.starts_at + INTERVAL '14 days');
    ELSE
        UPDATE content_sources
        SET active=false,version=version+1
        WHERE workspace_id=NEW.workspace_id AND source_kind='event' AND source_key='event:' || NEW.id::text AND active;
    END IF;

    IF NEW.status = 'completed' THEN
        IF TG_OP = 'INSERT'
           OR (TG_OP = 'UPDATE' AND OLD.status IS DISTINCT FROM 'completed') THEN
            INSERT INTO content_sources(
                workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
            ) VALUES(
                NEW.workspace_id,'show_completed','show_completed:' || NEW.id::text,NEW.title,
                NEW.starts_at,NEW.starts_at + INTERVAL '45 days',
                jsonb_build_object('event_id',NEW.id,'slug',NEW.slug,'venue',NEW.venue,'starts_at',NEW.starts_at,'city_id',NEW.city_id),true
            )
            ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
                title=EXCLUDED.title,metadata=EXCLUDED.metadata,
                occurred_at=EXCLUDED.occurred_at,expires_at=EXCLUDED.expires_at,
                active=true,version=content_sources.version+1;
        END IF;
    END IF;
    RETURN NEW;
END
$function$;

UPDATE content_sources AS source
SET occurred_at = event.starts_at,
    expires_at = event.starts_at + INTERVAL '45 days',
    updated_at = now()
FROM events AS event
WHERE source.workspace_id = event.workspace_id
  AND source.source_kind = 'show_completed'
  AND source.source_key = 'show_completed:' || event.id::text
  AND source.occurred_at > event.starts_at + INTERVAL '1 day';
