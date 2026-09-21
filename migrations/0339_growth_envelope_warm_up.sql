-- The warm-up: how many unattended actions a context may take per week while
-- it is still below its evidence floor.
--
-- `domain::autonomy::disposition_with_evidence` downgrades unattended
-- execution to approval until a context has twenty resolved outcomes behind
-- it, and its own comment names the trap it was trying to avoid: acting is
-- how the observations that clear the floor get made, so a gate that blocks
-- action below the floor guarantees the floor is never reached. It avoided
-- denial by routing below-floor work through approval instead -- correct in
-- principle, and in production the same thing. Approvals expire at 72 hours,
-- one person empties the queue, and the floor was never approached from
-- below: zero resolved outcomes against a floor of twenty.
--
-- So the gate was self-sealing in practice. This is the standard way out: a
-- small, bounded number of unattended actions per context per week whose
-- whole purpose is to produce the observations the floor is waiting for.
--
-- # Why it lives on the envelope
--
-- The envelope is already "the limits inside which the growth agent may act
-- without being asked", and every other number an operator tunes about
-- unattended action is here. A second table would be a second place to look
-- and a second place to disagree.
--
-- # Why the default is five and not zero
--
-- Zero is what the system already did, and it is why the floor never moved.
-- Five a week is small enough that a mistake is five actions rather than a
-- campaign, and large enough that twenty observations are a month away rather
-- than never. An operator who wants the old behaviour sets it to zero, and
-- that reads in code as "no warm-up at all" rather than as a missing setting.
--
-- Nothing else about the ladder changes. The warm-up is read only where the
-- evidence floor is what is holding an action back: a context below its
-- confidence bar is still denied, `observe` and `recommend` still produce
-- nothing, the class ceiling still clamps and the weekly touch budgets still
-- bound volume.

ALTER TABLE viryaos_growth_envelope
    ADD COLUMN weekly_bootstrap_actions integer NOT NULL DEFAULT 5
        CHECK (weekly_bootstrap_actions BETWEEN 0 AND 100);

COMMENT ON COLUMN viryaos_growth_envelope.weekly_bootstrap_actions IS
    'Unattended actions one context may take per rolling week while below its '
    'evidence floor, so the floor can be reached. 0 disables the warm-up.';
