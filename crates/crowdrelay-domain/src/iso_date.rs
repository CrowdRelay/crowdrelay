//! `time::Date` on the wire as `YYYY-MM-DD`.
//!
//! A bare `Date` serializes as serde's tuple, `[2026, 268]`: year and day of
//! year. The public night page printed that to anyone holding the link, and the
//! console's booking-agents panel ran `new Date([2026, 268])`, got an invalid
//! date, and offered the approach button for agents who had declined for the
//! season. Use `#[serde(with = "crowdrelay_domain::iso_date")]`, or
//! `iso_date::option` for an `Option<Date>`. `scripts/test_serialized_dates_v1.py`
//! enforces it for every write-only struct.

mod format {
    time::serde::format_description!(iso_date, Date, "[year]-[month]-[day]");
    pub use iso_date::*;
}

pub use format::*;

#[cfg(test)]
mod tests {
    #[derive(serde::Serialize)]
    struct Row {
        #[serde(with = "super")]
        day: time::Date,
        #[serde(with = "super::option")]
        maybe: Option<time::Date>,
    }

    #[test]
    fn a_date_is_an_iso_calendar_date() {
        let row = Row {
            day: time::macros::date!(2026 - 09 - 25),
            maybe: None,
        };
        assert_eq!(
            serde_json::to_string(&row).expect("serializes"),
            r#"{"day":"2026-09-25","maybe":null}"#
        );
    }
}
