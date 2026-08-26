use super::*;

fn sample_event() -> ShowModeEvent {
    ShowModeEvent {
        slug: "virya-live".to_owned(),
        title: "Virya Live".to_owned(),
        venue: Some("Club".to_owned()),
        starts_at: "2026-08-02T18:00:00Z".to_owned(),
    }
}

fn sample_pass() -> ShowModePass {
    ShowModePass {
        public_reference: "VRY-TICKET-1".to_owned(),
        holder_name: Some("Fan".to_owned()),
        holder_email_masked: "f***@example.com".to_owned(),
        ticket_type_name: Some("Regular".to_owned()),
        offline_eligible: true,
        qr_sha256: Some("ab".repeat(32)),
    }
}

#[test]
fn snapshot_checksum_is_stable_and_sensitive() {
    let event = sample_event();
    let pass = sample_pass();
    let checksum = snapshot_checksum(
        "snapshot-1",
        &event,
        "generated",
        "expires",
        std::slice::from_ref(&pass),
    );
    assert_eq!(
        checksum,
        snapshot_checksum(
            "snapshot-1",
            &event,
            "generated",
            "expires",
            std::slice::from_ref(&pass)
        )
    );
    let mut changed = pass;
    changed.offline_eligible = false;
    assert_ne!(
        checksum,
        snapshot_checksum("snapshot-1", &event, "generated", "expires", &[changed])
    );
}

#[test]
fn mutation_keys_reject_whitespace_and_control_bytes() {
    let mut headers = HeaderMap::new();
    headers.insert(IDEMPOTENCY_KEY.clone(), "valid-key-123".parse().unwrap());
    assert_eq!(mutation_key(&headers).unwrap(), "valid-key-123");
    headers.insert(IDEMPOTENCY_KEY.clone(), "bad key 123".parse().unwrap());
    assert!(matches!(
        mutation_key(&headers),
        Err(EcosystemError::BadRequest)
    ));
}

#[test]
fn email_masking_never_exposes_the_local_part() {
    assert_eq!(mask_email(Some("wojciech@example.com")), "w***@example.com");
    assert_eq!(mask_email(Some("invalid")), "—");
    assert_eq!(mask_email(None), "—");
}

#[test]
fn all_expected_feature_flags_have_safe_defaults() {
    assert_eq!(FLAG_KEYS.len(), 17);
    // The optional synesthesia module is the one deliberately-dark default:
    // a fresh tenant does not expose Virya's album surface until enabled.
    assert_eq!(flag_default("synesthesia_module"), Some(false));
    assert_eq!(flag_default("ticket_sales_enabled"), Some(true));
    assert_eq!(flag_default("unknown"), None);
}

#[test]
fn feature_flag_cache_is_strictly_bounded() {
    let now = Instant::now();
    let mut cache = FlagCache::new();
    for index in 0..(MAX_FLAG_CACHE_ENTRIES + 32) {
        insert_cached_flag(
            &mut cache,
            Uuid::from_u128(index as u128 + 1),
            "mailer_enabled",
            index % 2 == 0,
            now,
        );
    }
    assert_eq!(cache.len(), MAX_FLAG_CACHE_ENTRIES);
    assert!(cache.contains_key(&(
        Uuid::from_u128((MAX_FLAG_CACHE_ENTRIES + 32) as u128),
        "mailer_enabled"
    )));
}
