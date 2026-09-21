-- The attention budget: how many decisions the agent may put in front of a
-- person in a rolling week.
--
-- Every other limit on this table bounds what somebody outside the workspace
-- receives -- fans, venues, curators, press. Nothing bounded what the tenant's
-- own crew received, and that is the quantity that ran out.
--
-- The arithmetic was never close. `viryaos_autopilot_policies.max_actions_24h`
-- defaults to 50 and there are 26 contexts; every `awaiting_approval` action
-- becomes a `viryaos_team_assignments` row; every assignment owes a first
-- notice plus up to three reminders. Against that, one person.
--
-- The deeper problem is an incentive rather than a volume. The authority
-- ladder makes "ask a person" the cheapest move available to the agent in
-- almost every context, and nothing charged it for that move -- so an agent
-- behaving exactly as designed produced a queue nobody could empty, and the
-- approvals expired at 72 hours faster than they were read. Measured in
-- production: seven drafts, four approvals, four hundred and twelve
-- opportunities, zero posts published.
--
-- With a budget, asking stops being free. When it is spent the finding still
-- exists and still surfaces on the board -- it becomes a recommendation
-- instead of an action, so nothing is lost except the interruption. Note what
-- this does NOT do: it never widens authority. A budget with room left cannot
-- promote anything; it can only decline to park one more decision in a queue
-- that is already longer than the week.
--
-- # Why twenty
--
-- About three decisions a day, which is a number a band can work through. It
-- is deliberately the same order as `team_weekly_ask_ceiling`'s default of ten
-- per member: the two bound the same scarce thing from two directions, and a
-- workspace-wide budget far above the per-person one would be no budget at
-- all.
--
-- Zero is a posture, not a misconfiguration: read the board, send me nothing.
-- It is deliberately expressible, and the CHECK allows it.

ALTER TABLE viryaos_growth_envelope
    ADD COLUMN weekly_approval_requests integer NOT NULL DEFAULT 20
        CHECK (weekly_approval_requests BETWEEN 0 AND 1000);

COMMENT ON COLUMN viryaos_growth_envelope.weekly_approval_requests IS
    'Decisions the agent may put in front of a person per rolling week. When '
    'spent, findings surface as recommendations instead of approval requests. '
    '0 means never ask.';

-- The spend is counted from `viryaos_autopilot_actions`, not stored: an action
-- that was ever parked for a person carries `approval_expires_at`, and that
-- mark survives the approval. The count is therefore of asks made rather than
-- of asks still waiting -- an operator who answers quickly has still been
-- asked.
CREATE INDEX viryaos_autopilot_actions_asked_idx
    ON viryaos_autopilot_actions (workspace_id, created_at)
    WHERE approval_expires_at IS NOT NULL;
