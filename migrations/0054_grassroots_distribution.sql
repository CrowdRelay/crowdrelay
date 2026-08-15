-- Durable free/grassroots distribution state for one live event.
--
-- The show-growth executor already decides which lever is due. These tables keep
-- the provider/relationship outcome in CrowdRelay so retries, attribution and
-- future decisions do not depend on n8n memory or opaque third-party state.

CREATE TABLE viryaos_show_growth_surfaces (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    surface_key text NOT NULL CHECK (surface_key ~ '^[a-z0-9][a-z0-9_.:-]{1,95}$'),
    provider text NOT NULL CHECK (btrim(provider) <> '' AND char_length(provider) <= 64),
    surface_kind text NOT NULL CHECK (surface_kind IN (
        'owned','listing','audience_capture','provider_message','profile_feature',
        'ticket_surface','community','partner','physical_qr','social_proof'
    )),
    status text NOT NULL DEFAULT 'unknown' CHECK (status IN (
        'unknown','ready','manual','published','verified','blocked','skipped','retired'
    )),
    public_url text CHECK (public_url IS NULL OR (btrim(public_url) <> '' AND char_length(public_url) <= 2048)),
    attribution_url text CHECK (attribution_url IS NULL OR (btrim(attribution_url) <> '' AND char_length(attribution_url) <= 2048)),
    free_quota_remaining integer CHECK (free_quota_remaining IS NULL OR free_quota_remaining >= 0),
    attributable_reach bigint NOT NULL DEFAULT 0 CHECK (attributable_reach >= 0),
    attributed_clicks bigint NOT NULL DEFAULT 0 CHECK (attributed_clicks >= 0),
    attributed_rsvps bigint NOT NULL DEFAULT 0 CHECK (attributed_rsvps >= 0),
    attributed_ticket_orders bigint NOT NULL DEFAULT 0 CHECK (attributed_ticket_orders >= 0),
    last_checked_at timestamptz,
    last_published_at timestamptz,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, event_id, surface_key),
    CONSTRAINT viryaos_show_growth_surfaces_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE CASCADE
);
CREATE TRIGGER viryaos_show_growth_surfaces_set_updated_at
BEFORE UPDATE ON viryaos_show_growth_surfaces
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_show_growth_surfaces_due_idx
    ON viryaos_show_growth_surfaces (workspace_id, event_id, status, provider, surface_key)
    WHERE status IN ('unknown','ready','manual','blocked');

-- A relationship edge is consent/suppression state, not an excuse to contact a
-- second person. Both endpoints are Beacons; newly discovered intro targets are
-- inserted unverified first and only become outreach-eligible through the normal
-- Beacon verification policy.
CREATE TABLE viryaos_grassroots_edges (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    from_beacon_id uuid NOT NULL,
    to_beacon_id uuid NOT NULL,
    relationship_kind text NOT NULL CHECK (relationship_kind IN (
        'warm_intro','cross_promo','community_access','bill_partner',
        'venue_partner','creator_relay'
    )),
    status text NOT NULL DEFAULT 'candidate' CHECK (status IN (
        'candidate','permission_requested','introduced','active','declined','suppressed'
    )),
    consent_recorded_at timestamptz,
    source_public_url text CHECK (
        source_public_url IS NULL OR (btrim(source_public_url) <> '' AND char_length(source_public_url) <= 2048)
    ),
    notes text CHECK (notes IS NULL OR char_length(notes) <= 2000),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, from_beacon_id, to_beacon_id, relationship_kind),
    CHECK (from_beacon_id <> to_beacon_id),
    CONSTRAINT viryaos_grassroots_edges_from_fk
        FOREIGN KEY (workspace_id, from_beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_grassroots_edges_to_fk
        FOREIGN KEY (workspace_id, to_beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE
);
CREATE TRIGGER viryaos_grassroots_edges_set_updated_at
BEFORE UPDATE ON viryaos_grassroots_edges
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_grassroots_edges_active_idx
    ON viryaos_grassroots_edges (workspace_id, from_beacon_id, status, relationship_kind, to_beacon_id)
    WHERE status IN ('permission_requested','introduced','active');

-- One bounded activation receipt per destination/event. This is intentionally
-- provider-neutral: executor callbacks can record public URLs and measurable
-- outcomes without inventing a separate marketing CRM.
CREATE TABLE viryaos_grassroots_activations (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    event_id uuid NOT NULL,
    beacon_id uuid,
    activation_kind text NOT NULL CHECK (activation_kind IN (
        'listing','cross_post','warm_intro','community_post','physical_qr',
        'ticket_giveaway','live_clip','fan_relay','provider_message','profile_feature'
    )),
    destination_key text NOT NULL CHECK (btrim(destination_key) <> '' AND char_length(destination_key) <= 240),
    status text NOT NULL DEFAULT 'planned' CHECK (status IN (
        'planned','awaiting_permission','requested','completed','declined','blocked','cancelled'
    )),
    canonical_url text CHECK (canonical_url IS NULL OR (btrim(canonical_url) <> '' AND char_length(canonical_url) <= 2048)),
    public_receipt_url text CHECK (public_receipt_url IS NULL OR (btrim(public_receipt_url) <> '' AND char_length(public_receipt_url) <= 2048)),
    attributable_reach bigint NOT NULL DEFAULT 0 CHECK (attributable_reach >= 0),
    attributed_clicks bigint NOT NULL DEFAULT 0 CHECK (attributed_clicks >= 0),
    attributed_rsvps bigint NOT NULL DEFAULT 0 CHECK (attributed_rsvps >= 0),
    attributed_ticket_orders bigint NOT NULL DEFAULT 0 CHECK (attributed_ticket_orders >= 0),
    receipt jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(receipt) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, event_id, activation_kind, destination_key),
    CONSTRAINT viryaos_grassroots_activations_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_grassroots_activations_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE RESTRICT
);
CREATE TRIGGER viryaos_grassroots_activations_set_updated_at
BEFORE UPDATE ON viryaos_grassroots_activations
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_grassroots_activations_event_idx
    ON viryaos_grassroots_activations (workspace_id, event_id, status, activation_kind, updated_at DESC);

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;
ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h',
        'show_ticket_revenue_7d','show_growth_surface_clicks_7d',
        'show_growth_attributed_ticket_orders_7d','grassroots_activation_replies_14d'
    ));
