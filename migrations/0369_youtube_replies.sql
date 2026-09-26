-- The reply lane reaches the band's YouTube videos.
--
-- Comments under the band's own uploads join the same queue as Reddit,
-- Instagram and Facebook. Reading them needs only the API key the video
-- sync already holds; answering them needs the channel owner's grant, which
-- arrives as its own connection platform — `youtube_account` — so it never
-- collides with the `youtube` rows the video sync keys by channel id.
--
-- YouTube ids are URL-safe base64-ish (`UgzX…`, and `parent.child` for a
-- reply), unlike the numeric Graph ids, so the id checks gain a YouTube arm.

ALTER TABLE community_comments
    DROP CONSTRAINT IF EXISTS community_comments_platform_check,
    DROP CONSTRAINT IF EXISTS community_comments_platform_comment_id_check,
    DROP CONSTRAINT IF EXISTS community_comments_parent_id_check,
    DROP CONSTRAINT IF EXISTS community_comments_reply_comment_id_check;

ALTER TABLE community_comments
    ADD CONSTRAINT community_comments_platform_check
        CHECK (platform IN ('reddit', 'instagram', 'facebook', 'youtube')),
    ADD CONSTRAINT community_comments_platform_comment_id_check CHECK (
        (platform = 'reddit' AND platform_comment_id ~ '^t1_[A-Za-z0-9]{1,20}$')
        OR (platform IN ('instagram', 'facebook') AND platform_comment_id ~ '^[0-9_]{1,64}$')
        OR (platform = 'youtube' AND platform_comment_id ~ '^[A-Za-z0-9_.-]{1,128}$')
    ),
    ADD CONSTRAINT community_comments_parent_id_check CHECK (
        (platform = 'reddit' AND parent_id ~ '^t[13]_[A-Za-z0-9]{1,20}$')
        OR (platform IN ('instagram', 'facebook') AND parent_id ~ '^[0-9_]{1,64}$')
        OR (platform = 'youtube' AND parent_id ~ '^[A-Za-z0-9_.-]{1,128}$')
    ),
    ADD CONSTRAINT community_comments_reply_comment_id_check CHECK (
        reply_comment_id IS NULL
        OR (platform = 'reddit' AND reply_comment_id ~ '^t1_[A-Za-z0-9]{1,20}$')
        OR (platform IN ('instagram', 'facebook') AND reply_comment_id ~ '^[0-9_]{1,64}$')
        OR (platform = 'youtube' AND reply_comment_id ~ '^[A-Za-z0-9_.-]{1,128}$')
    );

ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY[
        'meta', 'google_ads', 'reddit', 'bandsintown', 'spotify', 'youtube',
        'facebook', 'instagram', 'soundcloud', 'tiktok',
        'discord', 'telegram', 'lastfm',
        'deezer', 'discogs', 'bluesky', 'bandcamp', 'x', 'gdrive', 'gmail',
        'youtube_account'
    ]));
