#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "workspace_name": "Example Artist",
        "cities": [{
            "slug": "wroclaw",
            "name": "Wrocław",
            "country": "PL",
            "region": "Dolnośląskie",
            "lat": 51.1079,
            "lng": 17.0385
        }],
        "campaigns": [{
            "name": "Virya launch",
            "active": true,
            "smart_links": [{
                "slug": "listen",
                "destination_url": "https://virya.example/listen",
                "active": true
            }]
        }],
        "webhook_endpoints": [{
            "name": "automation",
            "url": "https://automation.example/hooks/crowdrelay",
            "signing_secret_ref": "docker:/run/secrets/crowdrelay-webhook",
            "timeout_ms": 10000,
            "max_attempts": 12,
            "active": true
        }]
    }"#;

    #[test]
    fn parses_a_complete_production_document() -> Result<(), Box<dyn std::error::Error>> {
        let spec = BootstrapSpec::parse(VALID, true)?;

        assert_eq!(spec.workspace_name, "Example Artist");
        assert_eq!(spec.cities.len(), 1);
        assert_eq!(spec.campaigns.len(), 1);
        assert_eq!(spec.webhook_endpoints.len(), 1);
        Ok(())
    }

    #[test]
    fn merch_discount_reward_rule_defaults_kind_for_backward_compatibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "3 qualified fans = 10% merch",
                "threshold": 3,
                "discount_percent": 10.0,
                "expires_days": 30,
                "code_prefix": "VIRYA",
                "active": true
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.reward_rules.len(), 1);
        assert!(matches!(
            spec.reward_rules[0].config,
            RewardRuleConfig::MerchDiscount { .. }
        ));
        Ok(())
    }

    #[test]
    fn parses_a_physical_item_reward_rule() -> Result<(), Box<dyn std::error::Error>> {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "5 qualified fans = free album",
                "threshold": 5,
                "kind": "physical_item",
                "item_name": "Virya — Signal (CD)",
                "sku": "virya-signal-cd",
                "expires_days": 60,
                "active": true
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.reward_rules.len(), 1);
        match &spec.reward_rules[0].config {
            RewardRuleConfig::PhysicalItem { item_name, sku } => {
                assert_eq!(item_name, "Virya — Signal (CD)");
                assert_eq!(sku, "virya-signal-cd");
            }
            RewardRuleConfig::MerchDiscount { .. } => panic!("expected a physical_item reward"),
        }
        Ok(())
    }

    #[test]
    fn rejects_physical_item_reward_rule_missing_required_fields() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "5 qualified fans = free album",
                "threshold": 5,
                "kind": "physical_item",
                "sku": "virya-signal-cd",
                "expires_days": 60,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("missing item_name must fail"),
            BootstrapSpecError::InvalidField {
                field: "reward_rules[].item_name"
            }
        );
    }

    #[test]
    fn rejects_reward_rule_with_fields_from_the_wrong_kind() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "5 qualified fans = free album",
                "threshold": 5,
                "kind": "physical_item",
                "item_name": "Virya — Signal (CD)",
                "sku": "virya-signal-cd",
                "discount_percent": 10.0,
                "expires_days": 60,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("mismatched kind fields must fail"),
            BootstrapSpecError::InvalidField {
                field: "reward_rules[].kind"
            }
        );
    }

    #[test]
    fn rejects_unknown_reward_rule_kind() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "Unknown reward",
                "threshold": 5,
                "kind": "vinyl_time_machine",
                "expires_days": 60,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("unknown kind must fail"),
            BootstrapSpecError::InvalidField {
                field: "reward_rules[].kind"
            }
        );
    }

    #[test]
    fn parses_and_validates_event_bootstrap_data() -> Result<(), Box<dyn std::error::Error>> {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [{
                "slug": "wroclaw",
                "name": "Wrocław",
                "country": "PL",
                "region": "Dolnośląskie",
                "lat": 51.1079,
                "lng": 17.0385
            }],
            "campaigns": [],
            "webhook_endpoints": [],
            "events": [{
                "slug": "virya-wroclaw-2027",
                "city_slug": "wroclaw",
                "title": "Virya — Wrocław",
                "description": "Viryatkowo live",
                "venue": "Example Club",
                "venue_address": "Example 1, Wrocław",
                "timezone": "Europe/Warsaw",
                "starts_at": "2027-06-20T18:00:00Z",
                "doors_at": "2027-06-20T17:00:00Z",
                "ends_at": "2027-06-20T21:00:00Z",
                "ticket_url": "https://virya.music/tickets/example",
                "listen_url": "https://virya.music/listen",
                "image_url": "https://virya.music/example.jpg",
                "trailer_url": "https://virya.music/example.mp4",
                "external_event_url": "https://virya.music/live/virya-wroclaw-2027",
                "status": "published"
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.events.len(), 1);
        Ok(())
    }

    #[test]
    fn parses_manual_admission_pool_for_a_published_event() -> Result<(), Box<dyn std::error::Error>>
    {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [{
                "slug": "example-city",
                "name": "Example City",
                "country": "PL",
                "region": "Opolskie",
                "lat": null,
                "lng": null
            }],
            "campaigns": [],
            "webhook_endpoints": [],
            "events": [{
                "slug": "example-show-2030",
                "city_slug": "example-city",
                "title": "Example Tour",
                "description": null,
                "venue": "Example Venue",
                "venue_address": "123 Example Street",
                "timezone": "Europe/Warsaw",
                "starts_at": "2030-09-05T19:30:00+02:00",
                "doors_at": null,
                "ends_at": null,
                "ticket_url": null,
                "listen_url": null,
                "image_url": null,
                "trailer_url": null,
                "external_event_url": "https://example.test/event",
                "status": "published"
            }],
            "admission_pools": [{
                "event_slug": "example-show-2030",
                "slug": "example-guest-list",
                "name": "Example guest list",
                "capacity": 4,
                "active": true
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.events.len(), 1);
        assert_eq!(spec.admission_pools.len(), 1);
        assert_eq!(spec.admission_pools[0].capacity, 4);
        assert!(spec.admission_pools[0].active);
        Ok(())
    }

    #[test]
    fn rejects_zero_capacity_admission_pool() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "events": [],
            "admission_pools": [{
                "event_slug": "example-show-2030",
                "slug": "example-guest-list",
                "name": "Example guest list",
                "capacity": 0,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("zero capacity must fail"),
            BootstrapSpecError::InvalidField {
                field: "admission_pools[].capacity"
            }
        );
    }

    #[test]
    fn rejects_unknown_fields_at_nested_levels() {
        let document = VALID.replace(
            r#""active": true
            }]"#,
            r#""active": true,
                "raffle": true
            }]"#,
        );

        assert_eq!(
            BootstrapSpec::parse(&document, true).expect_err("unknown field must fail"),
            BootstrapSpecError::InvalidJson
        );
    }

    #[test]
    fn production_requires_https_without_echoing_the_url() {
        let insecure = "http://private.example/secret-path";
        let document = VALID.replace("https://virya.example/listen", insecure);
        let error = BootstrapSpec::parse(&document, true)
            .expect_err("HTTP redirect must be rejected in production");

        assert_eq!(
            error,
            BootstrapSpecError::HttpsRequired {
                field: "campaigns[].smart_links[].destination_url"
            }
        );
        assert!(!format!("{error:?}").contains(insecure));
        assert!(BootstrapSpec::parse(&document, false).is_ok());
    }

    #[test]
    fn rejects_duplicate_smart_link_slugs_across_campaigns() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [
                {
                    "name": "First",
                    "active": true,
                    "smart_links": [{
                        "slug": "listen",
                        "destination_url": "https://one.example",
                        "active": true
                    }]
                },
                {
                    "name": "Second",
                    "active": true,
                    "smart_links": [{
                        "slug": "listen",
                        "destination_url": "https://two.example",
                        "active": true
                    }]
                }
            ],
            "webhook_endpoints": []
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("duplicate slug must fail"),
            BootstrapSpecError::Duplicate {
                kind: "smart-link slug"
            }
        );
    }

    #[test]
    fn rejects_ambiguous_city_slugs_and_coordinate_halves() {
        let duplicate = r#"{
            "workspace_name": "Example Artist",
            "cities": [
                {
                    "slug": "springfield",
                    "name": "Springfield",
                    "country": "US",
                    "region": null,
                    "lat": null,
                    "lng": null
                },
                {
                    "slug": "springfield",
                    "name": "Springfield",
                    "country": "CA",
                    "region": null,
                    "lat": null,
                    "lng": null
                }
            ],
            "campaigns": [],
            "webhook_endpoints": []
        }"#;
        assert_eq!(
            BootstrapSpec::parse(duplicate, true).expect_err("duplicate city slug must fail"),
            BootstrapSpecError::Duplicate { kind: "city slug" }
        );

        let half = VALID.replace("\"lng\": 17.0385", "\"lng\": null");
        assert_eq!(
            BootstrapSpec::parse(&half, true).expect_err("coordinate half must fail"),
            BootstrapSpecError::InvalidField {
                field: "cities[].lat/lng"
            }
        );
    }

    #[test]
    fn rejects_invalid_secret_references_and_webhook_fragments() {
        let bad_reference = VALID.replace(
            "docker:/run/secrets/crowdrelay-webhook",
            "docker ref with spaces",
        );
        assert_eq!(
            BootstrapSpec::parse(&bad_reference, true).expect_err("bad reference must fail"),
            BootstrapSpecError::InvalidField {
                field: "webhook_endpoints[].signing_secret_ref"
            }
        );

        let fragment = VALID.replace(
            "https://automation.example/hooks/crowdrelay",
            "https://automation.example/hooks/crowdrelay#ignored",
        );
        assert_eq!(
            BootstrapSpec::parse(&fragment, true).expect_err("fragment must fail"),
            BootstrapSpecError::InvalidField {
                field: "webhook_endpoints[].url"
            }
        );
    }

    #[test]
    fn debug_redacts_names_urls_and_secret_references() -> Result<(), Box<dyn std::error::Error>> {
        let spec = BootstrapSpec::parse(VALID, true)?;
        let rendered = format!("{spec:?}");

        assert!(!rendered.contains("Example Artist"));
        assert!(!rendered.contains("virya.example"));
        assert!(!rendered.contains("crowdrelay-webhook"));
        assert!(rendered.contains("smart_link_count"));
        Ok(())
    }

    #[test]
    fn rejects_oversized_documents_before_deserialization() {
        let document = "x".repeat(MAX_DOCUMENT_BYTES + 1);
        assert_eq!(
            BootstrapSpec::parse(&document, false).expect_err("oversized document must fail"),
            BootstrapSpecError::DocumentTooLarge {
                max_bytes: MAX_DOCUMENT_BYTES
            }
        );
    }

    #[test]
    fn change_summary_is_idempotency_friendly() {
        assert!(BootstrapChanges::default().is_empty());
        assert_eq!(BootstrapChanges::default().total(), 0);

        let changes = BootstrapChanges {
            workspaces: 1,
            smart_links: 2,
            ..BootstrapChanges::default()
        };
        assert!(!changes.is_empty());
        assert_eq!(changes.total(), 3);
    }
}
