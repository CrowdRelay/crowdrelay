-- Add Częstochowa to Signal Strength for every existing workspace and add the
-- hot partial indexes used by event automation and ticket accounting.

WITH city AS (
    INSERT INTO cities (slug, name, country_code, region, latitude, longitude)
    VALUES ('czestochowa', 'Częstochowa', 'PL', 'śląskie', 50.8118, 19.1203)
    ON CONFLICT (country_code, slug) DO UPDATE
    SET name = EXCLUDED.name,
        region = EXCLUDED.region,
        latitude = EXCLUDED.latitude,
        longitude = EXCLUDED.longitude
    RETURNING id
)
INSERT INTO city_aggregates (workspace_id, city_id)
SELECT workspace.id, city.id
FROM workspaces AS workspace
CROSS JOIN city
ON CONFLICT (workspace_id, city_id) DO NOTHING;

CREATE INDEX IF NOT EXISTS fan_consents_latest_marketing_idx
    ON fan_consents (workspace_id, fan_id, recorded_at DESC, id DESC)
    INCLUDE (granted)
    WHERE purpose = 'marketing';

CREATE INDEX IF NOT EXISTS event_interests_event_fan_idx
    ON event_interests (workspace_id, event_id, fan_id);

CREATE INDEX IF NOT EXISTS ticket_orders_paid_period_idx
    ON ticket_orders (workspace_id, paid_at, ticket_sale_id, id)
    INCLUDE (status, currency, amount_gross_minor, amount_net_minor, amount_vat_minor, invoice_requested)
    WHERE paid_at IS NOT NULL;
