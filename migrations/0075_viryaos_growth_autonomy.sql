-- The autonomy ceiling: how much the growth agent may do unattended, keyed by
-- what an action costs and how far it reaches rather than by which bounded
-- context produced it.
--
-- The Autopilot policy table already answers "how much may this context do".
-- That axis alone cannot separate a push to consented fans from an email to a
-- playlist curator when both come from the `release` context, and those two
-- carry completely different risk. So this table sits above it and the stricter
-- of the two wins.
--
-- The values are operator data on purpose. Widening the agent's autonomy later
-- -- letting it approach curators unattended, for example -- is an update here
-- plus a set of pre-approved templates, not a code change. Tightening is the
-- same update in reverse, which means an operator who gets nervous at 2am can
-- act without a deploy.
--
-- Seeded to the safest posture the operator chose: the agent acts alone on its
-- own surfaces and its own consented audience, and asks before it touches
-- anybody else's relationship or any money.

CREATE TABLE viryaos_growth_autonomy (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_class text NOT NULL CHECK (action_class IN (
        'first_party_reversible', 'owned_audience', 'third_party', 'paid'
    )),
    ceiling text NOT NULL CHECK (ceiling IN (
        'observe', 'recommend', 'require_approval', 'bounded_auto'
    )),
    -- Free text so an operator widening a ceiling records why, and the next
    -- person reading the row learns something the audit log cannot tell them.
    rationale text CHECK (rationale IS NULL OR char_length(rationale) <= 240),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, action_class)
);

CREATE TRIGGER viryaos_growth_autonomy_set_updated_at
BEFORE UPDATE ON viryaos_growth_autonomy
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- Money and third-party contact can never start out unattended. An absent row
-- is read in code as the safest ceiling, never as "no ceiling" -- a missing
-- migration must not be a grant of authority.
INSERT INTO viryaos_growth_autonomy (workspace_id, action_class, ceiling, rationale)
SELECT workspace.id, seed.action_class, seed.ceiling, seed.rationale
FROM workspaces AS workspace
CROSS JOIN (VALUES
    ('first_party_reversible', 'bounded_auto',
     'costs nothing, reaches nobody outside the workspace, undone by doing the opposite'),
    ('owned_audience', 'bounded_auto',
     'reaches fans who opted in; free, but a sent message cannot be unsent'),
    ('third_party', 'require_approval',
     'the band gets one first approach to each venue, curator or press contact'),
    ('paid', 'require_approval',
     'moves money and cannot be recovered by changing our minds')
) AS seed(action_class, ceiling, rationale)
ON CONFLICT (workspace_id, action_class) DO NOTHING;

CREATE OR REPLACE FUNCTION viryaos_provision_growth_autonomy()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_growth_autonomy (workspace_id, action_class, ceiling, rationale)
    VALUES
        (NEW.id, 'first_party_reversible', 'bounded_auto',
         'costs nothing, reaches nobody outside the workspace, undone by doing the opposite'),
        (NEW.id, 'owned_audience', 'bounded_auto',
         'reaches fans who opted in; free, but a sent message cannot be unsent'),
        (NEW.id, 'third_party', 'require_approval',
         'the band gets one first approach to each venue, curator or press contact'),
        (NEW.id, 'paid', 'require_approval',
         'moves money and cannot be recovered by changing our minds')
    ON CONFLICT (workspace_id, action_class) DO NOTHING;
    RETURN NEW;
END;
$$;

CREATE TRIGGER workspaces_provision_growth_autonomy
AFTER INSERT ON workspaces
FOR EACH ROW
EXECUTE FUNCTION viryaos_provision_growth_autonomy();
