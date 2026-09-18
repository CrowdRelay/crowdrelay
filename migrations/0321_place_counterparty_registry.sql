-- Shared counterparty registry (5.14, §4h-7.3 + §4f-2): the promoter side
-- of the archive the venue registry started.
--
-- The same shape as place_venues, deliberately:
--
-- place_counterparties is global. It carries no workspace_id — the promoter
-- who booked three different tenants' shows is one person, and the identity
-- is the email the event already records. A venue's identity needed a city
-- because two cities can share a room name; an email needs nothing but
-- itself.
--
-- place_counterparty_marks is the contributed observation, workspace-scoped
-- the same way: a mark says "this tenant's show named that counterparty"
-- and stays private to its contributor. What the roster archive reads is
-- the recurrence — the same person named by several acts — which is the
-- density §4f-2's registries were waiting for.
--
-- The contribution source is events.counterparty_email, so the record
-- builds itself: every published or completed event naming a counterparty
-- marks them. A cancelled or de-counterpartied event retracts the claim it
-- made, matching the venue trigger's discipline — a planned night is not a
-- met promoter.

CREATE TABLE place_counterparties (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The normalized identity: the email as the event recorded it, lowered
    -- and trimmed. The events CHECK already guarantees the shape.
    email_key text NOT NULL CHECK (btrim(email_key) <> '' AND char_length(email_key) <= 320),
    display_name text CHECK (display_name IS NULL OR char_length(display_name) <= 160),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (email_key)
);

CREATE TABLE place_counterparty_marks (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    counterparty_id uuid NOT NULL REFERENCES place_counterparties(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    -- The name as this event recorded it — the per-mark spelling, so the
    -- archive can show the most recent contribution without rewriting the
    -- shared row's identity.
    contributed_name text CHECK (contributed_name IS NULL OR char_length(contributed_name) <= 160),
    marked_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, event_id)
);

CREATE INDEX place_counterparty_marks_counterparty_idx
    ON place_counterparty_marks (counterparty_id, workspace_id);

CREATE FUNCTION place_counterparties_mark_event() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    counterparty_uuid uuid;
    new_key text;
BEGIN
    -- An event never changes workspace in practice — but if one ever does,
    -- the mark it leaves behind under the old workspace would double-count
    -- the person's record. Retract it before anything else. (Same guard the
    -- venue trigger carries.)
    IF TG_OP = 'UPDATE'
       AND OLD.workspace_id IS DISTINCT FROM NEW.workspace_id THEN
        DELETE FROM place_counterparty_marks
        WHERE workspace_id = OLD.workspace_id AND event_id = OLD.id;
    END IF;

    new_key := nullif(lower(btrim(NEW.counterparty_email)), '');
    -- A show that no longer names a counterparty, or left the show
    -- statuses, retracts the claim it made.
    IF new_key IS NULL
       OR NEW.status NOT IN ('published', 'completed') THEN
        IF TG_OP = 'UPDATE' THEN
            DELETE FROM place_counterparty_marks
            WHERE workspace_id = NEW.workspace_id AND event_id = NEW.id;
        END IF;
        RETURN NEW;
    END IF;

    INSERT INTO place_counterparties (email_key, display_name)
    VALUES (new_key, NEW.counterparty_name)
    ON CONFLICT (email_key) DO NOTHING
    RETURNING id INTO counterparty_uuid;
    IF counterparty_uuid IS NULL THEN
        SELECT id INTO counterparty_uuid
        FROM place_counterparties
        WHERE email_key = new_key;
    END IF;

    -- One mark per show per tenant. A re-named counterparty re-states the
    -- mark's contribution rather than stacking a second claim on the event.
    INSERT INTO place_counterparty_marks (counterparty_id, workspace_id, event_id, contributed_name)
    VALUES (counterparty_uuid, NEW.workspace_id, NEW.id, NEW.counterparty_name)
    ON CONFLICT (workspace_id, event_id)
    DO UPDATE SET counterparty_id = EXCLUDED.counterparty_id,
                  contributed_name = EXCLUDED.contributed_name;
    RETURN NEW;
END;
$$;

CREATE TRIGGER place_counterparties_mark_event
AFTER INSERT OR UPDATE OF counterparty_email, counterparty_name, status, workspace_id ON events
FOR EACH ROW EXECUTE FUNCTION place_counterparties_mark_event();

-- Backfill from the events that already exist so the archive answers on the
-- first request, not the first new show — the roster scan the row was
-- waiting for, applied to every tenant at once.
INSERT INTO place_counterparties (email_key, display_name)
SELECT DISTINCT ON (lower(btrim(counterparty_email)))
    lower(btrim(counterparty_email)),
    counterparty_name
FROM events
WHERE counterparty_email IS NOT NULL
  AND status IN ('published', 'completed')
ORDER BY lower(btrim(counterparty_email)), starts_at DESC
ON CONFLICT (email_key) DO NOTHING;

INSERT INTO place_counterparty_marks (counterparty_id, workspace_id, event_id, contributed_name, marked_at)
SELECT counterparty.id, event.workspace_id, event.id,
       event.counterparty_name,
       event.starts_at
FROM events AS event
JOIN place_counterparties AS counterparty
  ON counterparty.email_key = lower(btrim(event.counterparty_email))
WHERE event.status IN ('published', 'completed')
ON CONFLICT (workspace_id, event_id) DO NOTHING;
