-- 4G.4 follow-up: one measurement per recipient, not one per action.
--
-- RequestGigOutreach schedules a booking_reply_7d measurement for every
-- recipient of the letter, keyed by subject_id. The old
-- (workspace_id, action_id, measurement_kind) uniqueness collapsed every
-- recipient after the first into an ON CONFLICT no-op, so a letter to a
-- three-promoter room was measured against one promoter and the track record
-- under-reported it forever. Worse, the outcome insert for the second
-- recipient violated viryaos_autopilot_outcomes_action_metric_uidx — a
-- constraint the ON CONFLICT clause does not name — which raised instead of
-- settling, leaving the measurement stuck in flight.

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT viryaos_autopilot_measurement_workspace_id_action_id_measur_key,
    ADD CONSTRAINT viryaos_autopilot_measurements_action_kind_subject_key
        UNIQUE (workspace_id, action_id, measurement_kind, subject_id);

-- An outcome written for a measurement is already unique on
-- (workspace_id, measurement_id) — one settled observation per measurement.
-- The action-metric uniqueness exists for decision-level outcomes that carry
-- no measurement, so it must not reach the per-recipient ones.
DROP INDEX viryaos_autopilot_outcomes_action_metric_uidx;
CREATE UNIQUE INDEX viryaos_autopilot_outcomes_action_metric_uidx
    ON viryaos_autopilot_outcomes (workspace_id, action_id, metric_key)
    WHERE action_id IS NOT NULL AND measurement_id IS NULL;
