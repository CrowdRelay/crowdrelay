-- A projection must be a function of the fact it projects, never of the clock
-- or of how often the projector runs.
--
-- `viryaos_project_event_content_sources` bumped `version` on every firing of
-- the `events` trigger. Event sync writes `source_last_seen_at` on every cycle
-- and carries `status`, `title`, `starts_at`, `venue` and `city_id` in the same
-- `SET` list, so the trigger fired every sync even when nothing about the event
-- had changed. In production that moved one content source from version 1 to
-- version 283 in thirteen days without a single edit.
--
-- That churn was not cosmetic. An autopilot content-artifact action pins the
-- source version it was decided against, and execution re-reads the source at
-- that exact version. A version bump between the decision and the attempt makes
-- the action fail as `state_changed`, and the next cycle proposes the same work
-- against the new version, which fails the same way. Twelve failed actions in
-- ninety minutes came from this loop alone.
--
-- Two changes, both narrowing what counts as a change:
--
-- 1. `expires_at` on update is derived from the event's own schedule and the
--    value already stored, never from `now()`. The `now() + 7 days` floor still
--    applies when the source is first created, so a source is never born
--    expired; afterwards the window can only be extended by the show moving.
-- 2. The update is skipped entirely when the projected columns already hold the
--    projected values, so `version` moves only when the projected fact moves.
--
-- The completed-show branch is left alone: it fires on the transition into
-- `completed`, not on every sync, so it cannot churn.

CREATE OR REPLACE FUNCTION viryaos_project_event_content_sources()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status IN ('published','completed') THEN
        INSERT INTO viryaos_content_sources(
            id,workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
        ) VALUES(
            NEW.id,NEW.workspace_id,'event','event:' || NEW.id::text,NEW.title,now(),
            GREATEST(NEW.starts_at + INTERVAL '14 days', now() + INTERVAL '7 days'),
            jsonb_build_object('event_id',NEW.id,'slug',NEW.slug,'venue',NEW.venue,'starts_at',NEW.starts_at,'city_id',NEW.city_id),true
        )
        ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
            title=EXCLUDED.title,
            expires_at=GREATEST(viryaos_content_sources.expires_at, NEW.starts_at + INTERVAL '14 days'),
            metadata=EXCLUDED.metadata,
            active=true,version=viryaos_content_sources.version+1
        WHERE viryaos_content_sources.title IS DISTINCT FROM EXCLUDED.title
           OR viryaos_content_sources.metadata IS DISTINCT FROM EXCLUDED.metadata
           OR viryaos_content_sources.active IS DISTINCT FROM true
           OR viryaos_content_sources.expires_at IS DISTINCT FROM
              GREATEST(viryaos_content_sources.expires_at, NEW.starts_at + INTERVAL '14 days');
    ELSE
        UPDATE viryaos_content_sources
        SET active=false,version=version+1
        WHERE workspace_id=NEW.workspace_id AND source_kind='event' AND source_key='event:' || NEW.id::text AND active;
    END IF;

    IF NEW.status = 'completed' THEN
        IF TG_OP = 'INSERT'
           OR (TG_OP = 'UPDATE' AND OLD.status IS DISTINCT FROM 'completed') THEN
            INSERT INTO viryaos_content_sources(
                workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
            ) VALUES(
                NEW.workspace_id,'show_completed','show_completed:' || NEW.id::text,NEW.title,now(),now()+INTERVAL '45 days',
                jsonb_build_object('event_id',NEW.id,'slug',NEW.slug,'venue',NEW.venue,'starts_at',NEW.starts_at,'city_id',NEW.city_id),true
            )
            ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
                title=EXCLUDED.title,metadata=EXCLUDED.metadata,expires_at=EXCLUDED.expires_at,
                active=true,version=viryaos_content_sources.version+1;
        END IF;
    END IF;
    RETURN NEW;
END
$$;
