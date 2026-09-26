-- What a harvested comment answers, kept beside it.
--
-- The reply drafter was told "the band's earlier reply in this thread" as a
-- placeholder whenever a fan answered the band, because the harvest kept only
-- fans' comments. The harvest already holds the whole thread when it reads
-- one, so the parent's words and who wrote them are stored with the comment:
-- a multi-turn conversation drafts against what the band actually said.
--
--   parent_body     the text of the comment this one replies to; NULL when
--                   it replies to the post itself
--   parent_by_band  whether the band's own account wrote that parent

ALTER TABLE community_comments
    ADD COLUMN IF NOT EXISTS parent_body text
        CHECK (parent_body IS NULL OR char_length(parent_body) <= 4000),
    ADD COLUMN IF NOT EXISTS parent_by_band boolean NOT NULL DEFAULT false;
