/// Deterministic pseudo-random roll from a string key. Maps the key's
/// hash to [0, 1). Used for the randomized holdout — the same decision
/// key always gets the same roll within one cycle, preventing flapping.
fn deterministic_roll(key: &str) -> f64 {
    // FNV-1a hash → u64 → [0, 1).
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in key.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // Map to [0, 1) using the upper 53 bits (mantissa precision of f64).
    f64::from_bits(0x3FF0_0000_0000_0000 | (hash & 0x000F_FFFF_FFFF_FFFF)) - 1.0
}
