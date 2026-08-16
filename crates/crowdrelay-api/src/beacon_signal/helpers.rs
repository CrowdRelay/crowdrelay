use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use getrandom::fill;
use sha2::{Digest, Sha256};

const MIN_RADIUS_KM: i32 = 10;
const MAX_RADIUS_KM: i32 = 500;

pub(super) fn random_token<const N: usize>() -> Option<String> {
    let mut bytes = [0_u8; N];
    fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn token_hash(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

pub(super) fn clean_locale(value: &str) -> Option<String> {
    let value = value.trim();
    let valid = matches!(value.len(), 2 | 5)
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.as_bytes().get(1).is_some_and(u8::is_ascii_lowercase)
        && (value.len() == 2
            || (value.as_bytes().get(2) == Some(&b'-')
                && value.as_bytes().get(3).is_some_and(u8::is_ascii_uppercase)
                && value.as_bytes().get(4).is_some_and(u8::is_ascii_uppercase)));
    valid.then(|| value.to_owned())
}

pub(super) fn clean_topics(values: Vec<String>) -> Option<Vec<String>> {
    let mut values = values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .collect::<Vec<_>>();
    if values.is_empty()
        || values.iter().any(|value| {
            !matches!(
                value.as_str(),
                "shows" | "press_materials" | "releases" | "interviews" | "accreditation"
            )
        })
    {
        return None;
    }
    values.sort();
    values.dedup();
    Some(values)
}

pub(super) fn valid_radius(value: i32) -> bool {
    (MIN_RADIUS_KM..=MAX_RADIUS_KM).contains(&value)
}

pub(super) fn valid_invite_token(value: &str) -> bool {
    let value = value.trim();
    (24..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
