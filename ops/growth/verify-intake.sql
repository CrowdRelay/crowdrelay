-- Read-only intake and growth reconciliation. No emails, tokens, or message bodies.
-- Run: psql -X "$DATABASE_URL" -v ON_ERROR_STOP=1 -v workspace_slug=virya \
--       -v intake_revision=7 -f ops/growth/verify-intake.sql
\set ON_ERROR_STOP on
\pset pager off
\if :{?workspace_slug}
\else
  DO $$ BEGIN RAISE EXCEPTION 'workspace_slug is required'; END $$;
\endif
\if :{?intake_revision}
\else
  DO $$ BEGIN RAISE EXCEPTION 'intake_revision is required; use SHEET_INTAKE_REVISION from the deployed worker'; END $$;
\endif

BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET LOCAL statement_timeout = '15s';
SET LOCAL lock_timeout = '2s';
SELECT id AS workspace_id FROM workspaces WHERE slug = :'workspace_slug' \gset
-- \gset fails if the tenant does not exist. Never fall back to a global query.

\echo 'Snapshot and connection status'
SELECT now() AS captured_at, :'workspace_slug' AS workspace_slug,
       :'intake_revision' AS expected_intake_revision;
SELECT platform, status, health, last_sync_at, last_sync_failed_at,
       (last_sync_error IS NOT NULL) AS has_sync_error
FROM fanbase_connections
WHERE workspace_id = :'workspace_id' AND platform IN ('gdrive', 'gmail', 'github')
ORDER BY platform;

\echo 'Stored file markers (includes previously scanned files outside the current scope)'
SELECT file_name, rows_read, contacts_written, no_email_column, last_scanned_at,
       split_part(split_part(coalesce(last_mtime, ''), '#', 2), '#', 1)
           = :'intake_revision' AS current_revision,
       position('#refused:' IN coalesce(last_mtime, '')) > 0 AS export_refused
FROM drive_files
WHERE workspace_id = :'workspace_id'
ORDER BY file_name, file_id;

\echo 'Staged contacts are not fans or marketing consent'
SELECT staged_status, fan_outcome, beacon_outcome, suggested_kind,
       count(*) AS contacts,
       count(*) FILTER (WHERE disappeared_at IS NOT NULL) AS disappeared
FROM drive_contacts
WHERE workspace_id = :'workspace_id'
GROUP BY staged_status, fan_outcome, beacon_outcome, suggested_kind
ORDER BY contacts DESC;

\echo 'Outreach eligibility and recorded history'
SELECT target_kind, count(*) AS targets,
       count(*) FILTER (WHERE active AND verified AND accepts_outreach
                       AND NOT do_not_contact) AS eligible
FROM outreach_targets WHERE workspace_id = :'workspace_id'
GROUP BY target_kind ORDER BY target_kind;
SELECT direction, disposition, count(*) AS interactions
FROM outreach_interactions WHERE workspace_id = :'workspace_id'
GROUP BY direction, disposition ORDER BY direction, disposition;

\echo 'Scout opportunities (inventory is not conversion)'
SELECT source, opportunity_kind, status, count(*) AS opportunities,
       count(*) FILTER (WHERE eligible AND deadline > now()) AS eligible_future_deadline
FROM team_opportunities WHERE workspace_id = :'workspace_id'
GROUP BY source, opportunity_kind, status ORDER BY source, opportunity_kind, status;
SELECT count(*) AS festival_editions FROM festival_editions
WHERE workspace_id = :'workspace_id';

\echo 'Actual fan activation and acquisition'
SELECT signups_30d, activated_30d, active_30d, reachable_consented, retained_30d
FROM fan_activation_kpi WHERE workspace_id = :'workspace_id';
SELECT source, count(*) AS arrivals_30d,
       count(*) FILTER (WHERE campaign_id IS NOT NULL) AS campaign_linked_30d
FROM fan_acquisition_events
WHERE workspace_id = :'workspace_id' AND occurred_at > now() - interval '30 days'
GROUP BY source ORDER BY source;

\echo 'Delivery receipts (action success alone is not a receipt)'
SELECT platform, status, count(*) AS posts_7d,
       count(*) FILTER (WHERE removed_by_category IS NOT NULL) AS removed
FROM community_posts
WHERE workspace_id = :'workspace_id' AND created_at > now() - interval '7 days'
GROUP BY platform, status ORDER BY platform, status;
SELECT transport, audience_kind, active, count(*) AS endpoints
FROM fan_push_endpoints WHERE workspace_id = :'workspace_id'
GROUP BY transport, audience_kind, active ORDER BY transport, audience_kind, active;
SELECT audience_kind, status, error_code, count(*) AS deliveries_7d
FROM fan_push_deliveries
WHERE workspace_id = :'workspace_id' AND created_at > now() - interval '7 days'
GROUP BY audience_kind, status, error_code ORDER BY audience_kind, status, error_code;
SELECT status, count(*) AS ticket_orders
FROM ticket_orders WHERE workspace_id = :'workspace_id'
GROUP BY status ORDER BY status;

COMMIT;
