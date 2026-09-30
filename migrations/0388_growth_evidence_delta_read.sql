-- Match the current five-timestamp cursor and eligible-row predicate.
-- The older three-timestamp/COALESCE index cannot serve this expression.
CREATE INDEX IF NOT EXISTS growth_evidence_delta_cursor_idx
    ON growth_evidence (
        workspace_id,
        GREATEST(resolved_at, replayed_3d_at, replayed_14d_at,
                 replayed_30d_at, last_partial_resolution_at), timestamp
    )
    WHERE resolved_at IS NOT NULL OR COALESCE(partial_resolution_count, 0) > 0;
