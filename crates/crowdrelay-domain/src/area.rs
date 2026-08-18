//! Pure domain types and invariants for tenant AREA game management.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

pub const MIN_AREA_RADIUS_METERS: i32 = 25;
pub const MAX_AREA_RADIUS_METERS: i32 = 500;
pub const MIN_AREA_CAPACITY: i32 = 1;
pub const MAX_AREA_CAPACITY: i32 = 500;
pub const MAX_AREA_CLUE_CHARS: usize = 2_000;
pub const MAX_AREA_COLLECTIBLE_LINE_CHARS: usize = 1_000;
pub const MAX_AREA_LABEL_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AreaDropStatus {
    Draft,
    Paused,
    Scheduled,
    Live,
    Ended,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AreaLocalizedClue {
    pub en: String,
    pub pl: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AreaCollectible {
    pub line: String,
    pub track: String,
    pub edition: String,
    pub riddle: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AreaDropDraft {
    pub number: String,
    pub city_id: Uuid,
    pub map_x: i16,
    pub map_y: i16,
    pub approximate_lat: f64,
    pub approximate_lng: f64,
    pub exact_lat: Option<f64>,
    pub exact_lng: Option<f64>,
    pub radius_meters: i32,
    pub max_claims: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
    pub clue: AreaLocalizedClue,
    pub collectible: AreaCollectible,
    pub sort_order: i32,
}

impl core::fmt::Debug for AreaDropDraft {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("AreaDropDraft")
            .field("number", &self.number)
            .field("city_id", &self.city_id)
            .field("map_x", &self.map_x)
            .field("map_y", &self.map_y)
            .field("approximate_lat", &self.approximate_lat)
            .field("approximate_lng", &self.approximate_lng)
            .field(
                "exact_location",
                &self.exact_lat.zip(self.exact_lng).map(|_| "[REDACTED]"),
            )
            .field("radius_meters", &self.radius_meters)
            .field("max_claims", &self.max_claims)
            .field("starts_at", &self.starts_at)
            .field("ends_at", &self.ends_at)
            .field("clue", &self.clue)
            .field("collectible", &self.collectible)
            .field("sort_order", &self.sort_order)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaValidationIssue {
    pub code: &'static str,
    pub field: &'static str,
    pub message: &'static str,
    pub confirmation_required: bool,
}

impl AreaDropDraft {
    #[must_use]
    pub fn validate(&self, existing_claims: i64) -> Vec<AreaValidationIssue> {
        let mut issues = Vec::new();
        if self.number.len() != 3 || !self.number.bytes().all(|byte| byte.is_ascii_digit()) {
            issues.push(issue(
                "INVALID_NUMBER",
                "number",
                "Drop number must contain exactly three digits.",
            ));
        }
        if !(0..=100).contains(&self.map_x) || !(0..=100).contains(&self.map_y) {
            issues.push(issue(
                "INVALID_MAP_POSITION",
                "mapPosition",
                "Map position must be between 0 and 100.",
            ));
        }
        if !valid_lat(self.approximate_lat) || !valid_lng(self.approximate_lng) {
            issues.push(issue(
                "INVALID_PUBLIC_LOCATION",
                "approximateLocation",
                "Public coordinates are invalid.",
            ));
        }
        match (self.exact_lat, self.exact_lng) {
            (Some(lat), Some(lng)) if valid_lat(lat) && valid_lng(lng) => {}
            (None, None) => issues.push(issue(
                "EXACT_LOCATION_REQUIRED",
                "exactLocation",
                "Exact claim location is required before publish.",
            )),
            _ => issues.push(issue(
                "INVALID_EXACT_LOCATION",
                "exactLocation",
                "Exact claim coordinates are incomplete or invalid.",
            )),
        }
        if !(MIN_AREA_RADIUS_METERS..=MAX_AREA_RADIUS_METERS).contains(&self.radius_meters) {
            issues.push(issue(
                "INVALID_RADIUS",
                "radiusMeters",
                "Radius must be between 25 and 500 metres.",
            ));
        }
        if !(MIN_AREA_CAPACITY..=MAX_AREA_CAPACITY).contains(&self.max_claims) {
            issues.push(issue(
                "INVALID_CAPACITY",
                "maxClaims",
                "Capacity must be between 1 and 500.",
            ));
        } else if i64::from(self.max_claims) < existing_claims {
            issues.push(issue(
                "CAPACITY_BELOW_CLAIMS",
                "maxClaims",
                "Capacity cannot be lower than the number of existing claims.",
            ));
        }
        if self.ends_at <= self.starts_at {
            issues.push(issue(
                "INVALID_WINDOW",
                "endsAt",
                "AREA end time must be after its start time.",
            ));
        }
        if self.clue.en.trim().is_empty() || self.clue.pl.trim().is_empty() {
            issues.push(issue(
                "MISSING_CLUE",
                "clue",
                "Both English and Polish clues are required.",
            ));
        }
        if self.clue.en.chars().count() > MAX_AREA_CLUE_CHARS
            || self.clue.pl.chars().count() > MAX_AREA_CLUE_CHARS
        {
            issues.push(issue(
                "CLUE_TOO_LONG",
                "clue",
                "Each clue must be at most 2000 characters.",
            ));
        }
        if [
            self.collectible.line.as_str(),
            self.collectible.track.as_str(),
            self.collectible.edition.as_str(),
            self.collectible.riddle.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            issues.push(issue(
                "MISSING_COLLECTIBLE",
                "collectible",
                "Collectible line, track, edition and riddle are required.",
            ));
        }
        if self.collectible.line.chars().count() > MAX_AREA_COLLECTIBLE_LINE_CHARS
            || self.collectible.track.chars().count() > MAX_AREA_LABEL_CHARS
            || self.collectible.edition.chars().count() > MAX_AREA_LABEL_CHARS
            || self.collectible.riddle.chars().count() > MAX_AREA_LABEL_CHARS
        {
            issues.push(issue(
                "COLLECTIBLE_TOO_LONG",
                "collectible",
                "Collectible text exceeds the AREA field limits.",
            ));
        }
        issues
    }
}

#[must_use]
pub fn valid_area_drop_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7
        && bytes.iter().take(3).all(u8::is_ascii_lowercase)
        && bytes.get(3) == Some(&b'-')
        && bytes.iter().skip(4).all(u8::is_ascii_digit)
}

#[must_use]
pub fn changed_area_fields(published: &AreaDropDraft, draft: &AreaDropDraft) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if published.number != draft.number {
        changed.push("number");
    }
    if published.city_id != draft.city_id {
        changed.push("cityId");
    }
    if published.map_x != draft.map_x || published.map_y != draft.map_y {
        changed.push("mapPosition");
    }
    if published.approximate_lat != draft.approximate_lat
        || published.approximate_lng != draft.approximate_lng
    {
        changed.push("approximateLocation");
    }
    if published.exact_lat != draft.exact_lat || published.exact_lng != draft.exact_lng {
        changed.push("exactLocation");
    }
    if published.radius_meters != draft.radius_meters {
        changed.push("radiusMeters");
    }
    if published.max_claims != draft.max_claims {
        changed.push("maxClaims");
    }
    if published.starts_at != draft.starts_at {
        changed.push("startsAt");
    }
    if published.ends_at != draft.ends_at {
        changed.push("endsAt");
    }
    if published.clue != draft.clue {
        changed.push("clue");
    }
    if published.collectible != draft.collectible {
        changed.push("collectible");
    }
    if published.sort_order != draft.sort_order {
        changed.push("sortOrder");
    }
    changed
}

#[must_use]
pub fn live_change_confirmation_issues(
    published: &AreaDropDraft,
    draft: &AreaDropDraft,
    is_live: bool,
) -> Vec<AreaValidationIssue> {
    if !is_live {
        return Vec::new();
    }
    let mut issues = Vec::new();
    if published.exact_lat != draft.exact_lat || published.exact_lng != draft.exact_lng {
        issues.push(confirm_issue(
            "MOVE_LIVE_DROP",
            "exactLocation",
            "Publishing moves the exact claim location of a LIVE drop.",
        ));
    }
    if draft.max_claims < published.max_claims {
        issues.push(confirm_issue(
            "REDUCE_LIVE_CAPACITY",
            "maxClaims",
            "Publishing reduces capacity of a LIVE drop.",
        ));
    }
    if draft.ends_at < published.ends_at {
        issues.push(confirm_issue(
            "SHORTEN_LIVE_WINDOW",
            "endsAt",
            "Publishing shortens the active window of a LIVE drop.",
        ));
    }
    issues
}

#[must_use]
pub fn derive_area_status(
    active: bool,
    starts_at: OffsetDateTime,
    ends_at: OffsetDateTime,
    archived_at: Option<OffsetDateTime>,
    has_draft: bool,
    published_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> AreaDropStatus {
    if archived_at.is_some() {
        AreaDropStatus::Archived
    } else if published_at.is_none() && has_draft {
        AreaDropStatus::Draft
    } else if !active {
        AreaDropStatus::Paused
    } else if starts_at > now {
        AreaDropStatus::Scheduled
    } else if ends_at < now {
        AreaDropStatus::Ended
    } else {
        AreaDropStatus::Live
    }
}

const fn confirm_issue(
    code: &'static str,
    field: &'static str,
    message: &'static str,
) -> AreaValidationIssue {
    AreaValidationIssue {
        code,
        field,
        message,
        confirmation_required: true,
    }
}

const fn issue(
    code: &'static str,
    field: &'static str,
    message: &'static str,
) -> AreaValidationIssue {
    AreaValidationIssue {
        code,
        field,
        message,
        confirmation_required: false,
    }
}

fn valid_lat(value: f64) -> bool {
    value.is_finite() && (-90.0..=90.0).contains(&value)
}
fn valid_lng(value: f64) -> bool {
    value.is_finite() && (-180.0..=180.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> AreaDropDraft {
        AreaDropDraft {
            number: "001".to_owned(),
            city_id: Uuid::nil(),
            map_x: 50,
            map_y: 50,
            approximate_lat: 51.1,
            approximate_lng: 17.0,
            exact_lat: Some(51.11),
            exact_lng: Some(17.03),
            radius_meters: 100,
            max_claims: 25,
            starts_at: OffsetDateTime::UNIX_EPOCH,
            ends_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(1),
            clue: AreaLocalizedClue {
                en: "clue".to_owned(),
                pl: "trop".to_owned(),
            },
            collectible: AreaCollectible {
                line: "line".to_owned(),
                track: "track".to_owned(),
                edition: "edition".to_owned(),
                riddle: "riddle".to_owned(),
            },
            sort_order: 0,
        }
    }

    #[test]
    fn valid_draft_has_no_issues() {
        assert!(draft().validate(0).is_empty());
    }

    #[test]
    fn exact_location_is_redacted_from_debug() {
        let debug = format!("{:?}", draft());
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("51.11"));
        assert!(!debug.contains("17.03"));
    }

    #[test]
    fn capacity_cannot_drop_below_claims() {
        let issues = draft().validate(26);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "CAPACITY_BELOW_CLAIMS")
        );
    }

    #[test]
    fn drop_id_validation_matches_schema() {
        assert!(valid_area_drop_id("wro-001"));
        assert!(!valid_area_drop_id("wr-001"));
        assert!(!valid_area_drop_id("WRO-001"));
        assert!(!valid_area_drop_id("wro-0001"));
    }

    #[test]
    fn changed_fields_redact_exact_values() {
        let published = draft();
        let mut changed = published.clone();
        changed.exact_lat = Some(52.0);
        changed.clue.pl = "inny trop".to_owned();
        assert_eq!(
            changed_area_fields(&published, &changed),
            vec!["exactLocation", "clue"]
        );
    }

    #[test]
    fn live_location_move_requires_confirmation() {
        let published = draft();
        let mut changed = published.clone();
        changed.exact_lat = Some(51.2);
        let issues = live_change_confirmation_issues(&published, &changed, true);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, "MOVE_LIVE_DROP");
        assert!(issues[0].confirmation_required);
    }
}
