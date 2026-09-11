-- An installed Signal app, recorded before it has any fan identity.
--
-- Nineteen people have the app. CrowdRelay knows about two of them, and it
-- only knows about those two because they happened to complete a chain that
-- starts with a fan session: `/v1/me/push/endpoints` requires one, and the app
-- returns silently when there is none. An install that never signs in, or
-- whose owner declines the notification prompt, leaves no trace at all.
--
-- So the system cannot count what it is missing. It cannot say whether the app
-- is failing to convert installs into fans, or simply is not being installed,
-- and those call for opposite work. Every number it does have about Signal --
-- endpoints, pushes delivered, activated fans -- is measured at the bottom of
-- a funnel whose top is invisible.
--
-- This table is the top. It is written by a public endpoint so that recording
-- an install never depends on the identity the install is supposed to produce,
-- which is the inversion that hid seventeen people.
--
-- `installation_id` is the app's own random, device-scoped identifier. It is
-- not a person, carries nothing about one, and is generated on the device
-- without any account, so a row here is not personal data until `fan_id` is
-- set -- which happens only once that fan has identified themselves.
CREATE TABLE signal_installations (
  workspace_id    UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  installation_id TEXT        NOT NULL,
  platform        TEXT        NOT NULL
                  CHECK (platform IN ('android', 'ios', 'desktop', 'web')),
  -- Free-form because it is the app's own version string; the brain reads it
  -- to tell "installs stopped" from "installs stopped on the old build".
  app_version     TEXT,
  -- Null until the install identifies itself. The gap between the count of
  -- rows and the count of non-null values here IS the activation funnel.
  fan_id          UUID        REFERENCES fans(id) ON DELETE SET NULL,
  first_seen_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, installation_id)
);

-- The funnel query reads installs, then installs with a fan, in one workspace.
CREATE INDEX signal_installations_workspace_idx
  ON signal_installations (workspace_id, last_seen_at DESC);

-- Partial, because the identified set is the small one and is what the
-- activation gauge counts.
CREATE INDEX signal_installations_identified_idx
  ON signal_installations (workspace_id, fan_id)
  WHERE fan_id IS NOT NULL;
