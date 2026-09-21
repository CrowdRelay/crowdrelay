-- Every measured outcome becomes learnable, not just fan growth.
--
-- Until now a measurement resolved into `viryaos_autopilot_outcomes` and, for
-- five kinds only, into a typed column here (`observed_fans`,
-- `observed_incremental_fans`, `observed_incremental_fans_3d`,
-- `durable_fans_30d`, `observed_engagement`). Ticket revenue, merch gross,
-- promotion ROAS, booking replies, outreach replies, show clicks, attributed
-- orders, grassroots replies, lifecycle engagement and every quality
-- checkpoint all landed in the catch-all arm — stored, assessed, and never
-- read by the causal model. A lever that kept failing to move ticket revenue
-- kept its ranking forever, because nothing it produced could reach a
-- posterior.
--
-- `observed_metrics` is the general write-back: one JSONB map of
-- metric key to observed value, first-writer-wins per key, one row still
-- carrying everything a dispatch produced. Replay folds each key into the
-- metric posteriors on the causal model at full resolution.
--
-- The value a metric key names is the raw observed value in that kind's
-- natural units (minor currency units, counts, basis points) — never a
-- derived delta. Derivation belongs to the learner, where it can be
-- revised; a stored delta cannot be un-derived.
ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS observed_metrics jsonb NOT NULL DEFAULT '{}'::jsonb;

-- The prediction half of the same pair: what the brain expected each metric
-- to be, recorded at dispatch so a metric outcome can be scored as a
-- prediction error and not only as a level.
ALTER TABLE viryaos_dispatch_predictions
    ADD COLUMN IF NOT EXISTS expected_metrics jsonb NOT NULL DEFAULT '{}'::jsonb;
