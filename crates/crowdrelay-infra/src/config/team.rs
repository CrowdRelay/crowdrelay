//! Secret-backed VIRYA team contact configuration.

use super::*;

pub(super) const VIRYA_TEAM_MEMBER_1_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_1_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_2_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_2_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_3_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_3_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_4_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_4_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_5_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_5_EMAIL";

/// Secret-backed operator contacts for the human handoff router.
///
/// Slot identifiers are deliberately generic. Human identity belongs to
/// runtime member data, not source-level environment contracts.
#[derive(Clone, PartialEq, Eq)]
pub struct TeamOperationsConfig {
    pub member_1_email: Option<String>,
    pub member_2_email: Option<String>,
    pub member_3_email: Option<String>,
    pub member_4_email: Option<String>,
    pub member_5_email: Option<String>,
}

impl TeamOperationsConfig {
    pub fn configured_members(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("member_1", self.member_1_email.as_deref()),
            ("member_2", self.member_2_email.as_deref()),
            ("member_3", self.member_3_email.as_deref()),
            ("member_4", self.member_4_email.as_deref()),
            ("member_5", self.member_5_email.as_deref()),
        ]
        .into_iter()
        .filter_map(|(key, email)| email.map(|email| (key, email)))
    }

    /// Returns the first missing deploy-secret contact. Production Autopilot
    /// fails closed on this so handoffs never look healthy while silently
    /// lacking an owner notification path.
    #[must_use]
    pub fn first_missing_contact_key(&self) -> Option<&'static str> {
        [
            (VIRYA_TEAM_MEMBER_1_EMAIL_KEY, self.member_1_email.as_ref()),
            (VIRYA_TEAM_MEMBER_2_EMAIL_KEY, self.member_2_email.as_ref()),
            (VIRYA_TEAM_MEMBER_3_EMAIL_KEY, self.member_3_email.as_ref()),
            (VIRYA_TEAM_MEMBER_4_EMAIL_KEY, self.member_4_email.as_ref()),
            (VIRYA_TEAM_MEMBER_5_EMAIL_KEY, self.member_5_email.as_ref()),
        ]
        .into_iter()
        .find_map(|(name, value)| value.is_none().then_some(name))
    }
}

impl fmt::Debug for TeamOperationsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TeamOperationsConfig")
            .field(
                "member_1_email",
                &self.member_1_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "member_2_email",
                &self.member_2_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "member_3_email",
                &self.member_3_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "member_4_email",
                &self.member_4_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "member_5_email",
                &self.member_5_email.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

pub(super) fn validate_production_team_contacts(
    config: &TeamOperationsConfig,
    production: bool,
    autopilot_enabled: bool,
) -> Result<(), ConfigError> {
    if production
        && autopilot_enabled
        && let Some(name) = config.first_missing_contact_key()
    {
        return Err(ConfigError::MissingProductionTeamContact { name });
    }
    Ok(())
}

pub(super) fn parse_team_operations(
    values: &HashMap<String, String>,
) -> Result<TeamOperationsConfig, ConfigError> {
    Ok(TeamOperationsConfig {
        member_1_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_MEMBER_1_EMAIL_KEY),
            VIRYA_TEAM_MEMBER_1_EMAIL_KEY,
        )?,
        member_2_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_MEMBER_2_EMAIL_KEY),
            VIRYA_TEAM_MEMBER_2_EMAIL_KEY,
        )?,
        member_3_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_MEMBER_3_EMAIL_KEY),
            VIRYA_TEAM_MEMBER_3_EMAIL_KEY,
        )?,
        member_4_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_MEMBER_4_EMAIL_KEY),
            VIRYA_TEAM_MEMBER_4_EMAIL_KEY,
        )?,
        member_5_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_MEMBER_5_EMAIL_KEY),
            VIRYA_TEAM_MEMBER_5_EMAIL_KEY,
        )?,
    })
}

fn parse_optional_member_email(
    value: Option<&String>,
    name: &'static str,
) -> Result<Option<String>, ConfigError> {
    let Some(value) = value
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    NormalizedEmail::parse(value)
        .map(NormalizedEmail::into_inner)
        .map(Some)
        .map_err(|_| ConfigError::InvalidMemberEmail { name })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_email(local: &str) -> String {
        format!("{local}@example.test")
    }

    fn configured_team() -> TeamOperationsConfig {
        TeamOperationsConfig {
            member_1_email: Some(test_email("member1")),
            member_2_email: Some(test_email("member2")),
            member_3_email: Some(test_email("member3")),
            member_4_email: Some(test_email("member4")),
            member_5_email: Some(test_email("member5")),
        }
    }

    #[test]
    fn production_autopilot_fails_closed_without_every_contact() {
        let mut team = configured_team();
        team.member_1_email = None;
        assert!(matches!(
            validate_production_team_contacts(&team, true, true),
            Err(ConfigError::MissingProductionTeamContact {
                name: VIRYA_TEAM_MEMBER_1_EMAIL_KEY
            })
        ));
    }

    #[test]
    fn production_autopilot_accepts_secret_backed_contacts() {
        let team = configured_team();
        assert!(validate_production_team_contacts(&team, true, true).is_ok());
        assert_eq!(team.configured_members().count(), 5);
    }

    #[test]
    fn disabled_or_non_production_autopilot_does_not_require_contacts() {
        let mut team = configured_team();
        team.member_5_email = None;
        assert!(validate_production_team_contacts(&team, true, false).is_ok());
        assert!(validate_production_team_contacts(&team, false, true).is_ok());
    }
}
