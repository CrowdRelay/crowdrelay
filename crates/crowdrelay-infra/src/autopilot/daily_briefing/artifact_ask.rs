//! §6-B: pending-ask line discrimination for the daily briefing.
//!
//! A pending `content.artifact.request` used to read "Content artifact:
//! Signal push" every time — five artifact asks rendered as the same line.
//! The raw payload carries more than the typed variant needs, so the line
//! earns a discriminator: the source's own title when the producer attached
//! one, else the artifact kind plus the draft's opening, else the source
//! id's short form.

use crowdrelay_domain::content_supply::ContentArtifactKind;

pub(super) fn distinguish_artifact_ask(
    action_kind: &str,
    payload: &serde_json::Value,
    summary: String,
) -> String {
    if action_kind != "content.artifact.request" {
        return summary;
    }
    let field = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    if let Some(title) = field("source_title") {
        return format!("{summary} — {title}");
    }
    let draft_excerpt = field("title")
        .or_else(|| {
            payload
                .get("draft")
                .and_then(|draft| draft.get("text").or_else(|| draft.get("title")))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map(|text| text.chars().take(40).collect::<String>());
    if let Some(excerpt) = draft_excerpt {
        let kind = payload
            .get("artifact")
            .and_then(|raw| serde_json::from_value::<ContentArtifactKind>(raw.clone()).ok())
            .map_or_else(
                || field("artifact").unwrap_or("artifact").to_owned(),
                |kind| kind.label().to_owned(),
            );
        return format!("{summary} — {kind}: {excerpt}");
    }
    if let Some(source_id) = field("source_id") {
        let short: String = source_id.chars().take(8).collect();
        return format!("{summary} — {short}");
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §6-B: five pending signal-push asks — the real flood — must read as
    /// five different lines, not one line repeated.
    #[test]
    fn same_kind_artifact_asks_stay_distinguishable() {
        let titles = [
            "Kortrijk recap",
            "Fest review",
            "New single teaser",
            "Merch drop",
            "Tour dates",
        ];
        let lines: Vec<String> = titles
            .iter()
            .map(|title| {
                distinguish_artifact_ask(
                    "content.artifact.request",
                    &serde_json::json!({
                        "artifact": "signal_push",
                        "source_title": title,
                    }),
                    "Content artifact".to_owned(),
                )
            })
            .collect();
        let mut dedup = lines.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), 5, "five asks collapsed: {lines:?}");
        assert!(
            lines[0].contains("Kortrijk recap"),
            "the source title never made the line: {}",
            lines[0]
        );
    }

    /// No source title — the draft's opening distinguishes; no draft — the
    /// source id's short form does. Other action kinds pass through.
    #[test]
    fn artifact_ask_falls_back_to_draft_then_source_id() {
        let with_draft = distinguish_artifact_ask(
            "content.artifact.request",
            &serde_json::json!({
                "artifact": "signal_push",
                "draft": {"text": "the excerpt that should appear in the line and then some"},
            }),
            "Content artifact".to_owned(),
        );
        assert!(
            with_draft.contains("Signal push: the excerpt that should appear"),
            "the draft fallback did not name kind and excerpt: {with_draft}"
        );
        let with_id = distinguish_artifact_ask(
            "content.artifact.request",
            &serde_json::json!({
                "artifact": "signal_push",
                "source_id": "deadbeefcafebabe12345678",
            }),
            "Content artifact".to_owned(),
        );
        assert!(with_id.contains("deadbeef"), "no source id: {with_id}");
        assert!(!with_id.contains("cafe"), "id not shortened: {with_id}");
        assert_eq!(
            distinguish_artifact_ask(
                "community.engage.request",
                &serde_json::json!({}),
                "Post".to_owned()
            ),
            "Post",
            "a non-artifact ask was rewritten"
        );
    }
}
