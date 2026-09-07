-- Tenant park flag: when true, the autopilot cycle returns immediately
-- without evaluating or producing anything. Set by the Control Plane when
-- a tenant is parked (billing/non-payment), cleared on resume.
--
-- Distinct from agent_enabled (the kill switch for outward contact) and
-- dry_run (rehearsal): parked stops the entire cycle — no decisions, no
-- first-party work, no measurements — while agent_enabled only holds
-- outward actions. Outbox draining, retention and watchdog are not part
-- of the autopilot cycle and continue regardless.

ALTER TABLE viryaos_growth_envelope
    ADD COLUMN IF NOT EXISTS parked boolean NOT NULL DEFAULT false;
