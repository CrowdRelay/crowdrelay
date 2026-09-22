-- North Star regime shifts on the fan-source snapshots (WHAT_CHANGED_PLAN.md).
--
-- detect_fan_growth_shifts already runs every cycle over the daily North Star
-- series and its answer used to die in a log line — one regime shift out of
-- many, readable only inside a container. Persisting it on the snapshot row
-- keeps the two halves of the operator sentence in one read: the attribution
-- says who produced fans, this says when the rate changed.
--
-- A column on fan_source_snapshots rather than a second table: same writer,
-- same hourly cadence, same workspace scope — one read serves both.
--
-- Each stored shift carries a civil `date` resolved at write time, not the
-- detector's `timestamp` field — that field is a zero-based observation index
-- despite its name, and persisting it raw would invite a UI to render an
-- index as a date.
ALTER TABLE fan_source_snapshots
    ADD COLUMN IF NOT EXISTS north_star_shifts jsonb NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(north_star_shifts) = 'array');
