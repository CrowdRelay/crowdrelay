-- Synesthesia completion ledger and draw eligibility.
--
-- Additive by design: no existing fan/mail/ticket flow is changed. Gameplay runs
-- are pseudonymous until a player explicitly enters a reward draw. Shipping data
-- is not collected here; only selected winners enter fulfillment later.

CREATE TABLE synesthesia_runs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    campaign_slug text NOT NULL CHECK (campaign_slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    install_hash bytea NOT NULL CHECK (octet_length(install_hash) = 32),
    run_token_hash bytea NOT NULL CHECK (octet_length(run_token_hash) = 32),
    app_version text NOT NULL CHECK (btrim(app_version) <> '' AND char_length(app_version) <= 64),
    locale text CHECK (locale IS NULL OR char_length(locale) <= 35),
    next_room_index smallint NOT NULL DEFAULT 0 CHECK (next_room_index BETWEEN 0 AND 11),
    started_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    client_total_elapsed_ms bigint CHECK (client_total_elapsed_ms IS NULL OR client_total_elapsed_ms >= 0),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, campaign_slug, install_hash),
    CHECK ((completed_at IS NULL) = (client_total_elapsed_ms IS NULL))
);

CREATE TRIGGER synesthesia_runs_set_updated_at
BEFORE UPDATE ON synesthesia_runs
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX synesthesia_runs_campaign_completion_idx
    ON synesthesia_runs (workspace_id, campaign_slug, completed_at, id)
    WHERE completed_at IS NOT NULL;

CREATE TABLE synesthesia_room_completions (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    run_id uuid NOT NULL,
    room_index smallint NOT NULL CHECK (room_index BETWEEN 0 AND 10),
    room_id text NOT NULL CHECK (room_id ~ '^[a-z0-9][a-z0-9-]{0,127}$'),
    client_elapsed_ms bigint NOT NULL CHECK (client_elapsed_ms BETWEEN 1000 AND 7200000),
    recorded_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, run_id, room_index),
    UNIQUE (workspace_id, run_id, room_id),
    CONSTRAINT synesthesia_room_completions_run_fk
        FOREIGN KEY (workspace_id, run_id)
        REFERENCES synesthesia_runs (workspace_id, id)
        ON DELETE CASCADE
);

CREATE TABLE synesthesia_reward_entries (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    campaign_slug text NOT NULL CHECK (campaign_slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    run_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    normalized_email text NOT NULL CHECK (btrim(normalized_email) <> ''),
    policy_version text NOT NULL CHECK (btrim(policy_version) <> '' AND char_length(policy_version) <= 120),
    locale text CHECK (locale IS NULL OR char_length(locale) <= 35),
    entered_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, campaign_slug, run_id),
    UNIQUE (workspace_id, campaign_slug, normalized_email),
    CONSTRAINT synesthesia_reward_entries_run_fk
        FOREIGN KEY (workspace_id, run_id)
        REFERENCES synesthesia_runs (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT synesthesia_reward_entries_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX synesthesia_reward_entries_draw_idx
    ON synesthesia_reward_entries (workspace_id, campaign_slug, entered_at, fan_id);

-- Existing draws remain unchanged. Synesthesia adds one generic eligibility
-- reference so the draw worker can snapshot only completed album participants.
ALTER TABLE reward_draws
    DROP CONSTRAINT reward_draws_eligibility_kind_check;

ALTER TABLE reward_draws
    ADD CONSTRAINT reward_draws_eligibility_kind_check
    CHECK (eligibility_kind IN ('all_active', 'event_interest', 'synesthesia_completion'));

ALTER TABLE reward_draws
    ADD COLUMN eligibility_ref text
        CHECK (eligibility_ref IS NULL OR eligibility_ref ~ '^[a-z0-9][a-z0-9_-]{0,127}$');

ALTER TABLE reward_draws
    ADD CONSTRAINT reward_draws_synesthesia_ref_check
    CHECK (eligibility_kind <> 'synesthesia_completion' OR eligibility_ref IS NOT NULL);

CREATE INDEX reward_draws_synesthesia_eligibility_idx
    ON reward_draws (workspace_id, eligibility_ref, closes_at, id)
    WHERE eligibility_kind = 'synesthesia_completion';

-- One live Synesthesia draw per campaign reference. Completed/cancelled draws do
-- not block a later versioned campaign, while duplicate draft/scheduled setup
-- is rejected before it can reserve a second five-disc pool.
CREATE UNIQUE INDEX reward_draws_synesthesia_live_ref_uidx
    ON reward_draws (workspace_id, eligibility_ref)
    WHERE eligibility_kind = 'synesthesia_completion'
      AND status IN ('draft', 'scheduled', 'running');
