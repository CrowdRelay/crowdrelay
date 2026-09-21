-- Community relay batches: one approval per piece of content, not one per
-- community it lands in.
--
-- A synced post (or release video) drafted for fifty communities used to park
-- fifty `community.engage.request` actions — fifty cards, fifty notification
-- emails, and an approval click per subreddit for what is really one
-- question: "does this content go to the communities that will take it?" The
-- batch row is that question's answer made durable: the operator approves the
-- content once, and every delivery — drafted already or still landing —
-- queues under it and drips out at a safe interval on the worker.
--
-- `source_id` is the batch's identity: the content source whose facts the
-- posts carry. One batch per source, ever — a second outcome naming the same
-- source joins the existing batch instead of re-asking, and a revoked batch
-- rejects later drafts rather than re-parking the same ask.
--
-- Status lifecycle:
--   awaiting_approval → operator has not answered; deliveries park as
--                       individual `awaiting_approval` actions but surface as
--                       the single batch card, not fifty rows
--   approved          → deliveries execute and drip out of community_posts at
--                       `interval_seconds` between posts
--   revoked           → operator withdrew: queued actions are cancelled and
--                       pending community_posts are cancelled; late drafts are
--                       rejected at ingest
--   done              → `observe_until` passed; the batch is a result row now
--
-- `interval_seconds` is the per-batch floor between two posts from the same
-- batch on the community executor — the "one per hour so they don't ban us"
-- the operator approved. 300s is the smallest gap the table allows; a lower
-- floor would let a config value turn the drip into the burst it exists to
-- prevent.
--
-- `observe_until` is set at approval (approval + 7 days): the window the
-- batch card reports against — posts made, posts failed, clicks the tracked
-- links earned. The sweep that marks `done` fires once the window closes.

CREATE TABLE community_relay_batches (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id     uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source_id        uuid NOT NULL,
    status           text NOT NULL DEFAULT 'awaiting_approval'
                     CHECK (status IN ('awaiting_approval','approved','revoked','done')),
    interval_seconds int  NOT NULL DEFAULT 3600
                     CHECK (interval_seconds BETWEEN 300 AND 86400),
    approved_at      timestamptz,
    approved_by      text,
    revoked_at       timestamptz,
    revoked_by       text,
    observe_until    timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, source_id),
    CONSTRAINT community_relay_batches_source_fk
        FOREIGN KEY (workspace_id, source_id)
        REFERENCES viryaos_content_sources (workspace_id, id)
        ON DELETE CASCADE
);

-- The delivery ledger's link back to its batch. `community_posts` rows are
-- seeded from succeeded engage actions; `relay_source_id` carries the action
-- payload's source_id so the executor can pace the batch without re-reading
-- the action. Nullable: posts that predate batches, and drafts raised outside
-- a relay, have no batch. No FK — a source row expiring must not cascade away
-- the posting record.
ALTER TABLE community_posts
    ADD COLUMN IF NOT EXISTS relay_source_id uuid;

-- `cancelled`: the batch was revoked while the post still waited in the
-- queue. Distinct from `failed` — nothing was attempted, nothing went wrong;
-- the operator said stop.
ALTER TABLE community_posts
    DROP CONSTRAINT IF EXISTS community_posts_status_check,
    ADD CONSTRAINT community_posts_status_check
    CHECK (status = ANY (ARRAY['pending'::text, 'posting'::text, 'posted'::text, 'failed'::text, 'rate_limited'::text, 'awaiting_manual_post'::text, 'cancelled'::text]));

CREATE INDEX IF NOT EXISTS community_posts_relay_source_idx
    ON community_posts (workspace_id, relay_source_id, status, posted_at)
    WHERE relay_source_id IS NOT NULL;
