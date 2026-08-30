-- Add 'fanbase_connection' as an allowed subject_kind for growth metric series.
--
-- Migration 0172 added the growth_metric_sync worker (YouTube subscriber
-- counts) and the provider_account_id column on fanbase_connections, but did
-- not update the subject_kind check constraint. The worker inserts series
-- with subject_kind = 'fanbase_connection' (see growth_metric_sync.rs
-- record_metric_point), which the constraint rejected — so every YouTube
-- sync silently failed with a check-constraint violation.

ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT viryaos_growth_metric_series_subject_kind_check;

ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_subject_kind_check CHECK (
        subject_kind IS NULL OR subject_kind IN (
            'event', 'city', 'release_plan', 'content_source', 'beacon',
            'event_source', 'fanbase_connection'
        )
    );
