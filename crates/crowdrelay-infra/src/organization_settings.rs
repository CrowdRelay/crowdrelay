//! Settings that belong to an organisation rather than to one of its acts
//! (4G.2b).
//!
//! `tenant_settings` holds what a band wants. This holds what a label or a
//! management company wants, and the first of those is how many packages the
//! roster can actually run in a period.
//!
//! # Absent stays absent
//!
//! Every read returns `Option`. A roster that has never stated its capacity is
//! not a roster with a capacity of zero, and it is not a roster with whatever
//! number this file would otherwise have invented. The planner refuses to plan
//! until somebody says, because a plan built on an invented capacity is a plan
//! nobody agreed to staff — and the manager finds that out when the third
//! package needs people who are already busy.
//!
//! Not cached, unlike `tenant_settings`: these are read once per plan, which is
//! an operator-speed request, and a manager who lowers the cap expects the next
//! plan to respect it rather than the one after a TTL.

use sqlx::PgPool;
use uuid::Uuid;

/// How many packages the roster can run in one period.
///
/// A package is a booked night with its acts, not a message. Twelve is the
/// ceiling because the period is a month in every case anybody has described,
/// and a roster running more than one package every two days is not planning,
/// it is dispatching.
///
/// Pausing a roster is not this number's job: an act that is not playing says
/// so through its own `tenant_intent`, and the planner already refuses to
/// propose it. A cap of zero would refuse every act for a reason none of them
/// stated.
pub const KEY_ROSTER_PACKAGES_PER_PERIOD: &str = "roster_packages_per_period";

/// The inclusive bounds a stored capacity must fall inside.
pub const PACKAGES_PER_PERIOD_RANGE: std::ops::RangeInclusive<u16> = 1..=12;

/// The keys an operator may edit on an organisation.
///
/// Same allowlist discipline as `tenant_settings::EDITABLE_KEYS`: anything else
/// stays internal even if a row somehow appears, so the HTTP surface cannot be
/// used to smuggle state into the organisation.
pub const EDITABLE_KEYS: [&str; 1] = [KEY_ROSTER_PACKAGES_PER_PERIOD];

#[derive(Clone)]
pub struct OrganizationSettingsRepository {
    pool: PgPool,
}

impl OrganizationSettingsRepository {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One stored value, or `None` when the organisation has never set it.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn get(
        &self,
        organization_id: Uuid,
        key: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT value FROM organization_settings
             WHERE organization_id = $1 AND key = $2",
        )
        .bind(organization_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    /// Every override this organisation has set, for a panel that shows both
    /// the value and whether it was ever chosen.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn list(&self, organization_id: Uuid) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT key, value FROM organization_settings
             WHERE organization_id = $1
             ORDER BY key",
        )
        .bind(organization_id)
        .fetch_all(&self.pool)
        .await
    }

    /// How many packages this roster can run this period, as stated.
    ///
    /// `None` means nobody has said. A value outside the bounds resolves to
    /// `None` as well rather than clamping: writes are validated at the edge,
    /// so an out-of-range row is a hand edit, and clamping it would produce a
    /// plan sized by a number nobody chose. The planner then says what is
    /// missing, which is recoverable in one click.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn packages_this_period(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<u16>, sqlx::Error> {
        Ok(self
            .get(organization_id, KEY_ROSTER_PACKAGES_PER_PERIOD)
            .await?
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|packages| PACKAGES_PER_PERIOD_RANGE.contains(packages)))
    }

    /// Upserts one override.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn set(
        &self,
        organization_id: Uuid,
        key: &str,
        value: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO organization_settings (organization_id, key, value)
            VALUES ($1, $2, $3)
            ON CONFLICT (organization_id, key) DO UPDATE SET
                value = EXCLUDED.value, updated_at = now()
            "#,
        )
        .bind(organization_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Whether a submitted value is one this vocabulary accepts.
///
/// Lives here rather than at the edge so the bounds and the key that carries
/// them cannot drift apart: the edge validates, this file decides what valid
/// means, and the reader above discards anything that fails it.
#[must_use]
pub fn is_valid_value(key: &str, value: &str) -> bool {
    match key {
        KEY_ROSTER_PACKAGES_PER_PERIOD => value
            .trim()
            .parse::<u16>()
            .is_ok_and(|packages| PACKAGES_PER_PERIOD_RANGE.contains(&packages)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capacity_bounds_are_asserted_on_both_sides() {
        assert!(!is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, "0"));
        assert!(is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, "1"));
        assert!(is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, "12"));
        assert!(!is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, "13"));
    }

    #[test]
    fn a_value_that_is_not_a_number_is_refused_rather_than_coerced() {
        assert!(!is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, ""));
        assert!(!is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, "two"));
        assert!(!is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, "-1"));
        assert!(!is_valid_value(KEY_ROSTER_PACKAGES_PER_PERIOD, "3.5"));
    }

    /// The allowlist is the whole of the write surface. A key nobody has
    /// decided about must not be settable because the table would accept it.
    #[test]
    fn an_unknown_key_is_never_valid() {
        assert!(!is_valid_value("anything_else", "1"));
        assert_eq!(EDITABLE_KEYS.len(), 1);
        assert!(EDITABLE_KEYS.contains(&KEY_ROSTER_PACKAGES_PER_PERIOD));
    }
}
