-- FAN SCOUT prospect spine.
--
-- A public person the system notices is NOT a first-party fan. These tables
-- keep prospect discovery/evidence separate from the owned fanbase until a
-- verified first-party fan already exists and is explicitly linked.
--
-- fan_prospects is current relationship state. fan_prospect_observations
-- is append-only evidence. Re-discovery may refresh names/last_seen but never
-- silently clears refusal/suppression and never inserts into fans.

CREATE TABLE fan_prospects (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id        uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    platform            text NOT NULL
                        CHECK (
                            char_length(platform) BETWEEN 1 AND 32
                            AND platform = lower(platform)
                            AND platform ~ '^[a-z0-9_-]+$'
                        ),
    identity_kind       text NOT NULL
                        CHECK (identity_kind IN ('platform_user_id', 'handle')),
    external_identity   text NOT NULL
                        CHECK (
                            btrim(external_identity) <> ''
                            AND char_length(external_identity) <= 256
                        ),
    identity_key        text NOT NULL
                        CHECK (
                            btrim(identity_key) <> ''
                            AND char_length(identity_key) <= 256
                        ),
    display_name        text
                        CHECK (
                            display_name IS NULL
                            OR char_length(display_name) BETWEEN 1 AND 200
                        ),
    profile_url         text
                        CHECK (
                            profile_url IS NULL
                            OR char_length(profile_url) BETWEEN 1 AND 1000
                        ),
    status              text NOT NULL DEFAULT 'observed'
                        CHECK (status IN (
                            'observed', 'qualified', 'warming', 'invited',
                            'converted', 'held', 'refused', 'suppressed'
                        )),
    linked_fan_id       uuid,
    first_seen_at       timestamptz NOT NULL,
    last_seen_at        timestamptz NOT NULL,
    converted_at        timestamptz,
    metadata            jsonb NOT NULL DEFAULT '{}'::jsonb
                        CHECK (jsonb_typeof(metadata) = 'object'),
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CHECK (last_seen_at >= first_seen_at),
    CHECK (
        (status = 'converted' AND linked_fan_id IS NOT NULL AND converted_at IS NOT NULL)
        OR (status <> 'converted' AND linked_fan_id IS NULL AND converted_at IS NULL)
    ),
    FOREIGN KEY (workspace_id, linked_fan_id)
        REFERENCES fans(workspace_id, id) ON DELETE RESTRICT,
    UNIQUE (workspace_id, platform, identity_kind, identity_key),
    UNIQUE (workspace_id, id)
);

CREATE INDEX fan_prospects_status_idx
    ON fan_prospects (workspace_id, status, last_seen_at DESC);
CREATE INDEX fan_prospects_linked_fan_idx
    ON fan_prospects (workspace_id, linked_fan_id)
    WHERE linked_fan_id IS NOT NULL;

CREATE TABLE fan_prospect_observations (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id        uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    prospect_id         uuid NOT NULL,
    observation_kind    text NOT NULL
                        CHECK (observation_kind IN (
                            'discovery', 'public_engagement', 'affinity',
                            'intent', 'locality', 'referral_potential',
                            'network', 'contactability', 'negative_signal'
                        )),
    source_kind         text NOT NULL
                        CHECK (
                            char_length(source_kind) BETWEEN 1 AND 64
                            AND source_kind ~ '^[a-z0-9_.:-]+$'
                        ),
    source_id           text NOT NULL
                        CHECK (
                            btrim(source_id) <> ''
                            AND char_length(source_id) <= 512
                        ),
    source_url          text
                        CHECK (
                            source_url IS NULL
                            OR char_length(source_url) BETWEEN 1 AND 1000
                        ),
    evidence            jsonb NOT NULL DEFAULT '{}'::jsonb
                        CHECK (jsonb_typeof(evidence) = 'object'),
    observed_at         timestamptz NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, prospect_id)
        REFERENCES fan_prospects(workspace_id, id) ON DELETE CASCADE,
    UNIQUE (
        workspace_id,
        prospect_id,
        observation_kind,
        source_kind,
        source_id
    )
);

CREATE INDEX fan_prospect_observations_recent_idx
    ON fan_prospect_observations (workspace_id, prospect_id, observed_at DESC);
