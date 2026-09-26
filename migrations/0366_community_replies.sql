-- The band answering the people who comment on its own posts.
--
-- A band that posts and never answers reads as a bot, and a comment under its
-- own post is the warmest contact it gets. The community executor harvests
-- those comments (only when a post's comment count grew — every read goes
-- through the one Reddit session), drafts an answer through the agents
-- service, runs it past the same guards a post passes plus an independent
-- review, and sends it — after a person approved it, or unattended when the
-- operator switched that on and the draft came back clean.
--
-- Status lifecycle:
--   unanswered        → harvested, not drafted yet
--   awaiting_approval → drafted; `hold_reason` says what held it, NULL when
--                       it is only waiting because unattended replies are off
--   approved          → a person (or a clean unattended route) said yes;
--                       leaves after `not_before`
--   replying          → the send is in flight (crash recovery reclaims)
--   replied           → Reddit accepted it; reply ids recorded
--   skipped           → nothing to say (the drafter's reason) or a person
--                       declined
--   failed            → Reddit refused it, or it kept failing

ALTER TABLE community_posts
    ADD COLUMN IF NOT EXISTS comments_seen integer NOT NULL DEFAULT 0
        CHECK (comments_seen >= 0);

CREATE TABLE IF NOT EXISTS community_comments (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id        uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    community_post_id   uuid NOT NULL REFERENCES community_posts(id) ON DELETE CASCADE,
    reddit_comment_id   text NOT NULL CHECK (reddit_comment_id ~ '^t1_[A-Za-z0-9]{1,20}$'),
    parent_id           text NOT NULL CHECK (parent_id ~ '^t[13]_[A-Za-z0-9]{1,20}$'),
    author              text NOT NULL CHECK (char_length(author) BETWEEN 1 AND 64),
    body                text NOT NULL CHECK (char_length(body) <= 4000),
    status              text NOT NULL DEFAULT 'unanswered' CHECK (status IN (
                            'unanswered', 'awaiting_approval', 'approved', 'replying',
                            'replied', 'skipped', 'failed'
                        )),
    draft               text CHECK (draft IS NULL OR char_length(draft) BETWEEN 1 AND 2000),
    hold_reason         text CHECK (hold_reason IS NULL OR char_length(hold_reason) <= 500),
    review_score        smallint CHECK (review_score IS NULL OR review_score BETWEEN 0 AND 10),
    drafted_by          text CHECK (drafted_by IS NULL OR char_length(drafted_by) <= 128),
    approved_by         text CHECK (approved_by IS NULL OR char_length(approved_by) <= 200),
    not_before          timestamptz,
    attempts            integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    reply_comment_id    text CHECK (reply_comment_id IS NULL OR reply_comment_id ~ '^t1_[A-Za-z0-9]{1,20}$'),
    reply_permalink     text CHECK (reply_permalink IS NULL OR char_length(reply_permalink) <= 500),
    replied_at          timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, reddit_comment_id),
    CHECK (status NOT IN ('awaiting_approval', 'approved', 'replying', 'replied') OR draft IS NOT NULL),
    CHECK (status <> 'replied' OR (reply_comment_id IS NOT NULL AND replied_at IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS community_comments_status_idx
    ON community_comments (workspace_id, status, created_at);
CREATE INDEX IF NOT EXISTS community_comments_replied_idx
    ON community_comments (workspace_id, replied_at DESC)
    WHERE status = 'replied';
