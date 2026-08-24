-- Free-reach pitching, presented as something a human can actually approve.
--
-- The outreach context has been able to pitch one target at a time since 0039,
-- and everything about relevance, cadence, follow-ups and decline cooldowns
-- stays exactly where it is. What was missing is the shape of the work.
--
-- Forty individual approvals is how a human stops approving. The queue fills,
-- the operator clicks through the first six, and the rest sit there being
-- evidence the agent is working while nothing goes out. So a batch of pitches
-- for one release, of one kind, is drafted together, sealed together and
-- approved in one move.
--
-- The wave holds no authority of its own. Every pitch inside it is still an
-- ordinary `third_party` outreach action under the ordinary class ceiling and
-- the ordinary weekly budget; a wave is sized to what is left of that budget
-- rather than to however many targets happen to exist. Approving a wave
-- approves the actions that are already there — it cannot conjure more.
--
-- Membership lives in the action payload (`wave_id`), the way a play step
-- carries its `play_id`. A column on `viryaos_autopilot_actions` would put one
-- context's concern on the hottest table in the system.

CREATE TABLE viryaos_outreach_waves (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- What the wave is pitched around. A tour leg has no row of its own, so a
    -- leg is represented by the show it hangs off; the day the schema grows
    -- real legs this admits a third kind rather than being reinterpreted.
    anchor_kind text NOT NULL CHECK (anchor_kind IN ('release', 'event')),
    -- A loose reference on purpose, matching the growth-metric series and the
    -- plays: a record of what the agent did must not block deletion of the
    -- business row it was about.
    anchor_id uuid NOT NULL,
    anchor_at timestamptz NOT NULL,
    target_kind text NOT NULL CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator', 'support_slot',
        'endorsement', 'media_patronage'
    )),
    state text NOT NULL DEFAULT 'drafting' CHECK (state IN (
        'drafting', 'sealed', 'approved', 'expired'
    )),
    -- The ceiling this wave was sized against, frozen when it opened. Read back
    -- rather than recomputed: an operator looking at a sealed wave should see
    -- the budget it was drafted under, not today's.
    capacity integer NOT NULL CHECK (capacity BETWEEN 1 AND 200),
    opened_at timestamptz NOT NULL DEFAULT now(),
    sealed_at timestamptz,
    settled_at timestamptz,
    -- Present only on a wave that ended without being approved. An unapproved
    -- wave is a fact about the operator's queue, not about the agent, and
    -- `too_few_pitches` says something different from `anchor_passed`.
    expiry_reason text CHECK (expiry_reason IS NULL OR expiry_reason IN (
        'anchor_passed', 'anchor_withdrawn', 'too_few_pitches'
    )),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- One wave per kind per anchor, for ever. Not "one open wave": allowing a
    -- second after the first settles would re-pitch the same curators about the
    -- same record.
    UNIQUE (workspace_id, anchor_kind, anchor_id, target_kind),
    CHECK ((settled_at IS NOT NULL) = (state IN ('approved', 'expired'))),
    CHECK (expiry_reason IS NULL OR state = 'expired'),
    CHECK (state <> 'drafting' OR sealed_at IS NULL),
    CHECK (state <> 'sealed' OR sealed_at IS NOT NULL)
);

CREATE TRIGGER viryaos_outreach_waves_set_updated_at
BEFORE UPDATE ON viryaos_outreach_waves
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The cycle asks "which waves have work" every few minutes. Partial, because a
-- settled wave is never looked at again.
CREATE INDEX viryaos_outreach_waves_live_idx
    ON viryaos_outreach_waves (workspace_id, anchor_at)
    WHERE settled_at IS NULL;

-- Counting a wave's pitches, and approving them together, both read the
-- payload. Without this that is a sequential scan of every action in the
-- workspace, per wave, every cycle.
CREATE INDEX viryaos_autopilot_actions_wave_idx
    ON viryaos_autopilot_actions ((payload->>'wave_id'))
    WHERE context = 'outreach';
