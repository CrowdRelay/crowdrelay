//! Initial navigation promise; never consent, entitlement or an external URL.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOffer { Shows, Releases }

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanCaptureContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<CaptureOffer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_id: Option<String>,
}
impl FanCaptureContext {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.event_slug.as_ref().is_none_or(|s| !s.is_empty() && s.len() <= 120
            && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !s.starts_with('-') && !s.ends_with('-'))
        && self.video_id.as_ref().is_none_or(|s| s.len() == 11
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        && !(self.event_slug.is_some() && self.video_id.is_some())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_resource_context_rejects_redirects_and_arbitrary_promises() {
        for slug in ["//evil.test", "../admin", "x?token=y", "", "UPPER"] {
            assert!(!FanCaptureContext {event_slug:Some(slug.into()),..Default::default()}.is_valid());
        }
        assert!(FanCaptureContext {offer:Some(CaptureOffer::Shows),event_slug:Some("real-show".into()),video_id:None}.is_valid());
        assert!(serde_json::from_str::<FanCaptureContext>(r#"{"offer":"prize"}"#).is_err());
        assert!(serde_json::from_str::<FanCaptureContext>(r#"{"return_url":"https://evil.test"}"#).is_err());
    }
}
