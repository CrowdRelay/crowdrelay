//! Which social platforms the executor may publish to itself.
//!
//! `social_auto_post` is the tenant's master switch: off and every drafted
//! post waits for a person. When it is on, this vocabulary decides the lane
//! per platform — a platform in `social_autopost_platforms` goes to the
//! automatic queue (the machine posts and registers it), a platform outside
//! goes to the human queue (`awaiting_manual_post`) with a named reason.
//!
//! The set is deliberately narrower than every platform a draft can name:
//! it lists only platforms with a real first-party publish path. X is not
//! here — its write API sits behind a paid tier this stack does not hold,
//! so an `x` draft always lands in the human queue no matter what the
//! setting says. Reddit never appears either: it is read-only by policy.

/// Platforms that can ever publish automatically — the only values
/// `social_autopost_platforms` accepts. Adding one requires a working
/// executor path first; accepting a platform the worker cannot post would
/// let the setting promise a lane that does not exist.
pub const AUTOPOST_CAPABLE_PLATFORMS: [&str; 3] = ["facebook", "instagram", "telegram"];

/// The default when the setting is absent: everything the machine can do,
/// gated by `social_auto_post`. Preserves the pre-per-platform semantics —
/// turning the master switch on used to mean "publish where we can".
pub const DEFAULT_AUTOPOST_PLATFORMS: &[&str] = &AUTOPOST_CAPABLE_PLATFORMS;

/// Parses `social_autopost_platforms` — a comma list drawn entirely from
/// [`AUTOPOST_CAPABLE_PLATFORMS`], deduplicated in first-seen order. `None`
/// means at least one token is a platform the machine cannot post itself;
/// a refused-shape stored value reads as unset, so the default applies.
#[must_use]
pub fn parse_autopost_platforms(raw: &str) -> Option<Vec<String>> {
    let mut platforms: Vec<String> = Vec::new();
    for token in raw.split(',') {
        let platform = token.trim().to_ascii_lowercase();
        if !AUTOPOST_CAPABLE_PLATFORMS.contains(&platform.as_str()) {
            return None;
        }
        if !platforms.contains(&platform) {
            platforms.push(platform);
        }
    }
    if platforms.is_empty() {
        return None;
    }
    Some(platforms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_subset_and_dedupes() {
        assert_eq!(
            parse_autopost_platforms("Instagram, telegram,instagram"),
            Some(vec!["instagram".to_owned(), "telegram".to_owned()])
        );
    }

    #[test]
    fn refuses_platforms_the_machine_cannot_post() {
        // X's write API is paid-tier; reddit is read-only by policy. Writing
        // either would promise an automatic lane that does not exist.
        assert_eq!(parse_autopost_platforms("x"), None);
        assert_eq!(parse_autopost_platforms("reddit"), None);
        assert_eq!(parse_autopost_platforms("facebook,x"), None);
        assert_eq!(parse_autopost_platforms(""), None);
    }
}
