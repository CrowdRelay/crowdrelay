-- Principal-pass hardening for Latarnik network provenance and invitation leases.
-- Discovery provenance is append-only per run/beacon instead of living only in
-- the mutable `viryaos_beacons.metadata` snapshot. Invitation claims acquire a
-- bounded lease so a dead executor cannot leave a job looking healthy forever.

CREATE TABLE viryaos_beacon_network_discovery_observations (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    run_id uuid NOT NULL,
    beacon_id uuid NOT NULL,
    source_url text NOT NULL CHECK (source_url ~ '^https://'),
    source_note text CHECK (source_note IS NULL OR char_length(source_note) BETWEEN 1 AND 2000),
    relevance_basis_points integer NOT NULL CHECK (relevance_basis_points BETWEEN 0 AND 10000),
    confidence_basis_points integer NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    observed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, run_id, beacon_id),
    CONSTRAINT viryaos_beacon_network_observation_run_fk
        FOREIGN KEY (workspace_id, run_id)
        REFERENCES viryaos_beacon_network_discovery_runs (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT viryaos_beacon_network_observation_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id)
        ON DELETE CASCADE
);
CREATE INDEX viryaos_beacon_network_observations_beacon_idx
    ON viryaos_beacon_network_discovery_observations
       (workspace_id, beacon_id, observed_at DESC, run_id DESC);

ALTER TABLE viryaos_beacon_invite_delivery_jobs
    ADD COLUMN claim_expires_at timestamptz;

UPDATE viryaos_beacon_invite_delivery_jobs
SET claim_expires_at = claimed_at + interval '60 minutes'
WHERE claimed_at IS NOT NULL AND claim_expires_at IS NULL;

ALTER TABLE viryaos_beacon_invite_delivery_jobs
    ADD CONSTRAINT viryaos_beacon_invite_delivery_jobs_claim_lease_check
    CHECK (
        (status IN ('queued','cancelled') AND (status='cancelled' OR claim_expires_at IS NULL))
        OR (status IN ('claimed','completed','failed','ambiguous') AND claim_expires_at IS NOT NULL)
    );

CREATE INDEX viryaos_beacon_invite_delivery_jobs_stale_claim_idx
    ON viryaos_beacon_invite_delivery_jobs (claim_expires_at, id)
    WHERE status='claimed';
