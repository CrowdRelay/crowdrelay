-- The activation funnel already knew who these installs belong to.
--
-- `signal_installations.fan_id` got its first writer in the push-endpoint
-- registration, which is the only request holding both a fan session and the
-- app's own `installation_id`. That fix is forward-only: it fires when a fan
-- registers for push, so every registration that already happened stays
-- anonymous.
--
-- Production measured the consequence immediately after that shipped:
--
--   crowdrelay_brain_signal_installs            1
--   crowdrelay_brain_signal_installs_identified 0
--   crowdrelay_brain_signal_fans_push_enabled   2
--
-- Two fans are reachable by push. Every one of them registered from an
-- installation and named it in the request, and `fan_push_endpoints` has
-- recorded `(workspace_id, installation_id, fan_id)` since migration 0051. So
-- the join the funnel needs is not missing — it is sitting in the next table,
-- and the numerator reads zero anyway.
--
-- This closes that gap once, from the data already stored. New installs need no
-- backfill; they identify themselves through the endpoint.
--
-- Three properties, each deliberate:
--
--   * `fan_id IS NULL` only. The column answers "did this install ever
--     convert", and an install that already has an answer keeps it. That also
--     makes this re-runnable, which matters because a migration that corrects
--     data has to be safe to apply to a database somebody already corrected by
--     hand.
--   * Earliest endpoint wins, ordered by `created_at` then `id` so the choice
--     is total and not merely likely. This is the same rule the live write
--     follows — the first identification is the conversion — applied to
--     history.
--   * The workspace is named on both sides of the join. It is the whole of the
--     tenant isolation here, and an installation identifier is generated on a
--     device with no knowledge of which workspace it will report to, so two
--     tenants sharing one string is possible rather than merely theoretical.
--
-- It does NOT insert install rows for endpoints whose installation was never
-- reported. Those exist: the reporting code is newer than the app builds that
-- registered for push, so a fan can be reachable with no install row at all.
-- Inventing the row would raise the denominator with devices nobody observed
-- installing and make the funnel read worse than the truth in the other
-- direction. The honest reading is that the top of the funnel is still filling
-- in as those copies of the app relaunch.

UPDATE signal_installations AS installations
SET fan_id = earliest.fan_id
FROM (
  SELECT DISTINCT ON (endpoints.workspace_id, endpoints.installation_id)
         endpoints.workspace_id,
         endpoints.installation_id,
         endpoints.fan_id
  FROM fan_push_endpoints AS endpoints
  ORDER BY endpoints.workspace_id,
           endpoints.installation_id,
           endpoints.created_at,
           endpoints.id
) AS earliest
WHERE installations.workspace_id = earliest.workspace_id
  AND installations.installation_id = earliest.installation_id
  AND installations.fan_id IS NULL;

-- Deliberately not filtered to `active` endpoints. A fan who registered for
-- push and later disabled it did identify that install, and the funnel records
-- that it converted, not that it is still converting. Reachability is
-- `fan_push_endpoints.active`, which this does not touch.
