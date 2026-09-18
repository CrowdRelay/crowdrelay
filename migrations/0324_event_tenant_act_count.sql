-- 6.5 — the density counter the organiser product waits on (§4h-4 link).
--
-- `tenant_act_count` is how many acts on a bill resolved to a platform
-- workspace at bill-write time: the event's own act, a roster sibling, or
-- another tenant entirely. Two or more is the density a festival organiser
-- is buying; three is the deferred shared-object build's trigger — both are
-- a WHERE clause over this column, not a nightly scan over event_acts.
--
-- The counter is maintained by trigger rather than computed on read because
-- "the platform knows" is a durable fact: the roster bills that make this
-- climb are written by the ordinary bill path, and no reader should have to
-- re-derive what the write already knew.

ALTER TABLE events
    ADD COLUMN tenant_act_count integer NOT NULL DEFAULT 0
        CHECK (tenant_act_count >= 0);

-- One recompute per affected bill. Statement-level AFTER triggers would be
-- cheaper on the replace path, but a row trigger keeps the count right no
-- matter which statement touches the row — including the per-act tenant
-- resolution UPDATE `replace_event_acts` runs after inserting the bill.
CREATE FUNCTION event_tenant_act_count_refresh(
    p_workspace_id uuid,
    p_event_id uuid
) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE events
    SET tenant_act_count = (
        SELECT count(*)::integer
        FROM event_acts
        WHERE event_acts.workspace_id = p_workspace_id
          AND event_acts.event_id = p_event_id
          AND event_acts.act_workspace_id IS NOT NULL
    )
    WHERE events.workspace_id = p_workspace_id
      AND events.id = p_event_id;
END
$$;

CREATE FUNCTION event_acts_tenant_count() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- A row leaving one event's bill lowers that bill's count; the primary
    -- key can technically move, so the OLD pair is refreshed on the way out.
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM event_tenant_act_count_refresh(OLD.workspace_id, OLD.event_id);
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM event_tenant_act_count_refresh(NEW.workspace_id, NEW.event_id);
    END IF;
    RETURN NULL;
END
$$;

-- Only the columns that can move the count fire it: act_workspace_id is the
-- resolution, workspace_id/event_id are the bill the row belongs to.
CREATE TRIGGER event_acts_tenant_count
AFTER INSERT OR DELETE OR UPDATE OF act_workspace_id, workspace_id, event_id
ON event_acts
FOR EACH ROW EXECUTE FUNCTION event_acts_tenant_count();

-- Backfill: every bill already written counts.
UPDATE events AS event
SET tenant_act_count = counted.total
FROM (
    SELECT workspace_id, event_id, count(*)::integer AS total
    FROM event_acts
    WHERE act_workspace_id IS NOT NULL
    GROUP BY workspace_id, event_id
) AS counted
WHERE counted.workspace_id = event.workspace_id
  AND counted.event_id = event.id;
