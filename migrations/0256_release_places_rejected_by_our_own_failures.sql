-- Release the communities we marked as having rejected us, when in fact our
-- own side failed before Reddit ever saw the request.
--
-- `membership_state = 'rejected'` is terminal: the join executor only ever
-- claims `not_joined`, so a rejected place is never retried. It has to mean
-- "this community said no", and until now it did not — every error landed
-- there, including our own.
--
-- On the Virya workspace that produced 71 rejected places and not one was a
-- refusal:
--
--   37  agents /reddit/join HTTP 503 "no reddit credentials stored"
--   30  agents /reddit/join HTTP 400 "subreddit must be 2-21 chars of A-Za-z0-9_"
--    3  error sending request for url (http://agent-service:8095/reddit/join)
--    1  reddit login failed ... "retryable"
--
-- The 503s were a credential outage. The 400s were our own validator refusing
-- a subreddit title that the executor should never have sent — it read
-- `discovery_places.name`, which holds the display title, instead of the slug
-- in `url`. The other four never reached Reddit either.
--
-- This returns those places to `not_joined` so the corrected executor can try
-- them properly. It matches on the note text rather than clearing every
-- rejection, because a genuine refusal — a private subreddit, a ban — must
-- stay terminal. Anything this pattern does not recognise is left alone.
UPDATE discovery_places
SET membership_state = 'not_joined',
    membership_note = 'released: recorded as rejected by our own failure, not by the community',
    membership_changed_at = now(),
    membership_changed_by = 'migration:0256',
    updated_at = now()
WHERE membership_state = 'rejected'
  AND membership_note IS NOT NULL
  AND (
      -- Our agent service failed or was unreachable.
      membership_note LIKE '%HTTP 5%'
      OR membership_note LIKE '%error sending request%'
      OR membership_note LIKE '%http error%'
      -- Our own validator rejected the request before it was sent.
      OR membership_note LIKE '%subreddit must be%'
      OR membership_note LIKE '%HTTP 400%'
      -- The upstream itself said to try again.
      OR membership_note LIKE '%retryable%'
      OR membership_note LIKE '%rate limited%'
      OR membership_note LIKE '%auth key missing%'
  );

-- Release the community targets whose refusal was a consequence of the above.
--
-- `community_promotion` screens a place once: its candidate query skips any
-- place that already has a target row, so a verdict written while the place
-- was wrongly marked rejected would never be revisited. The screener has
-- gained a `previously_refused` reason for exactly this case — distinct from
-- `poor_fit`, which is a judgement about the community and stays true — and
-- the sweep now re-screens rows carrying it.
--
-- Older rows recorded that same cause as `poor_fit`, which is indistinguishable
-- from a genuine one. Only rows whose place is being released above are
-- retagged, so a real poor-fit verdict on a healthy place is untouched.
UPDATE agent_outreach_targets AS t
SET refusal_reason = 'previously_refused',
    updated_at = now()
FROM discovery_places AS p
WHERE p.id = t.place_id
  AND t.target_kind = 'community'
  AND t.screening_verdict = 'refused'
  AND t.refusal_reason = 'poor_fit'
  AND p.membership_changed_by = 'migration:0256';
