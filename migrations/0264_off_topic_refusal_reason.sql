-- Admit `off_topic` as a community screening refusal reason.
--
-- The community screen only ever measured size and paid-placement policy:
-- "METAL GEAR SOLID" (a video-game community) and "r/whatisthisthing" (object
-- identification) were admitted on member count alone, and the engager then
-- posted band material into them. The screener now judges topical fit from
-- the place's name, notes and genres, and refuses with `off_topic` — distinct
-- from `poor_fit` (a judgement about quality) and `previously_refused` (a
-- cause we recorded ourselves).
--
-- Expand-only: every previously legal value stays legal, so an older binary
-- is unaffected and the constraint validates without a table rewrite.
ALTER TABLE agent_outreach_targets
    DROP CONSTRAINT IF EXISTS agent_outreach_targets_refusal_reason_check;
ALTER TABLE agent_outreach_targets
    ADD CONSTRAINT agent_outreach_targets_refusal_reason_check
    CHECK (
        refusal_reason IS NULL
        OR refusal_reason = ANY (ARRAY[
            'route_inferred',
            'evidence_missing',
            'paid_placement',
            'sells_placement',
            'implausible_engagement',
            'indiscriminate_churn',
            'poor_fit',
            'too_small',
            'previously_refused',
            'off_topic'
        ])
    );

-- Retire the communities the size-only screen admitted.
--
-- These rows carry `screening_verdict = 'admitted'` purely on member count;
-- their place notes describe video games, object identification, treasure
-- hunting, or communities dedicated to a different artist. Re-screening
-- cannot fix them because the candidate query skips places that already have
-- a target row, so the verdict is corrected here. `status = 'discarded'`
-- keeps them out of every pool; the promoted-step tombstone in
-- community_promotion preserves a discarded row on the next sweep.
UPDATE agent_outreach_targets AS t
SET status = 'discarded',
    screening_verdict = 'refused',
    refusal_reason = 'off_topic',
    updated_at = now()
FROM discovery_places AS p
WHERE p.id = t.place_id
  AND t.target_kind = 'community'
  AND t.screening_verdict = 'admitted'
  AND (
      -- Communities about the game, the hobby, or curiosities — not music.
      lower(t.subreddit) IN (
          'metalgearsolid', 'whatisthisthing', 'artefactporn',
          'metaldetecting'
      )
      -- A community dedicated to a different artist is not a promotion venue:
      -- posting our material there is off-topic by the community's own rules.
      OR lower(t.subreddit) IN (
          'batushka', 'eodm', 'peripheryband', 'riversidepl', 'mglaband',
          'dieselboy'
      )
      -- Communities that exist to mock, or parody communities where
      -- self-promotion is itself the joke.
      OR lower(t.subreddit) IN ('blackmetalcringe', 'guitarcirclejerk')
      -- General regional communities with no music focus: a national sub is
      -- not a fan channel.
      OR lower(t.subreddit) IN ('poland', 'polska', 'music_buffalo')
  );
