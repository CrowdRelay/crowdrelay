use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Exponential delay with deterministic ±20% jitter.
///
/// Deterministic jitter avoids an OS RNG dependency while still spreading
/// retries for different deliveries and attempt numbers.
pub(super) fn retry_delay(
    base: Duration,
    cap: Duration,
    attempt_number: i32,
    delivery_id: Uuid,
) -> Duration {
    let base_ms = base.as_millis().max(1);
    let cap_ms = cap.as_millis().max(base_ms);
    let exponent = u32::try_from(attempt_number.saturating_sub(1))
        .unwrap_or(0)
        .min(63);
    let exponential_ms = base_ms.saturating_mul(1_u128 << exponent).min(cap_ms);
    let jitter_span = (exponential_ms / 5).max(1);

    let mut hasher = Sha256::new();
    hasher.update(delivery_id.as_bytes());
    hasher.update(attempt_number.to_be_bytes());
    let digest = hasher.finalize();
    let sample = digest
        .iter()
        .take(std::mem::size_of::<u64>())
        .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
    let window = jitter_span.saturating_mul(2).saturating_add(1);
    let offset = i128::from(sample % u64::try_from(window).unwrap_or(u64::MAX))
        - i128::try_from(jitter_span).unwrap_or(i128::MAX);
    let jittered_ms = i128::try_from(exponential_ms)
        .unwrap_or(i128::MAX)
        .saturating_add(offset)
        .clamp(1, i128::try_from(cap_ms).unwrap_or(i128::MAX));

    Duration::from_millis(u64::try_from(jittered_ms).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_is_deterministic_and_capped() {
        let id = Uuid::from_u128(42);
        let base = Duration::from_secs(2);
        let cap = Duration::from_secs(30);

        let first = retry_delay(base, cap, 3, id);
        assert_eq!(first, retry_delay(base, cap, 3, id));
        assert!((Duration::from_millis(6_400)..=Duration::from_millis(9_600)).contains(&first));

        let capped = retry_delay(base, cap, 50, id);
        assert!(capped <= cap);
        assert!(capped >= Duration::from_secs(24));
    }

    #[test]
    fn different_deliveries_do_not_share_every_retry_slot() {
        let first = retry_delay(
            Duration::from_secs(1),
            Duration::from_secs(60),
            4,
            Uuid::from_u128(1),
        );
        let second = retry_delay(
            Duration::from_secs(1),
            Duration::from_secs(60),
            4,
            Uuid::from_u128(2),
        );

        assert_ne!(first, second);
    }
}
