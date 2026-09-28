-- A community target's status follows its latest screening verdict.
--
-- Both writers re-screen a community every time it is proposed again, and
-- both recorded the new verdict without moving the status to match:
--
-- - `agent_outcomes.rs` promoted on an admission but never demoted on a
--   refusal, so a community admitted once and refused later stayed
--   `promoted` with verdict `refused`. Production held two on 2026-09-28
--   (r/Metal, r/doommetal), listed to the operator as promoted.
-- - `community_promotion.rs` never touched the status on conflict at all,
--   so a refused community that grew and was re-screened as admitted stayed
--   `proposed` — the readmission its own comments describe never happened.
--
-- Posting was never at risk (the growth loop requires `admitted` as well as
-- `promoted`), but the operator's lists and every reader of `status` alone
-- disagreed with the verdict. The rule lives here once and both upserts
-- call it: discarded is sticky, admitted promotes, refused demotes to the
-- `proposed` + `refused` shape a fresh refusal is written in, and a pass
-- that produced no verdict leaves the status where it was.

CREATE OR REPLACE FUNCTION community_target_status(current_status text, verdict text)
RETURNS text
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT CASE
        WHEN current_status = 'discarded' THEN 'discarded'
        WHEN verdict = 'admitted' THEN 'promoted'
        WHEN verdict = 'refused' THEN 'proposed'
        ELSE current_status
    END
$$;

UPDATE agent_outreach_targets
SET status = community_target_status(status, screening_verdict),
    updated_at = now()
WHERE target_kind = 'community'
  AND screening_verdict IS NOT NULL
  AND status IS DISTINCT FROM community_target_status(status, screening_verdict);
