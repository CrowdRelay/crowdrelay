-- Automated, auditable referral-weighted draws and asynchronous event ingestion.
--
-- Draws are opt-in at runtime through CROWDRELAY_RANDOM_DRAWS_ENABLED and
-- individually scheduled in reward_draws. Every run snapshots candidate weights,
-- exposes the completed random seed for reproducibility, and records winners.

CREATE TABLE event_sources (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    provider text NOT NULL CHECK (provider IN ('bandsintown')),
    artist_name text NOT NULL CHECK (btrim(artist_name) <> '' AND char_length(artist_name) <= 200),
    app_id text NOT NULL CHECK (btrim(app_id) <> '' AND char_length(app_id) <= 200),
    default_country_code char(2) NOT NULL DEFAULT 'PL' CHECK (default_country_code ~ '^[A-Z]{2}$'),
    timezone text NOT NULL DEFAULT 'Europe/Warsaw'
        CHECK (btrim(timezone) <> '' AND char_length(timezone) <= 128),
    sync_interval_seconds integer NOT NULL DEFAULT 1800
        CHECK (sync_interval_seconds BETWEEN 300 AND 86400),
    active boolean NOT NULL DEFAULT true,
    next_sync_at timestamptz NOT NULL DEFAULT now(),
    sync_lease_until timestamptz,
    sync_lease_owner uuid,
    last_started_at timestamptz,
    last_synced_at timestamptz,
    last_success_at timestamptz,
    consecutive_failures integer NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    consecutive_empty_syncs integer NOT NULL DEFAULT 0 CHECK (consecutive_empty_syncs >= 0),
    last_error text CHECK (last_error IS NULL OR char_length(last_error) <= 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, provider, artist_name),
    CHECK (
        (sync_lease_until IS NULL AND sync_lease_owner IS NULL)
        OR (sync_lease_until IS NOT NULL AND sync_lease_owner IS NOT NULL AND last_started_at IS NOT NULL)
    )
);

CREATE TRIGGER event_sources_set_updated_at
BEFORE UPDATE ON event_sources
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX event_sources_due_idx
    ON event_sources (next_sync_at, id)
    WHERE active;

ALTER TABLE events
    ADD COLUMN source_id uuid,
    ADD COLUMN source_provider text,
    ADD COLUMN source_event_id text,
    ADD COLUMN source_last_seen_at timestamptz,
    ADD CONSTRAINT events_source_fk
        FOREIGN KEY (workspace_id, source_id)
        REFERENCES event_sources (workspace_id, id)
        ON DELETE RESTRICT,
    ADD CONSTRAINT events_source_identity_check CHECK (
        (
            source_id IS NULL
            AND source_provider IS NULL
            AND source_event_id IS NULL
            AND source_last_seen_at IS NULL
        )
        OR (
            source_id IS NOT NULL
            AND source_provider IS NOT NULL
            AND btrim(source_provider) <> ''
            AND source_event_id IS NOT NULL
            AND btrim(source_event_id) <> ''
            AND source_last_seen_at IS NOT NULL
        )
    );

CREATE UNIQUE INDEX events_external_source_unique
    ON events (workspace_id, source_id, source_event_id)
    WHERE source_id IS NOT NULL;

ALTER TABLE admission_passes
    DROP CONSTRAINT admission_passes_issuance_method_check,
    ADD CONSTRAINT admission_passes_issuance_method_check
        CHECK (issuance_method IN ('manual', 'deterministic_reward', 'first_come', 'weighted_draw'));

CREATE TABLE reward_draws (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 200),
    prize_kind text NOT NULL CHECK (prize_kind IN ('admission_pass', 'physical_item')),
    eligibility_kind text NOT NULL DEFAULT 'all_active'
        CHECK (eligibility_kind IN ('all_active', 'event_interest')),
    event_id uuid,
    admission_pool_id uuid,
    reward_rule_id uuid,
    winner_count integer NOT NULL CHECK (winner_count BETWEEN 1 AND 10000),
    base_entries integer NOT NULL DEFAULT 1 CHECK (base_entries BETWEEN 1 AND 100000),
    entries_per_referral integer NOT NULL DEFAULT 1 CHECK (entries_per_referral BETWEEN 0 AND 100000),
    max_entries integer NOT NULL DEFAULT 1000 CHECK (max_entries BETWEEN 1 AND 1000000),
    claim_expires_hours integer NOT NULL DEFAULT 168 CHECK (claim_expires_hours BETWEEN 1 AND 8760),
    opens_at timestamptz NOT NULL,
    closes_at timestamptz NOT NULL,
    draw_at timestamptz NOT NULL,
    status text NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'scheduled', 'running', 'completed', 'cancelled')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error text CHECK (last_error IS NULL OR char_length(last_error) <= 500),
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug),
    CONSTRAINT reward_draws_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draws_pool_event_fk
        FOREIGN KEY (workspace_id, admission_pool_id, event_id)
        REFERENCES admission_pools (workspace_id, id, event_id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draws_rule_fk
        FOREIGN KEY (workspace_id, reward_rule_id)
        REFERENCES reward_rules (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (opens_at < closes_at AND closes_at <= draw_at),
    CHECK (max_entries >= base_entries),
    CHECK (eligibility_kind <> 'event_interest' OR event_id IS NOT NULL),
    CHECK (
        (prize_kind = 'admission_pass' AND event_id IS NOT NULL AND admission_pool_id IS NOT NULL AND reward_rule_id IS NULL)
        OR
        (prize_kind = 'physical_item' AND admission_pool_id IS NULL AND reward_rule_id IS NOT NULL)
    ),
    CHECK ((status = 'completed') = (completed_at IS NOT NULL))
);

CREATE TRIGGER reward_draws_set_updated_at
BEFORE UPDATE ON reward_draws
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX reward_draws_due_idx
    ON reward_draws (draw_at, id)
    WHERE status = 'scheduled';

CREATE INDEX reward_draws_active_public_idx
    ON reward_draws (workspace_id, closes_at, id)
    WHERE status IN ('scheduled', 'running');

CREATE TABLE reward_draw_runs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    draw_id uuid NOT NULL,
    algorithm_version text NOT NULL CHECK (btrim(algorithm_version) <> ''),
    seed_hash bytea NOT NULL CHECK (octet_length(seed_hash) = 32),
    revealed_seed_hex text CHECK (revealed_seed_hex IS NULL OR revealed_seed_hex ~ '^[0-9a-f]{64}$'),
    eligible_count integer NOT NULL DEFAULT 0 CHECK (eligible_count >= 0),
    total_entries bigint NOT NULL DEFAULT 0 CHECK (total_entries >= 0),
    requested_winners integer NOT NULL CHECK (requested_winners > 0),
    selected_winners integer NOT NULL DEFAULT 0 CHECK (selected_winners >= 0),
    status text NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    started_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    failure_kind text,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, draw_id, id),
    CONSTRAINT reward_draw_runs_draw_fk
        FOREIGN KEY (workspace_id, draw_id)
        REFERENCES reward_draws (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (selected_winners <= requested_winners),
    CHECK (
        (status = 'running' AND completed_at IS NULL AND revealed_seed_hex IS NULL)
        OR (status = 'completed' AND completed_at IS NOT NULL AND revealed_seed_hex IS NOT NULL)
        OR (status = 'failed' AND completed_at IS NOT NULL)
    )
);

CREATE INDEX reward_draw_runs_draw_time_idx
    ON reward_draw_runs (workspace_id, draw_id, started_at DESC);

CREATE TABLE reward_draw_candidates (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    draw_id uuid NOT NULL,
    run_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    qualified_referrals integer NOT NULL CHECK (qualified_referrals >= 0),
    entry_count integer NOT NULL CHECK (entry_count > 0),
    selection_score double precision NOT NULL CHECK (
        selection_score > 0 AND selection_score < 'Infinity'::double precision
    ),
    selected boolean NOT NULL DEFAULT false,
    winner_rank integer CHECK (winner_rank IS NULL OR winner_rank > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, run_id, fan_id),
    CONSTRAINT reward_draw_candidates_run_fk
        FOREIGN KEY (workspace_id, draw_id, run_id)
        REFERENCES reward_draw_runs (workspace_id, draw_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draw_candidates_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (selected = (winner_rank IS NOT NULL))
);

CREATE INDEX reward_draw_candidates_rank_idx
    ON reward_draw_candidates (workspace_id, run_id, selection_score, fan_id);

CREATE TABLE reward_draw_winners (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    draw_id uuid NOT NULL,
    run_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    winner_rank integer NOT NULL CHECK (winner_rank > 0),
    admission_pass_id uuid,
    reward_grant_id uuid,
    selected_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, draw_id, fan_id),
    UNIQUE (workspace_id, run_id, winner_rank),
    CONSTRAINT reward_draw_winners_run_fk
        FOREIGN KEY (workspace_id, draw_id, run_id)
        REFERENCES reward_draw_runs (workspace_id, draw_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draw_winners_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draw_winners_pass_fk
        FOREIGN KEY (workspace_id, admission_pass_id)
        REFERENCES admission_passes (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT reward_draw_winners_grant_fk
        FOREIGN KEY (workspace_id, reward_grant_id)
        REFERENCES reward_grants (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK ((admission_pass_id IS NOT NULL)::integer + (reward_grant_id IS NOT NULL)::integer = 1)
);

CREATE INDEX reward_draw_winners_fan_time_idx
    ON reward_draw_winners (workspace_id, fan_id, selected_at DESC);
