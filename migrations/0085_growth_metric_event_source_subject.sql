-- The first external eye.
--
-- `growth_metrics` has been live and honest since migration 0073, and every
-- series in it measures something CrowdRelay already owns: tickets, signal
-- sessions, merch orders. That is a till, not a reach signal. The agent can
-- see whether people bought, and cannot see whether anybody new is listening,
-- which means it cannot tell a play that worked from a play that did nothing.
--
-- Bandsintown tracker counts are the cheapest honest fix. The credential is
-- already configured for the event sync, the number is public, and a tracker
-- is a person who asked to be told about gigs — closer to intent than a
-- follower count, which is why it lands as `intermediate` and not `vanity`.
--
-- The series belongs to the event source it was read through, not to the
-- workspace: a workspace may sync more than one artist, and a single
-- workspace-scoped `bandsintown/trackers` series would silently interleave two
-- artists' numbers into one timeline. `subject_kind` is a loose reference by
-- design (see 0073) so no foreign key is added here; the column just needs to
-- admit the kind.

ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT viryaos_growth_metric_series_subject_kind_check;

ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_subject_kind_check CHECK (
        subject_kind IS NULL OR subject_kind IN (
            'event', 'city', 'release_plan', 'content_source', 'beacon', 'event_source'
        )
    );
