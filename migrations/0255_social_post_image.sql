-- The image an Instagram post carried.
--
-- Instagram has no text-only post: every feed publication needs a media
-- container built from an image or video the platform can fetch. The drafts
-- are text, so CrowdRelay has to choose the image, and this records which one
-- it chose.
--
-- Recorded rather than derived for two reasons. It is the only way to say
-- afterwards what was actually published -- the asset row can be edited or
-- deactivated later, and a post is a thing that happened. And it is what makes
-- rotation possible: the selector picks the least recently published asset, so
-- a run of Instagram posts does not repeat one photo, which is what a bot
-- looks like.
--
-- Nullable because Facebook and X posts do not have one, and a column that
-- means "not applicable" must not be forced to lie with a placeholder.
ALTER TABLE social_posts
    ADD COLUMN IF NOT EXISTS image_url text
        CHECK (image_url IS NULL OR image_url ~* '^https://');

-- The rotation query asks "when was this image last published on this
-- platform". Partial, because only published posts with an image answer it.
CREATE INDEX IF NOT EXISTS social_posts_image_rotation_idx
    ON social_posts (workspace_id, platform, image_url, posted_at DESC)
    WHERE image_url IS NOT NULL AND status = 'posted';
