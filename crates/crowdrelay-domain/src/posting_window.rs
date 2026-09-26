//! When a community post may go out: while the community is awake, and not
//! the instant the draft became ready.
//!
//! A post that lands at 04:13 the community's time reaches nobody and says
//! "scheduled job" to anyone who looks; a post that appears the minute its
//! draft was approved, every time, says the same. Three rules:
//!
//! 1. **Active hours are learned when they can be.** The community's own
//!    `online_users` observations, grouped by hour of day: an hour at least
//!    half as busy as the community's busiest hour is active (at least the
//!    six busiest). It takes samples in at least [`MIN_SAMPLED_HOURS`]
//!    distinct hours to say anything.
//! 2. **Until then, local daytime by language** — a Polish community is awake
//!    on Warsaw's day, an English one on the American evening.
//! 3. **Nothing leaves on the dot.** A draft settles for 10-50 minutes after
//!    it is ready, and a window that opens gets a 0-45 minute offset. Both
//!    come from a per-post key, so every poll of the same post agrees.
//!
//! Everything is in UTC hours; a window may wrap midnight.

use time::{Duration, OffsetDateTime, Time};

/// Distinct sampled hours before learned hours replace the default.
pub const MIN_SAMPLED_HOURS: usize = 8;
/// A learned window is widened to at least this many hours.
const MIN_ACTIVE_HOURS: usize = 6;

/// Whether `hour` (0-23) is active. Out of range is not.
fn is_active(active: &[bool; 24], hour: u8) -> bool {
    active.get(usize::from(hour)).copied().unwrap_or(false)
}

fn mark(active: &mut [bool; 24], hour: usize) {
    if let Some(slot) = active.get_mut(hour) {
        *slot = true;
    }
}

/// Active hours from `(utc_hour, online_users)` samples, or `None` when the
/// samples cover too few hours to say.
#[must_use]
pub fn learned_active_hours(samples: &[(u8, i64)]) -> Option<[bool; 24]> {
    let mut totals = [(0i64, 0i64); 24];
    for &(hour, online) in samples {
        if let Some((sum, count)) = totals.get_mut(usize::from(hour)) {
            *sum = sum.saturating_add(online.max(0));
            *count += 1;
        }
    }
    let means: Vec<(usize, i64)> = totals
        .iter()
        .enumerate()
        .filter(|(_, (_, count))| *count > 0)
        .map(|(hour, (sum, count))| (hour, sum / count))
        .collect();
    if means.len() < MIN_SAMPLED_HOURS {
        return None;
    }
    // Relative to the community's own peak: an hour at least half as busy as
    // its busiest hour is an active hour. A median would call most of the
    // day active whenever the busy hours are fewer than half of them.
    let peak = means.iter().map(|&(_, mean)| mean).max().unwrap_or(0);
    if peak <= 0 {
        return None;
    }
    let mut active = [false; 24];
    for &(hour, mean) in &means {
        if mean.saturating_mul(2) >= peak {
            mark(&mut active, hour);
        }
    }
    // Too narrow to be a schedule: widen to the busiest few hours sampled.
    if active.iter().filter(|&&a| a).count() < MIN_ACTIVE_HOURS {
        let mut ranked = means.clone();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for &(hour, _) in ranked.iter().take(MIN_ACTIVE_HOURS) {
            mark(&mut active, hour);
        }
    }
    Some(active)
}

/// Local daytime by the community's language, in UTC hours.
#[must_use]
pub fn default_active_hours(language: Option<&str>) -> [bool; 24] {
    const CENTRAL_EUROPE: &[&str] = &[
        "pl", "de", "cs", "sk", "fr", "it", "es", "nl", "hu", "sv", "no", "da",
    ];
    let (start, end) = match language.map(|l| l.trim().to_ascii_lowercase()) {
        // Central Europe, roughly 09:00-23:00 local.
        Some(l)
            if CENTRAL_EUROPE
                .iter()
                .any(|code| l == *code || l.starts_with(&format!("{code}-"))) =>
        {
            (7usize, 21usize)
        }
        // English and unknown: the American day into its evening, which also
        // catches the European evening.
        _ => (13, 3),
    };
    let mut active = [false; 24];
    let mut hour = start;
    for _ in 0..24 {
        mark(&mut active, hour);
        if hour == end {
            break;
        }
        hour = (hour + 1) % 24;
    }
    active
}

/// How long to wait before this post may go out, or `None` for now.
///
/// `ready_at` is when the draft became publishable; `key` is a stable
/// per-post number (its id's bits) that fixes both offsets.
#[must_use]
pub fn wait_before_posting(
    now: OffsetDateTime,
    ready_at: OffsetDateTime,
    active: &[bool; 24],
    key: u128,
) -> Option<Duration> {
    if !active.iter().any(|&a| a) {
        return None;
    }
    let settle = Duration::minutes(10 + i64::try_from(key % 40).unwrap_or(0));
    let opening_offset = Duration::minutes(i64::try_from((key >> 16) % 46).unwrap_or(0));

    let earliest = (ready_at + settle).max(now);
    let at = if is_active(active, earliest.hour()) {
        earliest
    } else {
        // The next active hour's start, plus the offset. Some hour is active,
        // so this ends within a day.
        let mut probe =
            earliest.replace_time(Time::MIDNIGHT) + Duration::hours(i64::from(earliest.hour()));
        let mut opening = None;
        for _ in 0..24 {
            probe += Duration::hours(1);
            if is_active(active, probe.hour()) {
                opening = Some(probe + opening_offset);
                break;
            }
        }
        opening?
    };
    (at > now).then(|| at - now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn learned_hours_need_coverage_and_pick_the_busy_ones() {
        let sparse: Vec<(u8, i64)> = (0..5).map(|h| (h, 100)).collect();
        assert_eq!(learned_active_hours(&sparse), None);
        let samples: Vec<(u8, i64)> = (0..24u8)
            .map(|h| (h, if (16..=23).contains(&h) { 500 } else { 50 }))
            .chain((0..24u8).map(|h| (h, if (16..=23).contains(&h) { 450 } else { 60 })))
            .collect();
        let active = learned_active_hours(&samples).expect("full coverage");
        assert!(active[18] && active[23]);
        assert!(!active[4]);
    }

    #[test]
    fn polish_communities_wake_on_warsaw_time_english_on_the_american_evening() {
        let pl = default_active_hours(Some("pl"));
        assert!(pl[8] && pl[21] && !pl[23] && !pl[3]);
        let en = default_active_hours(Some("en"));
        assert!(en[14] && en[23] && en[0] && en[3] && !en[8]);
        assert_eq!(default_active_hours(None), en);
    }

    #[test]
    fn a_ready_draft_settles_before_it_goes() {
        let active = [true; 24];
        let now = datetime!(2026-09-26 12:00 UTC);
        let wait = wait_before_posting(now, now, &active, 7).expect("just ready: settle");
        assert_eq!(wait, Duration::minutes(17));
        // Ready an hour ago: no wait.
        assert_eq!(
            wait_before_posting(now, now - Duration::hours(1), &active, 7),
            None
        );
    }

    #[test]
    fn outside_the_window_it_waits_for_the_opening_plus_an_offset() {
        let pl = default_active_hours(Some("pl"));
        let now = datetime!(2026-09-26 02:30 UTC);
        let key: u128 = 5 << 16; // opening offset 5 minutes
        let wait = wait_before_posting(now, now - Duration::hours(3), &pl, key)
            .expect("asleep until 07:00");
        assert_eq!(now + wait, datetime!(2026-09-26 07:05 UTC));
    }

    #[test]
    fn the_same_post_always_gets_the_same_answer() {
        let pl = default_active_hours(Some("pl"));
        let now = datetime!(2026-09-26 02:30 UTC);
        let key = 0xDEAD_BEEF_u128;
        assert_eq!(
            wait_before_posting(now, now, &pl, key),
            wait_before_posting(now, now, &pl, key)
        );
    }
}
