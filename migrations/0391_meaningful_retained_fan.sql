-- Canonical meaningful retained fan predicate.
--
-- The causal Y30 observer and channel/source ranking used to disagree:
-- DurableFanGrowth30d counted an attributed account that merely remained
-- active for thirty days, while channel ROI additionally required consent and
-- meaningful behaviour. The optimizer could therefore learn "success" from a
-- silent account that the source selector correctly refused to call retained.
--
-- One function now owns that product meaning. A retained fan is:
--   * an attributed conversion old enough for a full 30-day maturity window;
--   * still an active account at observation time;
--   * currently consented to marketing at observation time; and
--   * observed doing a canonical first-party meaningful action in the current
--     30-day activity window, never before the conversion's D30 boundary.
--
-- The observation timestamp is explicit so historical measurements are
-- deterministic and future rows cannot leak into an earlier outcome.

CREATE OR REPLACE FUNCTION fan_is_meaningfully_retained(
    p_workspace_id uuid,
    p_fan_id uuid,
    p_acquired_at timestamptz,
    p_observed_at timestamptz
)
RETURNS boolean
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    SELECT
        p_workspace_id IS NOT NULL
        AND p_fan_id IS NOT NULL
        AND p_acquired_at IS NOT NULL
        AND p_observed_at IS NOT NULL
        AND p_acquired_at <= p_observed_at - INTERVAL '30 days'
        AND EXISTS (
            SELECT 1
            FROM fans AS fan
            WHERE fan.workspace_id = p_workspace_id
              AND fan.id = p_fan_id
              AND fan.status = 'active'
              AND COALESCE(
                  (
                      SELECT consent.granted
                      FROM fan_consents AS consent
                      WHERE consent.workspace_id = fan.workspace_id
                        AND consent.fan_id = fan.id
                        AND consent.purpose = 'marketing'
                        AND consent.recorded_at <= p_observed_at
                      ORDER BY consent.recorded_at DESC, consent.id DESC
                      LIMIT 1
                  ),
                  false
              )
              AND fan_has_meaningful_action_between(
                  fan.workspace_id,
                  fan.id,
                  fan.normalized_email,
                  GREATEST(
                      p_acquired_at + INTERVAL '30 days',
                      p_observed_at - INTERVAL '30 days'
                  ),
                  p_observed_at + INTERVAL '1 microsecond'
              )
        );
$$;

COMMENT ON FUNCTION fan_is_meaningfully_retained(uuid, uuid, timestamptz, timestamptz) IS
    'Canonical North-Star retention predicate: mature attributed conversion, active account, current consent, and meaningful post-D30 first-party action.';
