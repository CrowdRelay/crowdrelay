//! The weekly join-ask insert (§5): filing `social_posts` rows for
//! `social.join_ask.publish` actions that reached `succeeded`.
//!
//! Split out of `social_post_executor.rs` so the parent stays inside the
//! source-size ratchet. The claim shape is the agent-draft insert's own:
//! same `smart_links` bind, same `ON CONFLICT (action_id)`, same platform
//! guard — the differences are that a join-ask is raised by the evaluator
//! rather than a worker task (so there is no `agent_service_tasks` row to
//! join through) and that its payload fields sit at the top level instead
//! of under `draft`.

use super::{SocialPostExecutorError, SocialPostExecutorWorker};

impl SocialPostExecutorWorker {
    /// Files a `pending` row for every succeeded join-ask action that does
    /// not have one yet.
    ///
    /// The platform guard is the set this executor claims — `instagram`,
    /// `facebook`, `x`. A telegram/discord ask is held at the evaluator
    /// (`JoinAskHold::NoExecutor`), so it should never reach this insert;
    /// the guard is still restated here because a row filed for a platform
    /// nothing claims would sit `pending` forever, which reads as a stuck
    /// job rather than a declared boundary.
    pub(super) async fn file_join_ask_posts(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), SocialPostExecutorError> {
        sqlx::query(
            r#"
            INSERT INTO social_posts (workspace_id, action_id, platform, content, smart_link, smart_link_id, status)
            SELECT
                $1,
                a.id,
                a.payload->>'platform',
                jsonb_build_object(
                    'platform', a.payload->>'platform',
                    'text', a.payload->>'text',
                    'cta_url', a.payload->>'cta_url',
                    'join_ask', true
                ),
                COALESCE('/l/' || link.slug, a.payload->>'cta_url'),
                link.id,
                'pending'
            FROM autopilot_actions a
            LEFT JOIN smart_links link
              ON link.workspace_id = a.workspace_id
             AND link.slug = substring(a.payload->>'cta_url' from '/l/([a-zA-Z0-9][a-zA-Z0-9_-]*)$')
             -- Same own-namespace guard as the draft insert: only a `/l/`
             -- path or the tenant origin may bind an existing link. A bare
             -- destination is wrapped by `resolve_tracked_link` below.
             AND (a.payload->>'cta_url' LIKE '/l/%'
                  OR a.payload->>'cta_url' LIKE $2 || '/l/%')
            WHERE a.workspace_id = $1
              AND a.action_kind = 'social.join_ask.publish'
              AND a.status = 'succeeded'
              AND a.payload->>'platform' IN ('instagram', 'facebook', 'x')
              AND NOT EXISTS (
                  SELECT 1 FROM social_posts sp WHERE sp.action_id = a.id
              )
            ON CONFLICT (action_id) DO NOTHING
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(self.public_origin.trim_end_matches('/'))
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}
