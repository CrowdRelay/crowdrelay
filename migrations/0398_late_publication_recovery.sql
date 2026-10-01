ALTER TABLE autopilot_measurements ADD COLUMN publication_recovery_checked_at timestamptz;
CREATE INDEX autopilot_late_publication_recovery ON autopilot_measurements(workspace_id,publication_recovery_checked_at NULLS FIRST,finished_at DESC,id)
WHERE status='failed' AND last_error_kind='no_tracked_link';
