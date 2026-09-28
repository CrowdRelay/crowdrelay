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

/// The instant as the event's own wall clock reads it.
///
/// `events.starts_at` is stored in UTC, and everything that formats a show for
/// a person — a fan's day-of email, a letter to a promoter — reads `.day()` and
/// `.hour()` off whatever offset the value carries. Until 2026-09-27 that was
/// UTC everywhere: a 20:00 Warsaw show was announced to fans as starting at
/// 18:00, and a show after midnight carried the previous day's date. Convert
/// at the read, where `events.timezone` is in hand; the instant is unchanged,
/// so comparisons and durations behave as before.
///
/// A name the bundled database does not know leaves the value as it was
/// rather than guessing a zone.
#[must_use]
pub fn at_event_timezone(at: time::OffsetDateTime, timezone: &str) -> time::OffsetDateTime {
    use time_tz::OffsetDateTimeExt;
    match time_tz::timezones::get_by_name(timezone.trim()) {
        Some(zone) => at.to_timezone(zone),
        None => at,
    }
}

/// A clock time as a person reads it, on `zone` and naming it:
/// "2026-09-19 18:00 (Europe/Warsaw)". A zone the bundled database does not
/// know leaves the time in UTC and says "UTC" — never a guessed zone.
#[must_use]
pub fn format_on_clock(at: time::OffsetDateTime, zone: &str) -> String {
    if is_known_iana_timezone(zone) {
        let local = at_event_timezone(at, zone);
        format!(
            "{} {:02}:{:02} ({})",
            local.date(),
            local.hour(),
            local.minute(),
            zone.trim()
        )
    } else {
        let utc = at.to_offset(time::UtcOffset::UTC);
        format!("{} {:02}:{:02} UTC", utc.date(), utc.hour(), utc.minute())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn an_evening_show_reads_on_the_local_clock() {
        // CEST, UTC+2: 18:00 UTC is the 20:00 doors a fan was told about.
        let local = at_event_timezone(datetime!(2026-09-26 18:00 UTC), "Europe/Warsaw");
        assert_eq!((local.hour(), local.minute()), (20, 0));
        assert_eq!(local, datetime!(2026-09-26 18:00 UTC));
        // CET, UTC+1, after the October change.
        let winter = at_event_timezone(datetime!(2026-11-14 19:30 UTC), "Europe/Warsaw");
        assert_eq!(winter.hour(), 20);
    }

    #[test]
    fn a_show_after_midnight_carries_the_local_date() {
        let local = at_event_timezone(datetime!(2026-10-09 23:30 UTC), "Europe/Warsaw");
        assert_eq!(local.date(), time::macros::date!(2026 - 10 - 10));
    }

    #[test]
    fn a_clock_time_names_its_zone() {
        let at = datetime!(2026-09-19 16:00 UTC);
        assert_eq!(
            format_on_clock(at, "Europe/Warsaw"),
            "2026-09-19 18:00 (Europe/Warsaw)"
        );
        assert_eq!(format_on_clock(at, "UTC"), "2026-09-19 16:00 UTC");
        assert_eq!(format_on_clock(at, "Mars/Olympus"), "2026-09-19 16:00 UTC");
    }

    #[test]
    fn an_unknown_zone_leaves_the_instant_as_it_was() {
        let at = datetime!(2026-10-09 23:30 UTC);
        assert_eq!(at_event_timezone(at, "Mars/Olympus").offset(), at.offset());
    }

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
