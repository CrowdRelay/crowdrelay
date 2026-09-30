-- Run on native PostgreSQL with psql -v ON_ERROR_STOP=1 -f this-file.sql.
-- Synthetic range-plan comparison, not a full-cycle capacity/SLO test.
BEGIN;
CREATE TEMP TABLE learning_scale (
    workspace_id uuid NOT NULL,
    timestamp timestamptz NOT NULL,
    resolved_at timestamptz,
    replayed_3d_at timestamptz,
    replayed_14d_at timestamptz,
    replayed_30d_at timestamptz,
    last_partial_resolution_at timestamptz,
    partial_resolution_count integer
);
INSERT INTO learning_scale(workspace_id,timestamp,resolved_at)
SELECT '00000000-0000-0000-0000-000000000001',
       now()-interval '20 days',now()-interval '10 days'
FROM generate_series(1,100000);
-- Shape of the old cursor expression index.
CREATE INDEX old_learning_cursor ON learning_scale (
    workspace_id, COALESCE(replayed_30d_at,replayed_14d_at,replayed_3d_at)
) WHERE resolved_at IS NOT NULL;
ANALYZE learning_scale;
SET LOCAL plan_cache_mode = force_generic_plan;
PREPARE old_learning_delta(uuid,timestamptz) AS
SELECT * FROM learning_scale
WHERE workspace_id=$1
  AND (resolved_at IS NOT NULL OR COALESCE(partial_resolution_count,0)>0)
  AND COALESCE(resolved_at,last_partial_resolution_at)>=now()-interval '180 days'
  AND CASE WHEN $2 IS NULL THEN true ELSE
      GREATEST(resolved_at,replayed_3d_at,replayed_14d_at,replayed_30d_at,
               last_partial_resolution_at)>$2 END;
EXPLAIN (ANALYZE,BUFFERS)
EXECUTE old_learning_delta('00000000-0000-0000-0000-000000000001',now()-interval '2 days');
-- Shape and predicate of migration 0388.
CREATE INDEX new_learning_cursor ON learning_scale (
    workspace_id,
    GREATEST(resolved_at,replayed_3d_at,replayed_14d_at,replayed_30d_at,
             last_partial_resolution_at), timestamp
) WHERE resolved_at IS NOT NULL OR COALESCE(partial_resolution_count,0)>0;
ANALYZE learning_scale;
PREPARE new_learning_delta(uuid,timestamptz) AS
SELECT * FROM learning_scale
WHERE workspace_id=$1
  AND (resolved_at IS NOT NULL OR COALESCE(partial_resolution_count,0)>0)
  AND COALESCE(resolved_at,last_partial_resolution_at)>=now()-interval '180 days'
  AND GREATEST(resolved_at,replayed_3d_at,replayed_14d_at,replayed_30d_at,
               last_partial_resolution_at)>$2;
-- Expect a range condition including workspace AND cursor; zero result rows.
EXPLAIN (ANALYZE,BUFFERS)
EXECUTE new_learning_delta('00000000-0000-0000-0000-000000000001',now()-interval '2 days');
INSERT INTO learning_scale(workspace_id,timestamp,resolved_at)
VALUES('00000000-0000-0000-0000-000000000001',now(),now());
-- Same prepared plan, one new result row, no full old-history scan.
EXPLAIN (ANALYZE,BUFFERS)
EXECUTE new_learning_delta('00000000-0000-0000-0000-000000000001',now()-interval '2 days');
DEALLOCATE old_learning_delta;
DEALLOCATE new_learning_delta;
ROLLBACK;
