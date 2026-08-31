-- Add 'tiktok' to the viryaos_growth_metric_series platform check
-- constraint. The growth metric sync worker fetches TikTok creator
-- follower counts via the Display API /v2/user/info/ endpoint using
-- OAuth tokens stored encrypted in fanbase_connections.
-- 'tiktok' is already in the fanbase_connections check constraint.
ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT IF EXISTS viryaos_growth_metric_series_platform_check;
ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_platform_check
    CHECK (platform = ANY (ARRAY[
        'spotify', 'youtube', 'bandsintown', 'social', 'website',
        'ticketing', 'signal', 'merch', 'facebook', 'instagram',
        'soundcloud', 'tiktok'
    ]));

-- Add encrypted OAuth token columns to fanbase_connections.
-- Tokens are encrypted with SensitiveResponseKey (XChaCha20-Poly1305)
-- and stored as base64-encoded nonce||ciphertext. The credential_ref
-- column stores a short reference identifier (e.g. 'tiktok:{open_id}'),
-- not a secret blob. These columns are nullable — legacy connections
-- (n8n-backed, API-key-based) do not use them.
ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS encrypted_access_token text,
    ADD COLUMN IF NOT EXISTS encrypted_refresh_token text,
    ADD COLUMN IF NOT EXISTS token_expires_at timestamptz,
    ADD COLUMN IF NOT EXISTS token_scope text,
    ADD COLUMN IF NOT EXISTS token_type text;
