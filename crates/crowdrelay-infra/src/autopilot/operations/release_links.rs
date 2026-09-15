//! Tracked links for releases.
//!
//! Same rule as a show: nothing gets shared before there is a link that can be
//! counted. Kept in its own module because a release key is free text and the
//! slug column is not, so turning one into the other needs more care than the
//! two lines it looks like.

use super::*;

/// Turns an operator-supplied key into something the smart-link slug pattern
/// accepts, or returns `None` when nothing usable survives.
///
/// A release key is free text; the slug column is not, and the slug is unique
/// per workspace. Sanitising is therefore lossy in a way that matters: "Żmija"
/// and "Zmija" both reduce to the same letters, and the second release would
/// silently overwrite the first one's destination. Whenever anything was
/// replaced or truncated the slug carries a short digest of the original key,
/// which keeps two different releases apart while leaving clean ASCII keys
/// readable.
pub(in crate::autopilot) fn release_link_slug(source_key: &str) -> Option<String> {
    let cleaned: String = source_key
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    let bounded: String = trimmed.chars().take(100).collect();
    let bounded = bounded.trim_end_matches('-');
    if bounded.is_empty() || !bounded.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    // Lossless only when the key was already a clean lowercase ASCII slug.
    if bounded == source_key.to_ascii_lowercase() {
        return Some(format!("release-{bounded}"));
    }
    Some(format!("release-{bounded}-{:08x}", key_digest(source_key)))
}

/// FNV-1a over the original key. Not security-relevant — it exists only to keep
/// two keys that sanitise to the same letters from sharing one link.
fn key_digest(source_key: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in source_key.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Gives the release a tracked link, or repairs the one it has.
///
/// A release with no listen URL gets no link: a tracked route to nowhere is
/// worse than an untracked route to the right place, and a release plan often
/// exists before the music does.
///
/// The link is bound to the release's acquisition campaign — one campaign per
/// plan, created lazily here — so clicks and signups through it attribute to
/// the release rather than to ambient growth. The campaign exists only once a
/// link does; without a listen_url there is no path for either to prove.
pub(in crate::autopilot) async fn ensure_release_tracked_link(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    release_id: crowdrelay_domain::ReleasePlanId,
    source_key: &str,
    title: &str,
    listen_url: Option<&str>,
) -> Result<(), RepositoryError> {
    // Must satisfy smart_links.destination_url ~* '^https?://' — a looser gate
    // here turns a malformed listen_url into a CHECK violation that wedges the
    // whole milestone ladder on every retry, and a stricter one silently drops
    // a valid URL.
    let Some(destination) = listen_url.filter(|url| {
        url.get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
            || url
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
    }) else {
        return Ok(());
    };
    let Some(slug) = release_link_slug(source_key) else {
        return Ok(());
    };

    let campaign_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH existing AS (
            SELECT id FROM campaigns
            WHERE workspace_id = $1 AND release_plan_id = $2 AND active
            ORDER BY created_at
            LIMIT 1
        ), inserted AS (
            INSERT INTO campaigns (workspace_id, name, active, release_plan_id)
            SELECT $1, $3, true, $2
            WHERE NOT EXISTS (SELECT 1 FROM existing)
            RETURNING id
        )
        SELECT id FROM inserted UNION ALL SELECT id FROM existing LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(release_id.into_uuid())
    .bind(format!("{title} · release"))
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO smart_links (workspace_id, slug, destination_url, campaign_id, active)
        VALUES ($1, $2, $3, $4, true)
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            campaign_id = EXCLUDED.campaign_id,
            active = true
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&slug)
    .bind(destination)
    .bind(campaign_id)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
