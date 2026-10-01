use crate::acquisition::{CaptureOffer, FanCaptureContext};
/// Parses only a bounded, known navigation promise.
#[must_use]
pub fn parse_capture_context(raw: &str) -> Option<FanCaptureContext> {
    if raw.len() > 512 {
        return None;
    }
    serde_json::from_str::<FanCaptureContext>(raw)
        .ok()
        .filter(FanCaptureContext::is_valid)
}
fn capture_query(context: Option<&FanCaptureContext>) -> String {
    let Some(c) = context.filter(|c| c.is_valid()) else {
        return String::new();
    };
    let mut query = match c.offer {
        Some(CaptureOffer::Shows) => "&offer=shows".to_owned(),
        Some(CaptureOffer::Releases) => "&offer=releases".to_owned(),
        None => String::new(),
    };
    if let Some(event) = &c.event_slug {
        query.push_str("&event=");
        query.push_str(event);
    }
    if let Some(video) = &c.video_id {
        query.push_str("&video=");
        query.push_str(video);
    }
    query
}
#[cfg(test)]
mod capture_tests {
    use super::*;
    #[test]
    fn known_offer_has_local_query_and_invalid_resources_are_refused() {
        let c = parse_capture_context(r#"{"offer":"shows","event_slug":"real-show"}"#)
            .expect("context");
        assert_eq!(capture_query(Some(&c)), "&offer=shows&event=real-show");
        for raw in [
            r#"{"offer":"prize"}"#,
            r#"{"return_url":"https://evil.test"}"#,
            r#"{"event_slug":"../admin"}"#,
        ] {
            assert!(parse_capture_context(raw).is_none());
        }
    }
}
