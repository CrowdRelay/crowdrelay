fn validate_start(payload: &StartRunRequest) -> Result<(), ()> {
    if payload.campaign_slug != CAMPAIGN_SLUG
        || payload.install_id.len() < 24
        || payload.install_id.len() > 128
        || !payload
            .install_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        || payload.app_version.trim().is_empty()
        || payload.app_version.len() > 64
        || (payload.attempt_id.is_some()
            && clean_attempt_id(payload.attempt_id.as_deref()).is_none())
        || (payload.locale.is_some() && clean_locale(payload.locale.as_deref()).is_none())
    {
        return Err(());
    }
    Ok(())
}

fn clean_attempt_id(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(value.to_owned())
}

fn clean_locale(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty()
        || value.len() > 35
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(value.to_owned())
}

fn bearer_hash(headers: &HeaderMap) -> Option<Vec<u8>> {
    let token = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?
        .trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Sha256::digest(token.to_ascii_lowercase().as_bytes()).to_vec())
}

fn random_token() -> Result<String, ()> {
    let mut bytes = [0_u8; 32];
    fill_random(&mut bytes).map_err(|_| ())?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_contract_is_fixed_and_ordered() {
        assert_eq!(ROOM_IDS.len(), 11);
        assert_eq!(ROOM_IDS.first(), Some(&"wave-of-uncertainty"));
        assert_eq!(ROOM_IDS.last(), Some(&"rise"));
    }

    #[test]
    fn public_identifiers_are_bounded() {
        assert!(clean_locale(Some("pl-PL")).is_some());
        assert!(clean_locale(Some("pl/PL")).is_none());
        assert_eq!(
            clean_attempt_id(Some("attempt_01-A")),
            Some("attempt_01-A".to_owned())
        );
        assert!(clean_attempt_id(Some("bad/attempt")).is_none());
    }

    #[test]
    fn leaderboard_names_normalize_and_match_database_bounds() {
        assert_eq!(
            leaderboard::normalize_leaderboard_name(None).expect("default name"),
            "anonymous"
        );
        assert_eq!(
            leaderboard::normalize_leaderboard_name(Some("  Wojtek   VIRYA  "))
                .expect("normalized name"),
            "Wojtek VIRYA"
        );
        assert!(leaderboard::normalize_leaderboard_name(Some("ab")).is_ok());
        assert!(leaderboard::normalize_leaderboard_name(Some("12345678901234567890")).is_ok());
        assert!(leaderboard::normalize_leaderboard_name(Some("a")).is_err());
        assert!(leaderboard::normalize_leaderboard_name(Some("123456789012345678901")).is_err());
        assert!(leaderboard::normalize_leaderboard_name(Some("bad\u{0007}name")).is_err());
    }
}
