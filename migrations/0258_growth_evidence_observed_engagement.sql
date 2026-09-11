-- The engagement a dispatch earned, kept beside the fans it earned.
--
-- `agent_run_community_engagement_7d` already measures this: it sums the
-- latest score across every post made to a community in the week after a
-- dispatch. The measurement resolves, records its value on its own row, and
-- feeds nothing. Only the fan-growth and Signal-install kinds write back into
-- `viryaos_growth_evidence`, so engagement -- the fastest and highest-volume
-- signal this product can observe -- was measured and discarded.
--
-- That matters because of the arithmetic. Fans arrive at roughly one every
-- two days, so a fourteen-day incremental measurement has an expected effect
-- of about seven against Poisson noise of about two and a half: detecting a
-- twenty percent lift needs dozens of paired observations, which this tenant
-- will not have this year. Upvotes and comments arrive in hours, in tens, per
-- post. They are not the North Star and must never be mistaken for it, but
-- they are the only signal with enough volume to learn a ranking from while
-- the fan count is still counted on two hands.
--
-- No posterior is fitted from this yet, deliberately. There are zero rows in
-- `community_post_metrics` today because no post has ever been published, and
-- a model fitted on nothing is the "capability declared, consumer absent"
-- pattern this codebase keeps paying for. The column exists so that history
-- accumulates from the first published post rather than starting the day
-- somebody decides to build the model.
ALTER TABLE viryaos_growth_evidence
  ADD COLUMN IF NOT EXISTS observed_engagement DOUBLE PRECISION;

COMMENT ON COLUMN viryaos_growth_evidence.observed_engagement IS
  'Summed post score in the 7 days after the dispatch. Engagement, not reach '
  'and not fans: a proxy with volume, recorded so a ranking model can be '
  'fitted on real history when there is any.';
