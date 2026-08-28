-- Reddit proxy state — singleton row written by the reddit-proxy-finder
-- sidecar (Python free-proxy library). The Rust worker reads this table
-- every 5 minutes and dynamically rebuilds its HTTP client when the proxy
-- changes, bypassing Reddit's IP-level 403 blocks without a restart.
CREATE TABLE IF NOT EXISTS reddit_proxy_state (
    id                  INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    proxy_url           TEXT NOT NULL,
    last_verified_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_verified_ok    BOOLEAN NOT NULL DEFAULT true,
    reddit_status_code  INTEGER
);
