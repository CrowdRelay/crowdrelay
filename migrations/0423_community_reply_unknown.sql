-- Provider confirmation loss is not a failed reply.
--
-- A direct Graph API POST can succeed remotely and still lose its response.
-- Returning such a row to 'approved' would make the next worker cycle repeat
-- an external side effect and can post the same reply twice. Keep an explicit
-- unknown terminal-for-automation state instead: a later reconciliation or
-- operator may resolve it, but the sender never retries it blindly.

ALTER TABLE community_comments
    DROP CONSTRAINT IF EXISTS community_comments_status_check;

ALTER TABLE community_comments
    ADD CONSTRAINT community_comments_status_check CHECK (status IN (
        'unanswered', 'awaiting_approval', 'approved', 'replying',
        'unknown', 'replied', 'skipped', 'failed'
    ));

COMMENT ON COLUMN community_comments.status IS
    'Reply lifecycle. unknown means the external send may have succeeded but no definitive provider receipt was obtained; automation must not resend it.';
