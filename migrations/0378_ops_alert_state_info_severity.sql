-- Info-severity watchdog findings.
--
-- `video.unmeasured` is a note, not an alarm: a workspace with videos to
-- measure and no YouTube Analytics grant should see that gap named in the
-- attention feed without it wearing warning colours. The alert-state CHECK
-- admitted only warning and critical, so the finding could not exist as
-- written. Widening adds a value; nothing existing is refused.

ALTER TABLE ops_alert_state
    DROP CONSTRAINT IF EXISTS ops_alert_state_severity_check,
    ADD CONSTRAINT ops_alert_state_severity_check
        CHECK (severity IN ('info', 'warning', 'critical'));
