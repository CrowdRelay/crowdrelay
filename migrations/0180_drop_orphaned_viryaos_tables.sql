-- Drop orphaned viryaos_* tables that were created by migrations but are
-- never referenced in any Rust code. All 6 tables are empty in production.
--
-- These tables were part of an early attribution/causal inference design
-- that was superseded by the viryaos_reach_events + viryaos_reach_conversions
-- model. The schema was created but the write paths were never wired up.
--
-- CASCADE drops dependent indexes/constraints that reference these tables.

DROP TABLE IF EXISTS viryaos_opportunity_episodes CASCADE;
DROP TABLE IF EXISTS viryaos_episode_events CASCADE;
DROP TABLE IF EXISTS viryaos_audience_exposures CASCADE;
DROP TABLE IF EXISTS viryaos_propensity_log CASCADE;
DROP TABLE IF EXISTS viryaos_causal_estimates CASCADE;
DROP TABLE IF EXISTS viryaos_fan_attribution CASCADE;
