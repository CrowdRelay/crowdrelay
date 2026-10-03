-- Explicit public identities FAN SCOUT must never treat as prospects.
--
-- A production self-test proved why this cannot be heuristic: the owner's own
-- account was harvested as a prospect, two test replies four minutes apart
-- tripped scout.over_rate, and every autonomous reply lane halted. The row is a
-- durable operator declaration. It blocks future observation and, when written,
-- the repository suppresses an existing live matching prospect. Historical
-- touches remain audit evidence and a standing breach still requires human
-- acknowledgement; adding an exclusion is not an automatic incident clear.
CREATE TABLE fan_prospect_identity_exclusions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('platform_handle','platform_user_id')),
    platform text NOT NULL CHECK (
        btrim(platform) <> '' AND platform=lower(btrim(platform)) AND char_length(platform) <= 64
    ),
    value text NOT NULL CHECK (btrim(value) <> '' AND char_length(value) <= 256),
    reason text NOT NULL CHECK (reason IN ('staff','own_account','test')),
    recorded_by text NOT NULL CHECK (char_length(btrim(recorded_by)) BETWEEN 1 AND 120),
    recorded_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, kind, platform, value)
);

CREATE INDEX fan_prospect_identity_exclusions_workspace_idx
    ON fan_prospect_identity_exclusions (workspace_id, platform, kind, value);
