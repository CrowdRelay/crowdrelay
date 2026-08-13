//! Secret-backed VIRYA team contact configuration.

use super::*;

pub(super) const VIRYA_TEAM_WOJTEK_EMAIL_KEY: &str = "VIRYA_TEAM_WOJTEK_EMAIL";
pub(super) const VIRYA_TEAM_LUBEK_EMAIL_KEY: &str = "VIRYA_TEAM_LUBEK_EMAIL";
pub(super) const VIRYA_TEAM_KUBA_EMAIL_KEY: &str = "VIRYA_TEAM_KUBA_EMAIL";
pub(super) const VIRYA_TEAM_MARCIN_EMAIL_KEY: &str = "VIRYA_TEAM_MARCIN_EMAIL";
pub(super) const VIRYA_TEAM_MAREK_EMAIL_KEY: &str = "VIRYA_TEAM_MAREK_EMAIL";

/// Secret-backed operator contacts for the human handoff router.
///
/// Empty values are valid so a rollout can deploy code before production secrets.
/// Actual addresses never belong in Git, logs or API read models.
#[derive(Clone, PartialEq, Eq)]
pub struct TeamOperationsConfig {
    pub wojtek_email: Option<String>,
    pub lubek_email: Option<String>,
    pub kuba_email: Option<String>,
    pub marcin_email: Option<String>,
    pub marek_email: Option<String>,
}

impl TeamOperationsConfig {
    pub fn configured_members(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("wojtek", self.wojtek_email.as_deref()),
            ("lubek", self.lubek_email.as_deref()),
            ("kuba", self.kuba_email.as_deref()),
            ("marcin", self.marcin_email.as_deref()),
            ("marek", self.marek_email.as_deref()),
        ]
        .into_iter()
        .filter_map(|(key, email)| email.map(|email| (key, email)))
    }

    /// Returns the first missing deploy-secret contact. Production Autopilot
    /// fails closed on this so approvals never look healthy while silently
    /// lacking an owner notification path.
    #[must_use]
    pub fn first_missing_contact_key(&self) -> Option<&'static str> {
        [
            (VIRYA_TEAM_WOJTEK_EMAIL_KEY, self.wojtek_email.as_ref()),
            (VIRYA_TEAM_LUBEK_EMAIL_KEY, self.lubek_email.as_ref()),
            (VIRYA_TEAM_KUBA_EMAIL_KEY, self.kuba_email.as_ref()),
            (VIRYA_TEAM_MARCIN_EMAIL_KEY, self.marcin_email.as_ref()),
            (VIRYA_TEAM_MAREK_EMAIL_KEY, self.marek_email.as_ref()),
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
                "wojtek_email",
                &self.wojtek_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "lubek_email",
                &self.lubek_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "kuba_email",
                &self.kuba_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "marcin_email",
                &self.marcin_email.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "marek_email",
                &self.marek_email.as_ref().map(|_| "[REDACTED]"),
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
        wojtek_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_WOJTEK_EMAIL_KEY),
            VIRYA_TEAM_WOJTEK_EMAIL_KEY,
        )?,
        lubek_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_LUBEK_EMAIL_KEY),
            VIRYA_TEAM_LUBEK_EMAIL_KEY,
        )?,
        kuba_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_KUBA_EMAIL_KEY),
            VIRYA_TEAM_KUBA_EMAIL_KEY,
        )?,
        marcin_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_MARCIN_EMAIL_KEY),
            VIRYA_TEAM_MARCIN_EMAIL_KEY,
        )?,
        marek_email: parse_optional_member_email(
            values.get(VIRYA_TEAM_MAREK_EMAIL_KEY),
            VIRYA_TEAM_MAREK_EMAIL_KEY,
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
            wojtek_email: Some(test_email("member1")),
            lubek_email: Some(test_email("member2")),
            kuba_email: Some(test_email("member3")),
            marcin_email: Some(test_email("member4")),
            marek_email: Some(test_email("member5")),
        }
    }

    #[test]
    fn production_autopilot_fails_closed_without_every_contact() {
        let mut team = configured_team();
        team.wojtek_email = None;
        assert!(matches!(
            validate_production_team_contacts(&team, true, true),
            Err(ConfigError::MissingProductionTeamContact {
                name: VIRYA_TEAM_WOJTEK_EMAIL_KEY
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
        team.marek_email = None;
        assert!(validate_production_team_contacts(&team, true, false).is_ok());
        assert!(validate_production_team_contacts(&team, false, true).is_ok());
    }
}
