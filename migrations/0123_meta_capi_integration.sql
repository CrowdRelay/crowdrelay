-- Server-side ad conversion tracking for Meta CAPI, Google Ads, and Bandsintown.
--
-- fan_ad_attribution: captures browser-side identifiers and UTM/tracking
--   parameters at signup time so the conversion workers can send them
--   alongside hashed user data for maximum matching quality on each platform.
--   One row per fan, upserted at signup.
--
-- ad_conversion_deliveries: idempotent delivery log shared by all three
--   platforms. Each (platform, event_name, event_id) is sent exactly once.
--   The event_id is shared with the browser pixel/snippet for
--   server/browser deduplication.

CREATE TABLE fan_ad_attribution (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    -- Meta: _fbp cookie (browser ID) and _fbc cookie (click ID)
    meta_fbp text CHECK (meta_fbp IS NULL OR (btrim(meta_fbp) <> '' AND char_length(meta_fbp) <= 128)),
    meta_fbc text CHECK (meta_fbc IS NULL OR (btrim(meta_fbc) <> '' AND char_length(meta_fbc) <= 256)),
    -- Google: gclid (Google Click ID) from the ad URL
    google_gclid text CHECK (google_gclid IS NULL OR (btrim(google_gclid) <> '' AND char_length(google_gclid) <= 256)),
    -- Bandsintown: tracking ref from Bandsintown event links
    bandsintown_ref text CHECK (bandsintown_ref IS NULL OR (btrim(bandsintown_ref) <> '' AND char_length(bandsintown_ref) <= 256)),
    -- Shared UTM parameters (work for all platforms)
    utm_source text CHECK (utm_source IS NULL OR (btrim(utm_source) <> '' AND char_length(utm_source) <= 256)),
    utm_medium text CHECK (utm_medium IS NULL OR (btrim(utm_medium) <> '' AND char_length(utm_medium) <= 256)),
    utm_campaign text CHECK (utm_campaign IS NULL OR (btrim(utm_campaign) <> '' AND char_length(utm_campaign) <= 256)),
    utm_content text CHECK (utm_content IS NULL OR (btrim(utm_content) <> '' AND char_length(utm_content) <= 256)),
    utm_term text CHECK (utm_term IS NULL OR (btrim(utm_term) <> '' AND char_length(utm_term) <= 256)),
    -- Request context for server-side event forwarding
    client_ip_address text CHECK (
        client_ip_address IS NULL
        OR (btrim(client_ip_address) <> '' AND char_length(client_ip_address) <= 64)
    ),
    client_user_agent text CHECK (
        client_user_agent IS NULL
        OR (btrim(client_user_agent) <> '' AND char_length(client_user_agent) <= 512)
    ),
    event_source_url text CHECK (
        event_source_url IS NULL
        OR (btrim(event_source_url) <> '' AND char_length(event_source_url) <= 2048)
    ),
    captured_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, fan_id),
    CONSTRAINT fan_ad_attribution_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

CREATE TABLE ad_conversion_deliveries (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    platform text NOT NULL CHECK (platform IN ('meta', 'google', 'bandsintown')),
    fan_id uuid,
    ticket_order_id uuid,
    event_name text NOT NULL CHECK (btrim(event_name) <> ''),
    event_id text NOT NULL CHECK (btrim(event_id) <> ''),
    action_source text NOT NULL DEFAULT 'website',
    -- HTTP status from the platform API (200, 204, etc.)
    response_status integer NOT NULL,
    response_body text,
    sent_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, platform, event_name, event_id),
    CONSTRAINT ad_conversion_deliveries_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE SET NULL
);

CREATE INDEX ad_conversion_deliveries_sent_at_idx
    ON ad_conversion_deliveries (workspace_id, sent_at DESC);

CREATE INDEX ad_conversion_deliveries_fan_idx
    ON ad_conversion_deliveries (workspace_id, fan_id, platform, event_name);

CREATE INDEX ad_conversion_deliveries_pending_idx
    ON ad_conversion_deliveries (workspace_id, platform, event_name)
    INCLUDE (fan_id);
