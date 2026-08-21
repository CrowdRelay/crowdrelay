-- Close the release -> playlist-pitching loop without colliding with the
-- existing 0064 beacon migration.
CREATE OR REPLACE FUNCTION viryaos_seed_release_playlist_outreach()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.milestone <> 'start_press' THEN
        RETURN NEW;
    END IF;

    INSERT INTO viryaos_outreach_opportunities (
        workspace_id, target_id, source, subject_kind, subject_key,
        template_key, relevance_basis_points, confidence_basis_points,
        active, observed_at, expires_at
    )
    SELECT
        target.workspace_id,
        target.id,
        'release_autopilot',
        'release',
        format('release:%s', NEW.release_id),
        'release.playlist.v1',
        GREATEST(7_500, LEAST(10_000, target.relationship_score * 100)),
        9_200,
        true,
        NEW.completed_at,
        GREATEST(plan.release_at + INTERVAL '14 days', NEW.completed_at + INTERVAL '14 days')
    FROM viryaos_outreach_targets AS target
    JOIN viryaos_release_plans AS plan
      ON plan.workspace_id = NEW.workspace_id
     AND plan.id = NEW.release_id
    WHERE target.workspace_id = NEW.workspace_id
      AND target.target_kind = 'playlist'
      AND target.active
      AND target.verified
      AND target.accepts_outreach
      AND NOT target.do_not_contact
    ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key)
    DO UPDATE SET
        template_key = EXCLUDED.template_key,
        relevance_basis_points = EXCLUDED.relevance_basis_points,
        confidence_basis_points = EXCLUDED.confidence_basis_points,
        active = true,
        observed_at = EXCLUDED.observed_at,
        expires_at = EXCLUDED.expires_at;

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS viryaos_release_playlist_outreach ON viryaos_release_milestones;
CREATE TRIGGER viryaos_release_playlist_outreach
AFTER INSERT ON viryaos_release_milestones
FOR EACH ROW
EXECUTE FUNCTION viryaos_seed_release_playlist_outreach();

-- Backfill releases that crossed start_press before this migration.
INSERT INTO viryaos_outreach_opportunities (
    workspace_id, target_id, source, subject_kind, subject_key,
    template_key, relevance_basis_points, confidence_basis_points,
    active, observed_at, expires_at
)
SELECT
    milestone.workspace_id,
    target.id,
    'release_autopilot',
    'release',
    format('release:%s', milestone.release_id),
    'release.playlist.v1',
    GREATEST(7_500, LEAST(10_000, target.relationship_score * 100)),
    9_200,
    true,
    milestone.completed_at,
    GREATEST(plan.release_at + INTERVAL '14 days', milestone.completed_at + INTERVAL '14 days')
FROM viryaos_release_milestones AS milestone
JOIN viryaos_release_plans AS plan
  ON plan.workspace_id = milestone.workspace_id
 AND plan.id = milestone.release_id
JOIN viryaos_outreach_targets AS target
  ON target.workspace_id = milestone.workspace_id
WHERE milestone.milestone = 'start_press'
  AND target.target_kind = 'playlist'
  AND target.active
  AND target.verified
  AND target.accepts_outreach
  AND NOT target.do_not_contact
ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key)
DO UPDATE SET
    template_key = EXCLUDED.template_key,
    relevance_basis_points = EXCLUDED.relevance_basis_points,
    confidence_basis_points = EXCLUDED.confidence_basis_points,
    active = true,
    observed_at = EXCLUDED.observed_at,
    expires_at = EXCLUDED.expires_at;