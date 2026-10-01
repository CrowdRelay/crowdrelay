-- Event-network scout pipeline integrity.
--
-- 1) The agents service emits kind='beacon_candidates'; without the value in
--    the CHECK every scout outcome INSERT fails, and because emitOutcomes
--    shares the task-completion transaction the whole run dies as a watchdog
--    timeout — result lost, premium tokens spent, no trace.
-- 2) The scout template and the outcome validator both accept eleven
--    beacon kinds (venue and scene_partner included), but the beacons table
--    CHECK only knew nine — an accepted candidate died at beacon INSERT.
-- 3) `beacon_event_matches` records which scout run matched a beacon to an
--    event. A beacon's metadata carries at most one event_network_scout
--    block; the relationship history across events lives here so the second
--    festival does not overwrite what the first one taught us.

-- ── 1. agent_outcomes accepts the scout's outcome kind ───────────────────
ALTER TABLE agent_outcomes
    DROP CONSTRAINT IF EXISTS agent_outcomes_kind_check,
    ADD CONSTRAINT agent_outcomes_kind_check CHECK (kind IN (
        'press_pitch',
        'social_post',
        'signal_push',
        'audience_segments',
        'outreach_targets',
        'campaign_insight',
        'release_plan_note',
        'generic_insight',
        'opportunity_findings',
        'strategy_proposals',
        'beacon_candidates'
    ));

-- ── 2. beacons accepts the full scout kind vocabulary ────────────────────
-- Adding values is safe: the CHECK only narrows at insert time, and no
-- existing row can violate a superset.
ALTER TABLE beacons
    DROP CONSTRAINT IF EXISTS beacons_beacon_kind_check,
    ADD CONSTRAINT beacons_beacon_kind_check CHECK (beacon_kind IN (
        'radio','local_press','television','reviewer','creator',
        'photographer','promoter','venue','scene_partner','patron','community'
    ));

-- ── 3. Per-event match history ───────────────────────────────────────────
-- One canonical match row per (beacon, event): a promoter that fits next
-- month's show and last month's show is one relationship with a history,
-- not two discoveries. outcome_id keeps the evidence trail back to the
-- agent outcome that made the match; repeats bump matched_at/matched_count
-- without forking identity.
CREATE TABLE beacon_event_matches (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    beacon_id       uuid NOT NULL,
    event_id        uuid NOT NULL,
    outcome_id      uuid REFERENCES agent_outcomes(id) ON DELETE SET NULL,
    matched_at      timestamptz NOT NULL DEFAULT now(),
    matched_count   integer NOT NULL DEFAULT 1 CHECK (matched_count > 0),
    UNIQUE (workspace_id, beacon_id, event_id),
    CONSTRAINT beacon_event_matches_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES beacons (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT beacon_event_matches_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE
);

-- The review queue joins matches per beacon; the event side answers "which
-- partners did scouting find for this show" for the pilot scorecard.
CREATE INDEX beacon_event_matches_beacon_idx
    ON beacon_event_matches (workspace_id, beacon_id);
CREATE INDEX beacon_event_matches_event_idx
    ON beacon_event_matches (workspace_id, event_id);
-- agent_outcomes rows age out on retention; ON DELETE SET NULL would scan
-- this table per delete without the referencing-side index.
CREATE INDEX beacon_event_matches_outcome_idx
    ON beacon_event_matches (outcome_id) WHERE outcome_id IS NOT NULL;
