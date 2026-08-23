-- Phase 9: the pitcher's supply.
--
-- `viryaos_outreach_targets` is written only by operator upsert today, and it
-- holds zero rows in production, which makes every pitcher a loop over an empty
-- table. Discovery fills it — but a discovered contact is not a target, and the
-- distance between the two is the whole point of this migration.
--
-- A candidate carries where it came from and the evidence a route was read out
-- of. It becomes a target only once the route is confirmed by an operator. Two
-- rules are enforced by the schema rather than by remembering them:
--
-- 1. `route_is_published` is stored on the row. A route that was worked out
--    rather than read never enters the pipeline, and the column keeps that
--    decision auditable long after the sweep that made it.
-- 2. A submission channel carries its own cost, and the cost decides the class
--    of every pitch through it. `paid_placement` exists as a value so it can be
--    recorded and refused, never so it can be used.
--
-- Refusals are rows, not deletions. A candidate that has been screened and
-- rejected must stay visible, or the next sweep rediscovers it, re-screens it
-- and re-refuses it every week for ever.

CREATE TABLE viryaos_outreach_submission_channels (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (btrim(slug) <> '' AND char_length(slug) <= 80),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 200),
    -- Free is third-party contact, credit and fee are spend, and paid placement
    -- is never usable at any autonomy level.
    cost_model text NOT NULL CHECK (cost_model IN ('free', 'credit', 'fee', 'paid_placement')),
    submission_url text CHECK (submission_url IS NULL OR char_length(submission_url) <= 2048),
    active boolean NOT NULL DEFAULT true,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug)
);
CREATE TRIGGER viryaos_outreach_submission_channels_set_updated_at
BEFORE UPDATE ON viryaos_outreach_submission_channels
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_outreach_candidates (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_kind text NOT NULL CHECK (target_kind IN (
        'playlist','radio','press','creator','support_slot','endorsement','media_patronage'
    )),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 200),
    source text NOT NULL CHECK (source IN (
        'playlist_description', 'curator_site', 'submission_channel', 'reply',
        'operator_import', 'scene_adjacent_playlist'
    )),
    -- The playlist, page or message the route was read out of. Kept so a source
    -- that turns out to be bad can be revoked wholesale.
    source_reference text NOT NULL CHECK (
        btrim(source_reference) <> '' AND char_length(source_reference) <= 2048
    ),
    -- The published text the route was read from, verbatim and bounded. Without
    -- it no human can check the extraction later, so a candidate without it is
    -- refused rather than stored as an assertion.
    evidence text CHECK (evidence IS NULL OR char_length(evidence) <= 4000),
    route_kind text NOT NULL CHECK (route_kind IN ('email', 'submission_form', 'handle')),
    route_value text NOT NULL CHECK (
        btrim(route_value) <> '' AND char_length(route_value) <= 2048
    ),
    -- False means somebody worked the route out. Such a candidate is stored
    -- refused, never pitched, and never silently dropped.
    route_is_published boolean NOT NULL,
    channel_id uuid,
    fit_basis_points integer NOT NULL DEFAULT 0 CHECK (fit_basis_points BETWEEN 0 AND 10000),
    follower_count integer CHECK (follower_count IS NULL OR follower_count >= 0),
    engagement_count integer CHECK (engagement_count IS NULL OR engagement_count >= 0),
    sells_placement boolean NOT NULL DEFAULT false,
    churns_indiscriminately boolean NOT NULL DEFAULT false,
    status text NOT NULL DEFAULT 'screening' CHECK (status IN (
        -- Admitted and waiting for an operator to confirm the route.
        'admitted',
        -- Admitted, confirmed, and now a row in viryaos_outreach_targets.
        'promoted',
        -- Screened out, with the reason. Terminal, and deliberately kept.
        'refused',
        -- Ingested but not yet screened. Only ever transient.
        'screening'
    )),
    refusal_reason text CHECK (refusal_reason IS NULL OR refusal_reason IN (
        'route_inferred', 'evidence_missing', 'paid_placement', 'sells_placement',
        'implausible_engagement', 'indiscriminate_churn', 'poor_fit', 'too_small'
    )),
    -- The class a pitch through this route would carry. NULL while refused,
    -- because a refused candidate has no pitch.
    pitch_class text CHECK (pitch_class IS NULL OR pitch_class IN (
        'first_party_reversible', 'owned_audience', 'third_party', 'paid'
    )),
    promoted_target_id uuid,
    screened_at timestamptz,
    promoted_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((status = 'refused') = (refusal_reason IS NOT NULL)),
    CHECK ((status = 'promoted') = (promoted_target_id IS NOT NULL)),
    UNIQUE (workspace_id, id),
    -- Contact identity, not row identity: the same address found twice through
    -- two sources is one candidate, and re-finding it must not re-screen it.
    UNIQUE (workspace_id, route_kind, route_value),
    FOREIGN KEY (workspace_id, channel_id)
        REFERENCES viryaos_outreach_submission_channels (workspace_id, id) ON DELETE SET NULL,
    FOREIGN KEY (workspace_id, promoted_target_id)
        REFERENCES viryaos_outreach_targets (workspace_id, id) ON DELETE SET NULL
);
CREATE TRIGGER viryaos_outreach_candidates_set_updated_at
BEFORE UPDATE ON viryaos_outreach_candidates
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The operator queue: what is admitted and waiting for a human to confirm the
-- route, newest evidence first.
CREATE INDEX viryaos_outreach_candidates_queue_idx
    ON viryaos_outreach_candidates (workspace_id, status, fit_basis_points DESC, created_at DESC);
-- Revoking a bad source wholesale.
CREATE INDEX viryaos_outreach_candidates_source_idx
    ON viryaos_outreach_candidates (workspace_id, source, status);

-- Where a promoted target came from. The target table predates discovery and
-- holds operator-owned rows, so provenance is nullable: NULL means the operator
-- put it there by hand, which is a legitimate answer.
ALTER TABLE viryaos_outreach_targets
    ADD COLUMN IF NOT EXISTS discovered_from_candidate_id uuid;
