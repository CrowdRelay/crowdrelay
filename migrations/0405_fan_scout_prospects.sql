-- FAN SCOUT slice 1: the person layer.
--
-- Every candidate table before this one is place-, org- or curator-shaped, so
-- the brain could say where future fans gather but never who. These tables
-- are the missing entity:
--
--   persons              one row per real human the workspace knows about,
--                        whatever roles they hold.
--   person_identities    how a person is recognised: a normalized email or a
--                        platform handle. One (kind, platform, value) belongs
--                        to at most one person, so a second person presenting
--                        it is a merge signal, not a silent write.
--   fan_prospects        a publicly observed person who might become a fan and
--                        is NOT one. A prospect is never a row in `fans`;
--                        `linked_fan_id` is set only when the person joins
--                        through first-party opt-in.
--   fan_prospect_observations
--                        append-only evidence. An observation is never
--                        permission to contact.
--
-- Nothing here is populated by migration: no tenant is enrolled, and no
-- existing fan, contact or beacon is rewritten.

CREATE TABLE persons (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

CREATE TABLE person_identities (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    person_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN ('email', 'platform_handle')),
    -- Null for an email, the lowercase platform name for a handle. The
    -- vocabulary is owned by the domain, not by a CHECK here, so a new source
    -- is a code change and not a constraint rewrite.
    platform text CHECK (platform IS NULL OR (platform = lower(btrim(platform)) AND btrim(platform) <> '')),
    -- Stored normalized (trimmed, lowercase). The CHECK refuses an unnormalized
    -- write, so two spellings of one handle cannot become two people.
    value text NOT NULL CHECK (btrim(value) <> '' AND value = lower(btrim(value))),
    source text NOT NULL CHECK (btrim(source) <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT person_identities_platform_iff_handle
        CHECK ((kind = 'platform_handle') = (platform IS NOT NULL)),
    CONSTRAINT person_identities_person_fk
        FOREIGN KEY (workspace_id, person_id)
        REFERENCES persons (workspace_id, id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX person_identities_one_owner
    ON person_identities (workspace_id, kind, COALESCE(platform, ''), value);
CREATE INDEX person_identities_person_idx
    ON person_identities (workspace_id, person_id);

CREATE TABLE fan_prospects (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    person_id uuid NOT NULL,
    platform text NOT NULL CHECK (platform = lower(btrim(platform)) AND btrim(platform) <> ''),
    -- The handle exactly as the platform shows it, kept for display and for
    -- addressing a reply; identity matching uses the normalized copy on
    -- `person_identities`.
    external_identity text NOT NULL CHECK (btrim(external_identity) <> ''),
    display_name text,
    profile_url text,
    status text NOT NULL DEFAULT 'observed'
        CHECK (status IN ('observed', 'qualified', 'warming', 'invited',
                          'converted', 'held', 'refused', 'suppressed')),
    -- Why a prospect is held, refused or suppressed. Terminal states carry a
    -- reason so a later sweep can tell a decision from a gap.
    status_reason text,
    linked_fan_id uuid,
    -- A prospect is personal data held without the person's knowledge, so the
    -- basis is recorded on the row and the row has a deadline. The basis and
    -- the retention window come from the source class that produced the
    -- prospect (domain `fan_prospect::ProspectSource`), never from the caller.
    lawful_basis text NOT NULL DEFAULT 'legitimate_interest'
        CHECK (lawful_basis IN ('legitimate_interest', 'consent')),
    -- Pushed forward only by new evidence or progress. A prospect that does not
    -- progress is deleted by the retention sweep when this passes; deleting the
    -- prospect (or its person, through `fan_privacy` erasure) cascades its
    -- observations.
    expires_at timestamptz NOT NULL,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    CONSTRAINT fan_prospects_person_fk
        FOREIGN KEY (workspace_id, person_id)
        REFERENCES persons (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT fan_prospects_fan_fk
        FOREIGN KEY (workspace_id, linked_fan_id)
        REFERENCES fans (workspace_id, id) ON DELETE SET NULL (linked_fan_id),
    CONSTRAINT fan_prospects_converted_has_fan
        CHECK (status <> 'converted' OR linked_fan_id IS NOT NULL),
    CONSTRAINT fan_prospects_terminal_has_reason
        CHECK (status NOT IN ('refused', 'suppressed') OR btrim(coalesce(status_reason, '')) <> ''),
    CHECK (last_seen_at >= first_seen_at),
    CHECK (expires_at > first_seen_at)
);
-- One prospect per person per platform, however the handle is capitalized.
CREATE UNIQUE INDEX fan_prospects_one_per_handle
    ON fan_prospects (workspace_id, platform, lower(btrim(external_identity)));
CREATE INDEX fan_prospects_status_idx
    ON fan_prospects (workspace_id, status, last_seen_at DESC);
CREATE INDEX fan_prospects_person_idx
    ON fan_prospects (workspace_id, person_id);
CREATE INDEX fan_prospects_expiry_idx
    ON fan_prospects (expires_at)
    WHERE status NOT IN ('converted', 'refused', 'suppressed');

CREATE TABLE fan_prospect_observations (
    id bigserial PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    prospect_id uuid NOT NULL,
    observation_kind text NOT NULL CHECK (observation_kind IN (
        'commented_similar_band', 'collects_similar_music', 'asked_about_show',
        'asked_for_music', 'active_under_our_post', 'shared_material', 'replied',
        'appears_repeatedly', 'scene_participant', 'related_to_prospects',
        'active_referrer', 'content_creator', 'attends_local_shows')),
    -- The surface that produced it: `community_comments`, `youtube_comments`,
    -- `room_threads`, …
    source text NOT NULL CHECK (btrim(source) <> ''),
    -- The row or message this came from, so re-reading the same source cannot
    -- append the same fact twice.
    source_ref text NOT NULL CHECK (btrim(source_ref) <> ''),
    source_url text,
    observed_at timestamptz NOT NULL,
    -- Verbatim words from the source, never a paraphrase. Bounded: this is the
    -- audit trail for why the system looked at a person, not an archive of
    -- what they wrote.
    evidence text NOT NULL CHECK (char_length(evidence) BETWEEN 1 AND 500),
    confidence_basis_points smallint NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT fan_prospect_observations_prospect_fk
        FOREIGN KEY (workspace_id, prospect_id)
        REFERENCES fan_prospects (workspace_id, id) ON DELETE CASCADE,
    UNIQUE (prospect_id, observation_kind, source, source_ref)
);
CREATE INDEX fan_prospect_observations_prospect_idx
    ON fan_prospect_observations (workspace_id, prospect_id, observed_at DESC);
