-- Native Latarnik session attribution.
--
-- Invite capabilities remain one-way hashes and never enter durable campaign
-- telemetry. A queued invite job may be associated with the pending profile
-- only long enough for exchange to copy that attribution onto the revocable
-- device session. This lets operators measure web/native activation and later
-- relationship outcomes without tracking mail opens or persisting raw links.

ALTER TABLE viryaos_beacon_signal_profiles
    ADD COLUMN pending_invite_job_id uuid;

ALTER TABLE viryaos_beacon_signal_profiles
    ADD CONSTRAINT viryaos_beacon_signal_profiles_pending_invite_job_fk
        FOREIGN KEY (workspace_id, pending_invite_job_id)
        REFERENCES viryaos_beacon_invite_delivery_jobs (workspace_id, id);

ALTER TABLE viryaos_beacon_signal_sessions
    ADD COLUMN client_kind text NOT NULL DEFAULT 'unknown'
        CHECK (client_kind IN ('unknown','web','android','ios')),
    ADD COLUMN source_invite_job_id uuid;

ALTER TABLE viryaos_beacon_signal_sessions
    ADD CONSTRAINT viryaos_beacon_signal_sessions_source_invite_job_fk
        FOREIGN KEY (workspace_id, source_invite_job_id)
        REFERENCES viryaos_beacon_invite_delivery_jobs (workspace_id, id);

CREATE INDEX viryaos_beacon_signal_sessions_invite_attribution_idx
    ON viryaos_beacon_signal_sessions
       (workspace_id, source_invite_job_id, client_kind, created_at DESC, beacon_id)
    WHERE source_invite_job_id IS NOT NULL;

CREATE INDEX viryaos_beacon_signal_profiles_pending_invite_job_idx
    ON viryaos_beacon_signal_profiles (workspace_id, pending_invite_job_id, beacon_id)
    WHERE pending_invite_job_id IS NOT NULL;
