//! Shared authentication primitives for trusted API routes.

use axum::http::{HeaderMap, header::AUTHORIZATION};
use sha2::{Digest, Sha256};

/// Verifies a bearer token against a precomputed SHA-256 digest without
/// data-dependent early returns during digest comparison.
pub(crate) fn bearer_sha256(headers: &HeaderMap) -> Option<[u8; 32]> {
    let raw_token = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))?;
    if raw_token.is_empty() || raw_token.len() > 512 {
        return None;
    }
    Some(Sha256::digest(raw_token.as_bytes()).into())
}

/// Accepts either the active credential or its immediately preceding value,
/// so operators can rotate static bearer keys without a rejection window.
/// Both comparisons stay constant time; `None` on both sides fails closed.
pub(crate) fn bearer_sha256_matches_either(
    headers: &HeaderMap,
    expected_hash: Option<[u8; 32]>,
    previous_hash: Option<[u8; 32]>,
) -> bool {
    let Some(candidate) = bearer_sha256(headers) else {
        return false;
    };
    let matched_current =
        expected_hash.is_some_and(|expected| constant_time_eq(&candidate, &expected));
    let matched_previous =
        previous_hash.is_some_and(|previous| constant_time_eq(&candidate, &previous));
    matched_current | matched_previous
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

        assert!(!bearer_sha256_matches_either(
            &headers,
            Some(expected),
            None
        ));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer correct-key"),
        );
        assert!(bearer_sha256_matches_either(&headers, Some(expected), None));
        assert!(!bearer_sha256_matches_either(&headers, None, None));

        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong-key"));
        assert!(!bearer_sha256_matches_either(
            &headers,
            Some(expected),
            None
        ));

        let previous: [u8; 32] = Sha256::digest(b"previous-key").into();
        assert!(!bearer_sha256_matches_either(
            &headers,
            Some(expected),
            Some(previous)
        ));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer previous-key"),
        );
        assert!(bearer_sha256_matches_either(
            &headers,
            Some(expected),
            Some(previous)
        ));
        assert!(bearer_sha256_matches_either(&headers, None, Some(previous)));
        headers.remove(AUTHORIZATION);
        assert!(!bearer_sha256_matches_either(
            &headers,
            Some(expected),
            Some(previous)
        ));
        assert!(!bearer_sha256_matches_either(&headers, None, None));
    }
}
