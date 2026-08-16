-- Latarnik network acquisition remains a thin workflow layer over the existing
-- `viryaos_beacons` relationship source of truth. Discovery runs may propose
-- public-source candidates, but they never confer verification or marketing
-- permission. Invitation delivery is claimed exactly once so raw Signal invite
-- capabilities never need to be stored in the transactional outbox.

CREATE TABLE viryaos_beacon_network_discovery_runs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    country_code text NOT NULL DEFAULT 'PL' CHECK (country_code ~ '^[A-Z]{2}$'),
    target_count integer NOT NULL DEFAULT 100 CHECK (target_count BETWEEN 1 AND 500),
    status text NOT NULL DEFAULT 'requested' CHECK (status IN (
        'requested','running','ready','failed','cancelled'
    )),
    discovered_count integer NOT NULL DEFAULT 0 CHECK (discovered_count >= 0),
    report_filename text CHECK (
        report_filename IS NULL OR (char_length(report_filename) BETWEEN 1 AND 240)
    ),
    report_sha256 text CHECK (
        report_sha256 IS NULL OR report_sha256 ~ '^[0-9a-f]{64}$'
    ),
    requested_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    completed_at timestamptz,
    failure_kind text CHECK (failure_kind IS NULL OR char_length(failure_kind) <= 96),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);
CREATE TRIGGER viryaos_beacon_network_discovery_runs_set_updated_at
BEFORE UPDATE ON viryaos_beacon_network_discovery_runs
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_beacon_network_discovery_runs_recent_idx
    ON viryaos_beacon_network_discovery_runs (workspace_id, requested_at DESC, id DESC);

CREATE TABLE viryaos_beacon_invite_delivery_jobs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    beacon_ids uuid[] NOT NULL CHECK (
        array_length(beacon_ids, 1) BETWEEN 1 AND 200
    ),
    ttl_days integer NOT NULL DEFAULT 14 CHECK (ttl_days BETWEEN 1 AND 30),
    radius_km integer NOT NULL DEFAULT 100 CHECK (radius_km BETWEEN 1 AND 500),
    locale text NOT NULL DEFAULT 'pl' CHECK (locale ~ '^[a-z]{2}(-[A-Z]{2})?$'),
    status text NOT NULL DEFAULT 'queued' CHECK (status IN (
        'queued','claimed','completed','failed','ambiguous','cancelled'
    )),
    claim_token_hash bytea,
    claimed_by text CHECK (claimed_by IS NULL OR char_length(claimed_by) <= 120),
    claimed_at timestamptz,
    reported_at timestamptz,
    provider_summary jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(provider_summary)='object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    CHECK (
        (status='queued' AND claim_token_hash IS NULL AND claimed_at IS NULL)
        OR (status<>'queued' AND claim_token_hash IS NOT NULL AND claimed_at IS NOT NULL)
        OR status='cancelled'
    )
);
CREATE TRIGGER viryaos_beacon_invite_delivery_jobs_set_updated_at
BEFORE UPDATE ON viryaos_beacon_invite_delivery_jobs
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_beacon_invite_delivery_jobs_recent_idx
    ON viryaos_beacon_invite_delivery_jobs (workspace_id, status, created_at DESC, id DESC);

INSERT INTO viryaos_manager_config (workspace_id, config_key, value)
SELECT id, 'beacon_network_policy', jsonb_build_object(
    'discoveryCountry', 'PL',
    'publicSourcesOnly', true,
    'humanReviewRequired', true,
    'marketingEmailConsentRequired', true,
    'rawInviteCapabilitiesInOutbox', false,
    'maxInviteBatch', 200
)
FROM workspaces
ON CONFLICT (workspace_id, config_key) DO NOTHING;
