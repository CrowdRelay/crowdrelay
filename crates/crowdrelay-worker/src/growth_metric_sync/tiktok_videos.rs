//! The band's own TikTok videos, as content sources.
//!
//! TikTok is where a first-seconds hook lives or dies, and until this the
//! only TikTok fact CrowdRelay held was a follower count: no video reached
//! the hook scorecard, the resonance ranking or the relay. The metric sync
//! already holds a fresh TikTok token when it reads the follower count, so
//! the video list is read on the same pass (`/v2/video/list/`, scope
//! `video.list` — a connection made before the scope was added answers with
//! a scope error, logged once per pass, and the owner reconnects).
//!
//! TikTok reports plays, not reach. Plays are written as `views` and as the
//! `reach` denominator the scorecard divides by: plays are at least reach,
//! so a keep rate read against them is conservative, never inflated. TikTok
//! reports no watch time here, so a TikTok video is judged on shares only.

use serde::Deserialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::social_post_source_sync::{PostEntry, SocialPostSourceSyncWorker, weighted_engagement};

const VIDEO_FIELDS: &str = "id,title,video_description,create_time,share_url,cover_image_url,\
like_count,comment_count,share_count,view_count";
const MAX_VIDEOS: u32 = 20;

#[derive(Deserialize)]
struct VideoList {
    data: Option<VideoData>,
}

#[derive(Deserialize)]
struct VideoData {
    #[serde(default)]
    videos: Vec<Video>,
}

#[derive(Deserialize)]
struct Video {
    id: String,
    title: Option<String>,
    video_description: Option<String>,
    create_time: Option<i64>,
    share_url: Option<String>,
    cover_image_url: Option<String>,
    like_count: Option<i64>,
    comment_count: Option<i64>,
    share_count: Option<i64>,
    view_count: Option<i64>,
}

fn is_tiktok_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_digit())
}

fn entry(video: &Video) -> Option<PostEntry> {
    if !is_tiktok_id(&video.id) {
        return None;
    }
    let caption = video
        .video_description
        .clone()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| video.title.clone());
    let title = caption
        .as_deref()
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .map_or_else(
            || "TikTok video".to_owned(),
            |line| line.trim().chars().take(80).collect(),
        );
    Some(PostEntry {
        external_id: video.id.clone(),
        title,
        url: video.share_url.clone(),
        posted_at: video
            .create_time
            .and_then(|at| OffsetDateTime::from_unix_timestamp(at).ok()),
        caption,
        media_url: None,
        media_id: None,
        media_type: Some("VIDEO".to_owned()),
        thumbnail_url: video.cover_image_url.clone(),
        engagement: weighted_engagement(video.like_count, video.comment_count, video.share_count),
        comments_count: video.comment_count,
    })
}

/// Reads the account's recent videos and files them as `tiktok:{id}`
/// social-post sources with their plays and shares. Errors are logged, not
/// returned: the follower count this pass already recorded stands.
pub(super) async fn sync_videos(
    pool: &PgPool,
    http: &reqwest::Client,
    workspace_id: Uuid,
    access_token: &str,
) {
    let response = http
        .post("https://open.tiktokapis.com/v2/video/list/")
        .query(&[("fields", VIDEO_FIELDS)])
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "max_count": MAX_VIDEOS }))
        .send()
        .await;
    let list: VideoList = match response {
        Ok(response) if response.status().is_success() => match response.json().await {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(error = %error.without_url(), "tiktok video list unreadable");
                return;
            }
        },
        Ok(response) => {
            tracing::warn!(
                status = %response.status(),
                "tiktok video list refused — reconnect TikTok so the grant carries video.list"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(error = %error.without_url(), "tiktok video list failed");
            return;
        }
    };
    let Ok(sync) = SocialPostSourceSyncWorker::new(pool.clone(), workspace_id, None) else {
        return;
    };
    let mut filed = 0usize;
    for video in list.data.map(|data| data.videos).unwrap_or_default() {
        let Some(entry) = entry(&video) else {
            continue;
        };
        if let Err(error) = sync.upsert_post("tiktok", &entry).await {
            tracing::warn!(%error, "tiktok video source upsert failed");
            continue;
        }
        // Plays as views and as the reach denominator (see the module doc);
        // shares as the keep signal. Nulls stay absent, never zero.
        let result = sqlx::query(
            r#"
            UPDATE content_sources
            SET metadata = metadata || jsonb_strip_nulls(jsonb_build_object(
                    'views', $3::bigint, 'reach', $3::bigint, 'shares', $4::bigint,
                    'insights_at', now()))
            WHERE workspace_id = $1 AND source_key = $2
            "#,
        )
        .bind(workspace_id)
        .bind(format!("tiktok:{}", entry.external_id))
        .bind(video.view_count)
        .bind(video.share_count)
        .execute(pool)
        .await;
        match result {
            Ok(_) => filed += 1,
            Err(error) => tracing::warn!(%error, "tiktok video counts not written"),
        }
    }
    tracing::info!(videos = filed, "tiktok videos filed as content sources");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_video_becomes_a_post_entry_with_its_description_as_the_voice() {
        let list: VideoList = serde_json::from_value(serde_json::json!({
            "data": { "videos": [
                { "id": "7412345678901234567", "title": "", "video_description": "Riff first, talk later\n#doom",
                  "create_time": 1_758_000_000, "share_url": "https://www.tiktok.com/@band/video/7412345678901234567",
                  "like_count": 120, "comment_count": 8, "share_count": 14, "view_count": 5400 },
                { "id": "../me", "title": "bad" }
            ] }
        }))
        .expect("video list");
        let videos = list.data.expect("data").videos;
        let first = entry(&videos[0]).expect("a valid video");
        assert_eq!(first.title, "Riff first, talk later");
        assert_eq!(first.media_type.as_deref(), Some("VIDEO"));
        assert_eq!(first.engagement, Some(120 + 8 * 3 + 14 * 5));
        assert!(
            entry(&videos[1]).is_none(),
            "a non-numeric id never becomes a source"
        );
    }
}
