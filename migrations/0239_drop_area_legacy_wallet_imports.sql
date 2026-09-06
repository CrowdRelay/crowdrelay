-- The one-time legacy AREA wallet import is complete.
-- Drop the tracking table; the import routes and feature flag were removed in code.
DROP TABLE IF EXISTS area_legacy_wallet_imports;
