//! Tracked-link binding for claimed social-post rows.
//!
//! Split out of `social_post_executor.rs` so both stay inside the
//! source-size ratchet. This is one job with one shape: a claimed row whose
//! draft named a destination gets a `smart_links` row and a
//! `social_posts.smart_link_id` binding inside the claim transaction, so the
//! click measurement has something to count through. A destination the
//! validator refuses stays untracked — the post still goes out, and its
//! measurement abandons as `no_tracked_link` rather than recording a zero
//! that was never observable.

use uuid::Uuid;

use super::{ClaimedAction, SocialPostExecutorError, SocialPostExecutorWorker};

impl SocialPostExecutorWorker {
    /// Mints the `smart_links` row for a claimed post's CTA and binds the
    /// post to it, inside the claim transaction.
    ///
    /// The slug is deterministic — `social-{action_id}` — so a reclaim after
    /// a crash upserts rather than duplicating. Returns without binding when
    /// the draft carried no usable destination: untracked is a state the
    /// measurement can name, not a defect to retry.
    pub(super) async fn resolve_tracked_link(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        row: &mut ClaimedAction,
    ) -> Result<(), SocialPostExecutorError> {
        let Some(cta_url) = row
            .cta_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        else {
            return Ok(());
        };
        let ws = self.workspace_id.into_uuid();
        // A draft that already names one of our links binds to it rather
        // than minting a second for the same post — the same rule the
        // insert-time join applies, kept for rows written before it existed.
        if let Some(slug) = slug_in_cta(cta_url, self.public_origin.trim_end_matches('/')) {
            let bound = sqlx::query_scalar::<_, Option<Uuid>>(
                r#"
                UPDATE social_posts AS post
                SET smart_link_id = link.id,
                    smart_link = '/l/' || link.slug,
                    updated_at = now()
                FROM smart_links AS link
                WHERE post.workspace_id = $1 AND post.action_id = $2
                  AND link.workspace_id = $1 AND link.slug = $3
                RETURNING post.smart_link_id
                "#,
            )
            .bind(ws)
            .bind(row.action_id)
            .bind(slug)
            .fetch_optional(&mut **tx)
            .await?;
            // A `/l/` path naming a slug we never minted stays untracked —
            // there is no redirect to count clicks through.
            if let Some(link_id) = bound.flatten() {
                row.smart_link = Some(format!("/l/{slug}"));
                row.smart_link_id = Some(link_id);
            }
            return Ok(());
        }
        // The destination came out of a language model — the same refusal
        // the community path applies: only the tenant's own origin may be
        // wrapped, or the post would redirect the band's domain to wherever
        // the model said.
        let destination = match crowdrelay_domain::acquisition::agent_smart_link_destination(
            cta_url,
            &[self.public_origin.as_str()],
        ) {
            Ok(destination) => destination,
            Err(refusal) => {
                tracing::warn!(
                    action_id = %row.action_id,
                    refusal = %refusal,
                    "refused a social-post link destination; the post goes out untracked"
                );
                return Ok(());
            }
        };
        let slug = format!("social-{}", row.action_id.simple());
        sqlx::query(
            r#"
            INSERT INTO smart_links
                (workspace_id, slug, destination_url, active, channel_source)
            VALUES ($1, $2, $3, true, $4)
            ON CONFLICT (workspace_id, slug) DO UPDATE SET
                destination_url = EXCLUDED.destination_url,
                active = true
            "#,
        )
        .bind(ws)
        .bind(&slug)
        .bind(destination.as_str())
        .bind(&row.platform)
        .execute(&mut **tx)
        .await?;
        let bound = sqlx::query_scalar::<_, Option<Uuid>>(
            r#"
            UPDATE social_posts
            SET smart_link_id = link.id,
                smart_link = '/l/' || link.slug,
                updated_at = now()
            FROM smart_links AS link
            WHERE social_posts.workspace_id = $1
              AND social_posts.action_id = $2
              AND link.workspace_id = $1
              AND link.slug = $3
            RETURNING social_posts.smart_link_id
            "#,
        )
        .bind(ws)
        .bind(row.action_id)
        .bind(&slug)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(link_id) = bound.flatten() {
            row.smart_link = Some(format!("/l/{slug}"));
            row.smart_link_id = Some(link_id);
        }
        Ok(())
    }

    /// The text that actually publishes — the draft body plus the tracked
    /// link when the post carries one. Appended before the publish guard
    /// runs, so the guard reviews what the audience will see: the URL is on
    /// the tenant's own origin, which the guard already approves. A draft
    /// that already inlined the link path is not handed it twice.
    pub(super) fn publish_body(&self, action: &ClaimedAction, base: &str) -> String {
        match (&action.smart_link, action.smart_link_id) {
            (Some(link), Some(_)) if link.starts_with("/l/") && !base.contains(link.as_str()) => {
                format!(
                    "{base}\n\n{}{link}",
                    self.public_origin.trim_end_matches('/')
                )
            }
            _ => base.to_owned(),
        }
    }
}

/// The `/l/` slug a draft's CTA already names, when it names one of ours.
///
/// Only two shapes count: a bare `/l/{slug}` path, or the same path under the
/// tenant's own public origin. Anything else — a foreign host's `/l/` path,
/// a URL that merely contains the marker — is a destination to validate, not
/// a link to inherit.
pub(crate) fn slug_in_cta<'a>(cta_url: &'a str, public_origin: &str) -> Option<&'a str> {
    let path = cta_url.strip_prefix(public_origin).unwrap_or(cta_url);
    let slug = path.strip_prefix("/l/")?;
    let valid = !slug.is_empty()
        && slug.len() <= 128
        && slug
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    valid.then_some(slug)
}

#[cfg(test)]
mod tests {
    use super::slug_in_cta;

    #[test]
    fn slug_in_cta_only_reads_our_link_namespace() {
        let origin = "https://virya.music";
        assert_eq!(slug_in_cta("/l/tour-2026", origin), Some("tour-2026"));
        assert_eq!(
            slug_in_cta("https://virya.music/l/tour-2026", origin),
            Some("tour-2026")
        );
        // A foreign host's `/l/` path is not our slug.
        assert_eq!(
            slug_in_cta("https://evil.example/l/tour-2026", origin),
            None
        );
        // Nor is a lookalike origin.
        assert_eq!(
            slug_in_cta("https://virya.music.evil.example/l/x", origin),
            None
        );
        // Nor a bare destination.
        assert_eq!(slug_in_cta("https://virya.music/join", origin), None);
        // Nor a malformed slug.
        assert_eq!(slug_in_cta("/l/", origin), None);
        assert_eq!(slug_in_cta("/l/has space", origin), None);
    }
}
