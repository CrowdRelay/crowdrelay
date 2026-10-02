// Owned-social measurement tests — `include!`d from autopilot.rs so
// execution.rs stays under the modularity contract line limit.
#[cfg(test)]
mod owned_social_measurement_tests {
    use super::social_content_funnel_planned;
    use serde_json::json;

    #[test]
    fn social_post_fallback_plans_only_honest_clickable_rails() {
        assert!(social_content_funnel_planned(
            Some("social-post"),
            &json!({"platform":"facebook","text":"hello"})
        ));
        assert!(social_content_funnel_planned(
            Some("social-post"),
            &json!({"platform":"x","text":"hello"})
        ));
        assert!(!social_content_funnel_planned(
            Some("social-post"),
            &json!({"platform":"instagram","text":"hello"})
        ));
        assert!(social_content_funnel_planned(
            Some("social-post"),
            &json!({
                "platform":"instagram",
                "text":"hello",
                "cta_url":"https://band.example/signal/"
            })
        ));
        assert!(!social_content_funnel_planned(
            Some("press-pitch"),
            &json!({"platform":"facebook","text":"hello"})
        ));
    }
}
