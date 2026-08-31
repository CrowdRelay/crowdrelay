-- Worker generation leadership lease.
--
-- Ensures only one worker generation claims background work during blue-green
-- deploys. The candidate worker starts in standby and polls this table; the
-- old worker releases leadership on shutdown, allowing the candidate to
-- acquire the lease.
--
-- This is an expand-only migration: the table is new, no existing schema is
-- modified, and the N-1 binary (which does not know about leadership) remains
-- fully functional because the table is not referenced by any existing query.
CREATE TABLE IF NOT EXISTS worker_leadership (
    id              INTEGER     PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    leader_id       TEXT        NOT NULL,
    generation      BIGINT      NOT NULL DEFAULT 1,
    acquired_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at      TIMESTAMPTZ NOT NULL DEFAULT (NOW() + INTERVAL '60 seconds')
);

INSERT INTO worker_leadership (id, leader_id, generation, acquired_at, expires_at)
VALUES (1, 'bootstrap', 0, NOW(), NOW())
ON CONFLICT (id) DO NOTHING;
