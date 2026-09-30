//! Source-owned promotion restrictions and delivery evidence.

use serde_json::Value;

pub const PROMOTION_PLATFORMS: &[&str] = &[
    "reddit",
    "forum",
    "lemmy",
    "telegram",
    "discord",
    "facebook",
    "instagram",
    "x",
    "email",
    "signal_push",
];

/// Identify aliases of the same registered YouTube asset, not title matches.
pub fn youtube_video_id(metadata: &Value) -> Option<String> {
    let valid = |id: &str| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if let Some(id) = metadata
        .get("video_id")
        .and_then(Value::as_str)
        .filter(|id| valid(id))
    {
        return Some(id.to_owned());
    }
    for key in ["url", "listen_url", "video_url"] {
        let Some(url) = metadata.get(key).and_then(Value::as_str) else {
            continue;
        };
        let id = [
            "https://youtu.be/",
            "http://youtu.be/",
            "https://www.youtube.com/watch?v=",
            "https://youtube.com/watch?v=",
        ]
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix))
        .and_then(|suffix| suffix.split(['?', '&', '#', '/']).next());
        if let Some(id) = id.filter(|id| valid(id)) {
            return Some(id.to_owned());
        }
    }
    None
}

/// Missing policy on a video excludes Meta, not community distribution.
/// An explicit empty list permits Meta; malformed stored policy fails closed.
pub fn excluded_platforms(kind: &str, metadata: &Value) -> Vec<String> {
    if !metadata.is_object() {
        return PROMOTION_PLATFORMS
            .iter()
            .map(|platform| (*platform).to_owned())
            .collect();
    }
    match metadata.get("promotion_excluded_platforms") {
        None if kind == "video" || (kind == "release" && youtube_video_id(metadata).is_some()) => {
            vec!["facebook".into(), "instagram".into()]
        }
        None => Vec::new(),
        Some(value) => match value.as_array() {
            Some(items)
                if items.iter().all(|item| {
                    item.as_str()
                        .is_some_and(|platform| PROMOTION_PLATFORMS.contains(&platform))
                }) =>
            {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }
            _ => PROMOTION_PLATFORMS
                .iter()
                .map(|platform| (*platform).to_owned())
                .collect(),
        },
    }
}

pub fn platform_allowed(kind: &str, metadata: &Value, platform: &str) -> bool {
    PROMOTION_PLATFORMS.contains(&platform)
        && !excluded_platforms(kind, metadata)
            .iter()
            .any(|excluded| excluded == platform)
}

/// Surface-specific artifacts obey the same policy as direct promotion lanes.
pub fn artifact_allowed(
    artifact: crate::content_supply::ContentArtifactKind,
    excluded: &[String],
) -> bool {
    use crate::content_supply::ContentArtifactKind;
    let allowed = |platform: &str| !excluded.iter().any(|value| value == platform);
    match artifact {
        ContentArtifactKind::SignalPush => allowed("signal_push"),
        ContentArtifactKind::NewsletterBlock => allowed("email"),
        ContentArtifactKind::SocialStory => allowed("instagram") || allowed("facebook"),
        ContentArtifactKind::SocialFeed => ["telegram", "discord", "x", "instagram", "facebook"]
            .iter()
            .any(|platform| allowed(platform)),
        _ => true,
    }
}

/// A notification that work was requested is not delivery of the artifact.
pub fn artifact_delivered(metadata: &Value) -> bool {
    metadata.get("artifact_delivery").is_some_and(|delivery| {
        ["url", "surface", "reference"].iter().any(|key| {
            delivery
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn video_promotion_omits_meta_by_default() {
        assert!(!platform_allowed("video", &json!({}), "facebook"));
        assert!(!platform_allowed("video", &json!({}), "instagram"));
        assert!(platform_allowed("video", &json!({}), "forum"));
        assert!(platform_allowed("release", &json!({}), "facebook"));
    }

    #[test]
    fn explicit_policy_is_validated_and_malformed_policy_fails_closed() {
        assert!(!platform_allowed("video", &json!([]), "forum"));
        assert!(platform_allowed(
            "video",
            &json!({"promotion_excluded_platforms": []}),
            "facebook"
        ));
        assert!(!platform_allowed(
            "video",
            &json!({"promotion_excluded_platforms": ["forum"]}),
            "forum"
        ));
        assert!(!platform_allowed(
            "video",
            &json!({"promotion_excluded_platforms": "forum"}),
            "reddit"
        ));
        assert!(!platform_allowed(
            "video",
            &json!({"promotion_excluded_platforms": ["typo"]}),
            "reddit"
        ));
    }

    #[test]
    fn release_aliases_of_videos_share_safe_defaults_but_other_releases_do_not() {
        assert!(!platform_allowed(
            "release",
            &json!({"listen_url":"https://youtu.be/video-id"}),
            "facebook"
        ));
        assert!(!platform_allowed(
            "release",
            &json!({"url":"https://www.youtube.com/watch?v=video-id"}),
            "instagram"
        ));
        assert!(platform_allowed(
            "release",
            &json!({"listen_url":"https://open.spotify.com/album/record"}),
            "facebook"
        ));
        assert_eq!(
            youtube_video_id(&json!({"url":"https://youtu.be/video-id?feature=shared"})).as_deref(),
            Some("video-id")
        );
    }

    #[test]
    fn notification_receipts_do_not_prove_artifact_delivery() {
        assert!(!artifact_delivered(
            &json!({"provider": "discord", "provider_reference": "123"})
        ));
        assert!(!artifact_delivered(
            &json!({"artifact_delivery": {"url": " ", "surface": 1}})
        ));
        for key in ["url", "surface", "reference"] {
            assert!(artifact_delivered(
                &json!({"artifact_delivery": {key: "delivered-draft"}})
            ));
        }
    }
}
