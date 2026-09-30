-- The system-owned signal/activated_fans_30d series used a predicate that
-- drifted from fan_activation_kpi.activated_30d:
--
--   * the KPI is an activation cohort: signed up in the last 30 days,
--     latest marketing consent granted, and meaningful activity within
--     30 days of signup;
--   * the series writer instead counted any active, consented fan whose
--     latest meaningful action happened in the last 30 days.
--
-- Those are different populations. Keeping the old points would make a
-- definition correction look like a real audience drop or jump and would
-- contaminate 24d/28d trend reads for up to the retention horizon.
--
-- A wrong measurement is worse than a missing one. The cycle ledger keeps
-- the canonical North Star readings used by the brain, so monthly target
-- progress does not lose its trustworthy history. The next worker cycle
-- immediately writes fresh workspace and city points with the corrected
-- predicate.
DELETE FROM growth_metric_points AS point
USING growth_metric_series AS series
WHERE point.workspace_id = series.workspace_id
  AND point.series_id = series.id
  AND series.platform = 'signal'
  AND series.metric_key = 'activated_fans_30d';
