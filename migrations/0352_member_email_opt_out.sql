-- A member whose address must never receive crew mail. The seeded service
-- seats (admin/gate) have to stay 'active' — admission resolves them by
-- their configured email for issuance and door redemption — but there is
-- no mailbox behind the address, so every daily briefing bounced.
--
-- `queue_team_email_action` honours the flag at the single choke point all
-- member-facing mail flows through, so briefing, routing and reminder
-- producers cannot bypass it. The assignment itself is still recorded —
-- the flag suppresses the send, not the work item.
--
-- Bootstrap upserts never touch the column, so — like `status='disabled'` —
-- the flag is an operator decision that survives redeploys.

ALTER TABLE workspace_members
    ADD COLUMN email_opt_out boolean NOT NULL DEFAULT false;
