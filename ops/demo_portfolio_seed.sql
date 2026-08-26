-- LOCAL DEMO ONLY. Seeds a two-artist roster inside one label organization so
-- the portfolio surfaces have something honest to show: consent edges between
-- the artists and an amplification campaign flowing through the owner's own
-- outbox. Safe to re-run against a development database.
--
--   psql "$CROWDRELAY_DATABASE_URL" -f ops/demo_portfolio_seed.sql

BEGIN;

INSERT INTO organizations (id, slug, name)
VALUES ('00000000-0000-0000-0000-00000000d001', 'demo-label', 'Demo Label Roster')
ON CONFLICT (slug) DO NOTHING;

INSERT INTO workspaces (id, slug, name, organization_id) VALUES
    ('00000000-0000-0000-0000-00000000da01', 'demo-headliner', 'Demo Headliner', '00000000-0000-0000-0000-00000000d001'),
    ('00000000-0000-0000-0000-00000000da02', 'demo-new-signing', 'Demo New Signing', '00000000-0000-0000-0000-00000000d001')
ON CONFLICT (slug) DO NOTHING;

-- The headliner's proven audience.
INSERT INTO fans (id, workspace_id, normalized_email, display_name, status) VALUES
    ('00000000-0000-0000-0000-00000000fa01', '00000000-0000-0000-0000-00000000da01', 'fan.one@demo.test', 'Fan One', 'active'),
    ('00000000-0000-0000-0000-00000000fa02', '00000000-0000-0000-0000-00000000da01', 'fan.two@demo.test', 'Fan Two', 'active'),
    ('00000000-0000-0000-0000-00000000fa03', '00000000-0000-0000-0000-00000000da01', 'fan.three@demo.test', 'Fan Three', 'unsubscribed')
ON CONFLICT DO NOTHING;

-- Consent edge: the new signing may be featured to the headliner's active fans.
INSERT INTO amplification_consents (
    id, organization_id, from_workspace_id, to_workspace_id,
    purpose, scope, status, max_campaigns_per_month, cooldown_days,
    approved_by, approved_at
) VALUES (
    '00000000-0000-0000-0000-00000000fc01',
    '00000000-0000-0000-0000-00000000d001',
    '00000000-0000-0000-0000-00000000da01',
    '00000000-0000-0000-0000-00000000da02',
    'release_feature', 'all_active', 'active', 2, 21,
    'demo-operator', now()
)
ON CONFLICT (from_workspace_id, to_workspace_id, purpose) DO NOTHING;

COMMIT;
