//! Shared authentication primitives for trusted API routes.

use axum::http::{HeaderMap, header::AUTHORIZATION};
use sha2::{Digest, Sha256};

/// Verifies a bearer token against a precomputed SHA-256 digest without
/// data-dependent early returns during digest comparison.
pub(crate) fn bearer_sha256_matches(headers: &HeaderMap, expected_hash: Option<[u8; 32]>) -> bool {
    let Some(expected_hash) = expected_hash else {
        return false;
    };
    let Some(raw_token) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };

    let candidate: [u8; 32] = Sha256::digest(raw_token.as_bytes()).into();
    constant_time_eq(&candidate, &expected_hash)
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, header::AUTHORIZATION};

    use super::*;

    #[test]
    fn bearer_hash_comparison_is_exact_and_fail_closed() {
        let expected: [u8; 32] = Sha256::digest(b"correct-key").into();
        let mut headers = HeaderMap::new();

        assert!(!bearer_sha256_matches(&headers, Some(expected)));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer correct-key"),
        );
        assert!(bearer_sha256_matches(&headers, Some(expected)));
        assert!(!bearer_sha256_matches(&headers, None));

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong-key"));
        assert!(!bearer_sha256_matches(&headers, Some(expected)));
    }
}
