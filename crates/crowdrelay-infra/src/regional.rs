//! Shared validation for tenant regional settings used by API and workers.

/// Returns true only for a bounded, explicit `Area/Location` name present in
/// the bundled IANA timezone database.
#[must_use]
pub fn is_known_iana_timezone(value: &str) -> bool {
    let value = value.trim();
    (3..=64).contains(&value.len())
        && value.contains('/')
        && value.is_ascii()
        && time_tz::timezones::get_by_name(value).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_known_explicit_iana_zones() {
        assert!(is_known_iana_timezone("Europe/Warsaw"));
        assert!(is_known_iana_timezone("America/New_York"));
    }

    #[test]
    fn rejects_shape_only_or_non_explicit_zones() {
        assert!(!is_known_iana_timezone("Mars/Olympus"));
        assert!(!is_known_iana_timezone("UTC"));
        assert!(!is_known_iana_timezone("../Europe/Warsaw"));
        assert!(!is_known_iana_timezone(""));
    }
}
