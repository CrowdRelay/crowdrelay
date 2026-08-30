-- Drop orphaned viryaos_* tables that were created by migrations but are
-- never referenced in any Rust code. All 6 tables are empty in production.
--
-- These tables were part of an early attribution/causal inference design
-- that was superseded by the viryaos_reach_events + viryaos_reach_conversions
-- model. The schema was created but the write paths were never wired up.

DROP TABLE IF EXISTS viryaos_opportunity_episodes;
DROP TABLE IF EXISTS viryaos_episode_events;
DROP TABLE IF EXISTS viryaos_audience_exposures;
DROP TABLE IF EXISTS viryaos_propensity_log;
DROP TABLE IF EXISTS viryaos_causal_estimates;
DROP TABLE IF EXISTS viryaos_fan_attribution;
