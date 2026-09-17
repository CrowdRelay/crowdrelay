-- §4h-11 / 3.9: a press contact can say where it is.
--
-- A band playing a city wants that city's paper, station and blog, and the
-- table holding press contacts could not say where any of them were. The
-- booking half has carried a city since the first seed — `viryaos_booking
-- _candidates.city_slug`, `viryaos_booking_targets.city_id` — because a room
-- is somewhere by definition. A magazine is not, which is why this column is
-- nullable and stays nullable: a national title covers everywhere, and
-- inventing a city for it would file it under one and hide it from the rest.
--
-- Absent means "we do not know where this contact is", never "nowhere". The
-- read that asks who can help with Friday's show in Wrocław lists the ones it
-- can place and says how many it could not, rather than pretending the
-- unplaced are irrelevant.
--
-- Numbered 0307: 0305 is on main and 0306 is on the attestation-anchor branch.
-- Two files claiming one version is a migrator error rather than a merge
-- conflict somebody would notice.

ALTER TABLE agent_outreach_targets
    ADD COLUMN city_id uuid REFERENCES cities(id) ON DELETE SET NULL;

COMMENT ON COLUMN agent_outreach_targets.city_id IS
    'Where this contact is, when we know. NULL is unknown or national — never '
    'a claim that the contact is nowhere.';

-- The read is always "who is in this city, for this workspace".
CREATE INDEX agent_outreach_targets_city_idx
    ON agent_outreach_targets (workspace_id, city_id)
    WHERE city_id IS NOT NULL;
