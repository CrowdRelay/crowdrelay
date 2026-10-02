-- FAN SCOUT slice 2: the Latarnik role, keyed to a person.
--
-- `viryaos_beacon_signal_profiles` is keyed to a beacon (an organisation: a
-- radio station, a venue), so a fan physically could not hold Latarnik access,
-- and nothing created advocates before their first referral. A Latarnik is a
-- role on a *person* (`persons`, migration 0405); beacons keep their own
-- semantics and are not relabelled to fit.
--
-- One row per person. The role never changes the person's fan status, and a
-- person who is also a contact, a referrer or a beacon contact keeps one
-- `persons` row for all of it.
--
-- Nothing is populated by migration: no tenant is enrolled, no fan is asked.

CREATE TABLE latarnik_roles (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    person_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'candidate'
        CHECK (status IN ('candidate', 'invited', 'active', 'paused', 'revoked')),
    -- What produced the row: `fan_evidence_sweep`, later `operator`.
    source text NOT NULL CHECK (btrim(source) <> ''),
    -- The typed evidence the detection read, frozen at detection: counts and
    -- booleans only, never an address or a name. It is the answer to "why were
    -- they asked" a year from now, when the fan's behaviour has moved on.
    evidence jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(evidence) = 'object'),
    status_reason text,
    capabilities jsonb NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(capabilities) = 'array'),
    candidate_at timestamptz NOT NULL DEFAULT now(),
    invited_at timestamptz,
    activated_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, person_id),
    CONSTRAINT latarnik_roles_person_fk
        FOREIGN KEY (workspace_id, person_id)
        REFERENCES persons (workspace_id, id) ON DELETE CASCADE,
    -- A status carries the fact that put it there: nobody is "active" without
    -- an invitation and an activation on record, and an ended role says why.
    CONSTRAINT latarnik_roles_invited_has_time
        CHECK (status NOT IN ('invited', 'active', 'paused') OR invited_at IS NOT NULL),
    CONSTRAINT latarnik_roles_active_has_time
        CHECK (status NOT IN ('active', 'paused') OR activated_at IS NOT NULL),
    CONSTRAINT latarnik_roles_revoked_has_reason
        CHECK (status <> 'revoked' OR (revoked_at IS NOT NULL AND btrim(coalesce(status_reason, '')) <> ''))
);
CREATE INDEX latarnik_roles_status_idx
    ON latarnik_roles (workspace_id, status, candidate_at DESC);
