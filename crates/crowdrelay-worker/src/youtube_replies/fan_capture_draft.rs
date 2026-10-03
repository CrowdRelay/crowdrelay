//! The capture comment, prepared for a person when nothing may post it.
//!
//! `seed_fan_capture_comment` posts the tracked join link under a fresh owned
//! video, but only under `social_auto_post`. That switch is off by design for a
//! band that runs its own socials, so the lane has never run: on 2026-10-03 a
//! Reddit-driven video had 84 visitors, every one sent straight to YouTube with
//! no way to join. Doing the preparation without the posting keeps what needs no
//! trust (a tracked link that exists, in the tenant's own join-ask words) and
//! leaves only the act of posting to a person.
//!
//! Nothing here writes to YouTube, sends anything or needs a token. It mints the
//! same smart link the automatic path would, and records the exact comment on
//! the video's source row so the attention board can show it ready to paste.
//! The automatic path skips a drafted source, so turning the switch on later
//! cannot put a second comment under a video a person already commented on.

use super::fan_capture::{capture_comment_text, capture_slug, signal_destination};
use sqlx::PgPool;
use uuid::Uuid;

/// Videos newer than this are worth a capture comment (same window as the
/// automatic path).
pub const DRAFT_WINDOW_DAYS: i32 = 30;
/// Drafts waiting on a person at once. A queue of ten paste-this chores is a
/// chore nobody does; the newest few are the ones that still matter.
pub const OPEN_DRAFTS_MAX: i64 = 3;

/// What the capture lane may do right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMode {
    /// Prepare the placement for a person; write nothing to YouTube.
    Prepare,
    /// Post the comment.
    Post,
}

/// Decides between preparing and posting.
///
/// Posting needs both halves: the deployment's publish gate
/// (`CROWDRELAY_SOCIAL_AUTO_POST`) and the tenant's own YouTube grant. The
/// broad `social_auto_post` switch is deliberately *not* an input: it covers
/// Pages and Instagram feeds, and it used to be the only key this lane read, so
/// an owner who wanted a join link under their own video had to hand over every
/// other surface with it. Turning the broad switch on no longer posts here, and
/// turning this grant on posts nothing anywhere else.
#[must_use]
pub fn capture_mode(deployment_gate: bool, youtube_grant: bool) -> CaptureMode {
    if deployment_gate && youtube_grant {
        CaptureMode::Post
    } else {
        CaptureMode::Prepare
    }
}

/// Minimum gap between two capture comments posted by the machine.
pub const MIN_SPACING_SECONDS: i64 = 6 * 60 * 60;

/// Whether enough time has passed since the last capture comment went up.
/// Nothing posted yet is always fine; a clock that says the last one is in the
/// future is a fault, treated as "not yet" so it can never be spammed through.
#[must_use]
pub fn spaced_enough(last_posted_unix: Option<i64>, now_unix: i64) -> bool {
    match last_posted_unix {
        None => true,
        Some(last) => now_unix >= last && now_unix - last >= MIN_SPACING_SECONDS,
    }
}

/// The tenant's variant for this video: stable per video, spread across the
/// tenant's own words. `None` only when the tenant wrote no variants.
#[must_use]
pub fn pick_variant(variants: &[String], source_id: Uuid) -> Option<&str> {
    let count = u128::try_from(variants.len())
        .ok()
        .filter(|count| *count > 0)?;
    let index = usize::try_from(source_id.as_u128() % count).ok()?;
    variants.get(index).map(String::as_str)
}

/// Prepares at most one capture comment draft. Returns whether it did.
///
/// # Errors
///
/// Propagates the database error.
pub async fn prepare_fan_capture_draft(
    pool: &PgPool,
    workspace_id: Uuid,
    variants: &[String],
    site_root: &str,
) -> Result<bool, sqlx::Error> {
    let open: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*) FROM content_sources cs
        WHERE cs.workspace_id = $1 AND cs.source_kind = 'video' AND cs.active
          AND cs.metadata ? 'fan_capture_draft_at'
          AND NOT (cs.metadata ? 'fan_capture_comment_posted_unix')
          AND cs.occurred_at > now() - make_interval(days => $2)
          -- A click on the capture link means the comment is already up.
          AND NOT EXISTS (
              SELECT 1 FROM smart_links l
              JOIN click_events c ON c.smart_link_id = l.id
              WHERE l.workspace_id = cs.workspace_id
                AND l.slug = cs.metadata->>'fan_capture_link_slug')
        "#,
    )
    .bind(workspace_id)
    .bind(DRAFT_WINDOW_DAYS)
    .fetch_one(pool)
    .await?;
    if open >= OPEN_DRAFTS_MAX {
        return Ok(false);
    }

    let mut tx = pool.begin().await?;
    let candidate: Option<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, source_key
        FROM content_sources
        WHERE workspace_id = $1
          AND source_kind = 'video'
          AND source_key LIKE 'youtube:%'
          AND active
          AND occurred_at > now() - make_interval(days => $2)
          AND NOT (metadata ? 'fan_capture_draft_at')
          AND NOT (metadata ? 'fan_capture_comment_posted_unix')
          -- Same rule as the automatic path: a description that already
          -- carries the owned CTA does not need a second one in comments.
          AND COALESCE(metadata->>'body', '') NOT ILIKE '%/signal%'
        ORDER BY occurred_at DESC
        LIMIT 1
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(workspace_id)
    .bind(DRAFT_WINDOW_DAYS)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((source_id, source_key)) = candidate else {
        tx.rollback().await?;
        return Ok(false);
    };
    let Some(video_id) = source_key
        .strip_prefix("youtube:")
        .filter(|id| super::is_youtube_id(id))
        .map(str::to_owned)
    else {
        tx.rollback().await?;
        return Ok(false);
    };
    let Some(campaign_id) = crowdrelay_infra::promotion_campaign::ensure_source_campaign(
        &mut tx,
        workspace_id,
        source_id,
    )
    .await?
    else {
        tx.rollback().await?;
        return Ok(false);
    };

    let slug = capture_slug(source_id);
    sqlx::query(
        r#"
        INSERT INTO smart_links
            (workspace_id, slug, destination_url, campaign_id, active,
             channel_source, channel_community, channel_creative)
        VALUES ($1, $2, $3, $4, true, 'youtube', $5, 'fan_capture_comment')
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            campaign_id = EXCLUDED.campaign_id,
            channel_source = EXCLUDED.channel_source,
            channel_community = EXCLUDED.channel_community,
            channel_creative = EXCLUDED.channel_creative,
            active = true
        "#,
    )
    .bind(workspace_id)
    .bind(&slug)
    .bind(signal_destination(site_root, source_id))
    .bind(campaign_id)
    .bind(format!("video:{video_id}"))
    .execute(&mut *tx)
    .await?;

    let public_link = format!("{}/l/{slug}", site_root.trim_end_matches('/'));
    sqlx::query(
        r#"
        UPDATE content_sources
        SET metadata = metadata || jsonb_build_object(
                'fan_capture_draft_at', now(),
                'fan_capture_draft_text', $3::text,
                'fan_capture_link_slug', $4::text),
            updated_at = now()
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(source_id)
    .bind(capture_comment_text(
        pick_variant(variants, source_id).unwrap_or_default(),
        &public_link,
    ))
    .bind(&slug)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posts_are_spaced_and_a_future_stamp_never_unlocks_one() {
        let now = 1_800_000_000;
        assert!(spaced_enough(None, now));
        assert!(!spaced_enough(Some(now - 60), now));
        assert!(!spaced_enough(Some(now - MIN_SPACING_SECONDS + 1), now));
        assert!(spaced_enough(Some(now - MIN_SPACING_SECONDS), now));
        assert!(
            !spaced_enough(Some(now + 10), now),
            "a fault is not a licence"
        );
    }

    #[test]
    fn each_video_gets_a_stable_variant_and_the_words_are_spread() {
        let variants: Vec<String> = ["a", "b", "c", "d"].map(str::to_owned).to_vec();
        let ids: Vec<Uuid> = (0..40_u128).map(Uuid::from_u128).collect();
        assert!(pick_variant(&[], ids[0]).is_none());
        // Stable: the same video always gets the same words, retry after retry.
        assert_eq!(
            pick_variant(&variants, ids[7]),
            pick_variant(&variants, ids[7])
        );
        // Spread: forty videos use all four variants, none dominates.
        let mut used = [0_u32; 4];
        for id in &ids {
            let v = pick_variant(&variants, *id).expect("variant");
            used[usize::from(v.as_bytes()[0] - b'a')] += 1;
        }
        assert!(used.iter().all(|n| *n == 10), "{used:?}");
        // One variant is still valid.
        assert_eq!(pick_variant(&["only".to_owned()], ids[3]), Some("only"));
    }

    #[test]
    fn only_both_halves_together_post() {
        assert_eq!(capture_mode(true, true), CaptureMode::Post);
        assert_eq!(capture_mode(true, false), CaptureMode::Prepare);
        assert_eq!(capture_mode(false, true), CaptureMode::Prepare);
        assert_eq!(capture_mode(false, false), CaptureMode::Prepare);
    }
}
