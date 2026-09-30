-- Equivalent YouTube source and release rows share asset-level exclusions.
-- Titles are never an identity key; other releases retain their own defaults.
CREATE FUNCTION crowdrelay_promotion_video_key(metadata jsonb)
RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT COALESCE(
        NULLIF(metadata->>'video_id',''),
        substring(COALESCE(metadata->>'url',metadata->>'listen_url',metadata->>'video_url')
            FROM '^https?://youtu\.be/([^/?&#]+)'),
        substring(COALESCE(metadata->>'url',metadata->>'listen_url',metadata->>'video_url')
            FROM '^https?://(?:www\.)?youtube\.com/watch\?(?:[^#]*&)?v=([^&#]+)')
    );
$$;

-- Refreshes may replace facts but cannot erase source-owned promotion policy,
-- campaign identity, or an operator request. Explicit values remain authoritative.
CREATE FUNCTION crowdrelay_preserve_source_promotion_metadata()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE owned_key text; inherited_policy jsonb; asset_key text;
BEGIN
    IF NEW.source_kind IN ('video', 'release') THEN
        IF jsonb_typeof(NEW.metadata) <> 'object' THEN
            RAISE EXCEPTION USING ERRCODE = '23514',
                MESSAGE = 'video and release metadata must be a JSON object';
        END IF;
        IF TG_OP = 'UPDATE' AND NEW.source_kind = OLD.source_kind THEN
            FOREACH owned_key IN ARRAY ARRAY[
                'promotion_excluded_platforms', 'promotion_campaign_id', 'surge_requested_at'
            ] LOOP
                IF OLD.metadata ? owned_key AND NOT (NEW.metadata ? owned_key) THEN
                    NEW.metadata := jsonb_set(NEW.metadata, ARRAY[owned_key], OLD.metadata -> owned_key);
                END IF;
            END LOOP;
        END IF;
        asset_key := crowdrelay_promotion_video_key(NEW.metadata);
        IF asset_key IS NOT NULL AND NOT (NEW.metadata ? 'promotion_excluded_platforms') THEN
            SELECT peer.metadata->'promotion_excluded_platforms' INTO inherited_policy
            FROM content_sources peer
            WHERE peer.workspace_id=NEW.workspace_id AND peer.id<>NEW.id AND peer.active
              AND peer.source_kind IN ('video','release')
              AND peer.metadata ? 'promotion_excluded_platforms'
              AND crowdrelay_promotion_video_key(peer.metadata)=asset_key
            ORDER BY peer.created_at DESC,peer.id LIMIT 1;
            IF inherited_policy IS NOT NULL THEN
                NEW.metadata := jsonb_set(NEW.metadata,'{promotion_excluded_platforms}',inherited_policy);
            END IF;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER content_sources_preserve_promotion_metadata
BEFORE INSERT OR UPDATE OF metadata ON content_sources
FOR EACH ROW EXECUTE FUNCTION crowdrelay_preserve_source_promotion_metadata();
