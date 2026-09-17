-- Settings that belong to an organisation rather than to one of its acts
-- (4G.2b).
--
-- `tenant_settings` answers "what does this band want", and every roster
-- question it cannot answer has so far been a required query parameter — which
-- means nobody could persist one. The first of those is how many packages a
-- roster can actually run in a period: the manager's own number, which the
-- planner refuses to invent. A planner that picks a default produces a plan
-- nobody agreed to staff, and the manager only discovers that when the third
-- package needs people who are already busy.
--
-- Same shape as `tenant_settings` on purpose: a key/value row per organisation,
-- with the vocabulary validated at the edge rather than by a CHECK per key.
-- A CHECK per key would have to be widened by a migration every time the roster
-- surface grows one, and the value is text either way.
--
-- Numbered 0302 rather than 0301: another branch holds 0301, and two files
-- claiming one version is a migrator error rather than a merge conflict
-- somebody would notice.

CREATE TABLE organization_settings (
    organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    key text NOT NULL CHECK (btrim(key) <> '' AND char_length(key) <= 128),
    value text NOT NULL CHECK (char_length(value) <= 512),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (organization_id, key)
);

COMMENT ON TABLE organization_settings IS
    'Per-organisation operator settings. Absent means never stated, which is '
    'not the same as a zero and never resolves to an invented default.';
