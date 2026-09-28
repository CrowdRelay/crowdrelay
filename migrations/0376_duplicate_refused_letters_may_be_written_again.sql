-- Letters refused as duplicates may be written again.
--
-- Polish letters greeted "Dzień dobry," with no name, so every letter of one
-- template was word for word the same, and the dispatch guard refuses a
-- third-party draft that already went out ("a broadcast is not a pitch").
-- On 2026-09-28 the operator approved 39 show letters to Polish press; one
-- was sent and 38 failed as `state_changed`. Letters now greet by name
-- (`outreach_letter::greeting_pl`), so each is the contact's own.
--
-- A failed action keeps its idempotency key, and the evaluator dedupes the
-- pitch on that key — so those 38 contacts could never be offered a letter
-- again. Re-key exactly the rows the guard refused: a failed outreach
-- request whose draft is identical to one another action already sent. The
-- rows stay as the record of what happened; the next evaluation writes a
-- fresh, named letter for the operator to approve.

UPDATE autopilot_actions AS action
SET idempotency_key = action.idempotency_key || ':duplicate:' || action.id::text
WHERE action.action_kind = 'outreach.request'
  AND action.status = 'failed'
  AND action.last_error_kind = 'state_changed'
  AND action.payload ? 'draft'
  AND EXISTS (
      SELECT 1
      FROM autopilot_action_emissions AS emission
      JOIN outbox_events AS outbound
        ON outbound.id = emission.outbox_event_id
       AND outbound.workspace_id = emission.workspace_id
      WHERE emission.workspace_id = action.workspace_id
        AND emission.action_id <> action.id
        AND outbound.payload->'draft' = action.payload->'draft'
  );
