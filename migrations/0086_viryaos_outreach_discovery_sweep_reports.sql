-- What the sweep actually read, as opposed to what it produced.
--
-- The supply rule already tells two failure modes apart: an adapter that never
-- answered (an integration failure, which must not stop the agent asking) and
-- an adapter that answered with nothing admissible (a barren sweep, which after
-- `barren_sweep_limit` does stop it). That distinction is real and it is not
-- enough, because it cannot see the third case, and the third case is the one
-- production is in.
--
-- In production the discovery sweep answered, reported zero candidates, and was
-- counted as barren. Two more like it and the agent concludes `SourceExhausted`
-- and stops. But "we searched Spotify, read two hundred playlists and not one
-- published a submission route" and "we searched Spotify and got back nothing
-- at all, because the credential is dead" produce byte-identical answers. The
-- first is a dry source and widening it is an operator decision. The second is
-- a broken integration, and telling an operator their source is exhausted when
-- their credential expired sends them to fix the wrong thing.
--
-- So the adapter reports what it read. Deliberately a separate table rather
-- than columns on `operator_actions`: that ledger is generic, shared by every
-- operator mutation in the system, and one adapter's telemetry has no business
-- widening it.
--
-- The counts are adapter claims, not verified facts, and the rule treats them
-- as such — they can only ever change *which* hold is reported, never widen
-- authority, raise a cap, or let a candidate skip screening.

CREATE TABLE viryaos_outreach_discovery_sweep_reports (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The ingestion this report describes, so a report can be read back
    -- alongside the batch it arrived with rather than only by timestamp.
    operation_id uuid NOT NULL,
    -- How many sources the adapter queried, e.g. one per configured search.
    sources_read integer NOT NULL CHECK (sources_read >= 0 AND sources_read <= 1000),
    -- How many items those sources returned before any screening. Zero here
    -- with a successful sweep is the signature of a broken read path.
    items_seen integer NOT NULL CHECK (items_seen >= 0 AND items_seen <= 100000),
    -- What the adapter chose to report out of `items_seen`. Kept so the drop
    -- between reading and reporting is visible without recomputing it.
    candidates_reported integer NOT NULL CHECK (candidates_reported >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    -- One report per ingestion. A replayed batch must not append a second.
    UNIQUE (workspace_id, operation_id)
);

CREATE INDEX viryaos_outreach_discovery_sweep_reports_recent_idx
    ON viryaos_outreach_discovery_sweep_reports (workspace_id, created_at DESC, id DESC);
