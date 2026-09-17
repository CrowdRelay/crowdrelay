-- The moment a slipped assignment was first told about. `last_reminded_at`
-- is the latest reminder and overwrites itself, so "days between slip and
-- told" had no durable answer. Set once, by the reminder sweep, on the first
-- reminder that fires after `due_at`; never updated after.
ALTER TABLE viryaos_team_assignments
    ADD COLUMN IF NOT EXISTS first_overdue_reminder_at timestamptz;
