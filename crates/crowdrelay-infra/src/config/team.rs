//! Secret-backed team roster configuration.
//!
//! Two sources feed the roster. `CROWDRELAY_TEAM_MEMBERS_JSON` is the elastic
//! form — a JSON array of `{key?, name, email, skills}` entries the control
//! plane's provisioner renders from the onboarding wizard, so a tenant's crew
//! is however many people it actually has. The five `VIRYA_TEAM_MEMBER_N_EMAIL`
//! slots are the legacy form: each carries only an email, and the member's
//! name/skills come from the source-controlled slot profile below. Both may be
//! set; a JSON entry wins when its key collides with a slot's `member_N` key.

use crowdrelay_domain::team_operations::TeamSkill;
use serde::Deserialize;

use super::*;

pub const TEAM_MEMBERS_JSON_KEY: &str = "CROWDRELAY_TEAM_MEMBERS_JSON";

pub(super) const VIRYA_TEAM_MEMBER_1_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_1_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_2_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_2_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_3_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_3_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_4_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_4_EMAIL";
pub(super) const VIRYA_TEAM_MEMBER_5_EMAIL_KEY: &str = "VIRYA_TEAM_MEMBER_5_EMAIL";

/// Bound on roster size, not a crew-size policy — env parsing fails closed on
/// an absurd value rather than handing the bootstrap loop a hostile vec.
const MAX_TEAM_MEMBERS: usize = 32;

/// One resolved team member: stable routing identity, contact, and the
/// name/skills the bootstrap writes into `viryaos_team_profiles`.
#[derive(Clone, PartialEq, Eq)]
pub struct TeamMemberSpec {
    /// Written to `viryaos_team_profiles.member_key`, which requires
    /// `^[a-z0-9_-]{2,48}$` — the config layer checks the same grammar so a
    /// bad key fails at boot, not mid-bootstrap.
    pub member_key: String,
    pub email: String,
    pub display_name: String,
    pub skills: Vec<String>,
}

/// Operator contacts for the human handoff router. The vec is elastic: crew
/// size is tenant configuration, not a source-level assumption.
#[derive(Clone, PartialEq, Eq)]
pub struct TeamOperationsConfig {
    pub members: Vec<TeamMemberSpec>,
}

impl TeamOperationsConfig {
    pub fn configured_members(&self) -> impl Iterator<Item = &TeamMemberSpec> {
        self.members.iter()
    }
}

impl fmt::Debug for TeamOperationsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TeamOperationsConfig")
            .field(
                "members",
                &self.members.iter().map(TeamMemberDebug).collect::<Vec<_>>(),
            )
            .finish()
    }
}

struct TeamMemberDebug<'a>(&'a TeamMemberSpec);

impl fmt::Debug for TeamMemberDebug<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TeamMemberSpec")
            .field("member_key", &self.0.member_key)
            .field("email", &"[REDACTED]")
            .field("display_name", &self.0.display_name)
            .field("skills", &self.0.skills)
            .finish()
    }
}

/// Production Autopilot fails closed when nobody is reachable — handoffs must
/// never look healthy while silently lacking an owner notification path.
/// `None` from either source means "no roster at all", which is the failure.
pub(super) fn validate_production_team_contacts(
    config: &TeamOperationsConfig,
    production: bool,
    autopilot_enabled: bool,
) -> Result<(), ConfigError> {
    if production && autopilot_enabled && config.members.is_empty() {
        return Err(ConfigError::MissingProductionTeamContact {
            name: "CROWDRELAY_TEAM_MEMBERS_JSON or a VIRYA_TEAM_MEMBER_*_EMAIL slot",
        });
    }
    Ok(())
}

pub(super) fn parse_team_operations(
    values: &HashMap<String, String>,
) -> Result<TeamOperationsConfig, ConfigError> {
    let mut members = parse_team_members_json(values.get(TEAM_MEMBERS_JSON_KEY))?;
    for (slot, key) in [
        VIRYA_TEAM_MEMBER_1_EMAIL_KEY,
        VIRYA_TEAM_MEMBER_2_EMAIL_KEY,
        VIRYA_TEAM_MEMBER_3_EMAIL_KEY,
        VIRYA_TEAM_MEMBER_4_EMAIL_KEY,
        VIRYA_TEAM_MEMBER_5_EMAIL_KEY,
    ]
    .iter()
    .enumerate()
    {
        let member_key = format!("member_{}", slot + 1);
        if members.iter().any(|member| member.member_key == member_key) {
            continue;
        }
        if let Some(email) = parse_optional_member_email(values.get(*key), key)? {
            let Some((display_name, skills)) = legacy_slot_profile(&member_key) else {
                continue;
            };
            members.push(TeamMemberSpec {
                member_key,
                email,
                display_name: display_name.to_owned(),
                skills,
            });
        }
    }
    // `workspace_members` is keyed on normalized_email, so two members sharing
    // an email would collapse onto one member row and fight over its profile —
    // the second member_key silently wins the routing identity. Reject the
    // roster rather than boot into that ambiguity.
    for (index, member) in members.iter().enumerate() {
        if let Some(other) = members
            .iter()
            .take(index)
            .find(|other| other.email == member.email)
        {
            return Err(ConfigError::InvalidTeamMembersJson {
                detail: format!(
                    "members '{}' and '{}' share the same email address",
                    other.member_key, member.member_key
                ),
            });
        }
    }
    Ok(TeamOperationsConfig { members })
}

/// A slot's stable profile. Only the five legacy `member_N` keys have one —
/// the mapping exists so a deployment that still speaks the old contract keeps
/// the same routing metadata. Human names for JSON members arrive in the JSON.
fn legacy_slot_profile(member_key: &str) -> Option<(&'static str, Vec<String>)> {
    match member_key {
        "member_1" => Some((
            "Team Member 1",
            [
                "general",
                "operations",
                "booking",
                "approval",
                "technical",
                "people",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )),
        "member_2" => Some((
            "Team Member 2",
            ["visual", "video", "photography", "social"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )),
        "member_3" => Some((
            "Team Member 3",
            ["english_copy", "polish_copy"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )),
        "member_4" => Some((
            "Team Member 4",
            ["operations", "booking", "approval", "people"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )),
        "member_5" => Some((
            "Team Member 5",
            ["operations", "approval", "people", "polish_copy"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )),
        _ => None,
    }
}

#[derive(Deserialize)]
struct TeamMemberJson {
    /// Stable routing identity. Optional so a hand-edited roster stays simple;
    /// when absent the member's 1-based position becomes `member_{n}` —
    /// reordering the array then re-keys that member, so provisioned rosters
    /// always carry an explicit key.
    key: Option<String>,
    name: String,
    email: String,
    skills: Vec<String>,
}

fn parse_team_members_json(value: Option<&String>) -> Result<Vec<TeamMemberSpec>, ConfigError> {
    let Some(raw) = value
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let entries: Vec<TeamMemberJson> =
        serde_json::from_str(raw).map_err(|_| ConfigError::InvalidTeamMembersJson {
            detail: "must be a JSON array of {key?, name, email, skills} objects".to_owned(),
        })?;
    if entries.len() > MAX_TEAM_MEMBERS {
        return Err(ConfigError::InvalidTeamMembersJson {
            detail: format!("roster exceeds the {MAX_TEAM_MEMBERS}-member bound"),
        });
    }
    let mut members = Vec::with_capacity(entries.len());
    for (index, entry) in entries.into_iter().enumerate() {
        let member_key = match entry.key {
            Some(key) => {
                let key = key.trim().to_owned();
                if !member_key_ok(&key) {
                    return Err(ConfigError::InvalidTeamMembersJson {
                        detail: format!("member key '{key}' must match ^[a-z0-9_-]{{2,48}}$"),
                    });
                }
                key
            }
            None => format!("member_{}", index + 1),
        };
        if members
            .iter()
            .any(|member: &TeamMemberSpec| member.member_key == member_key)
        {
            return Err(ConfigError::InvalidTeamMembersJson {
                detail: format!("member key '{member_key}' is duplicated"),
            });
        }
        let display_name = entry.name.trim().to_owned();
        if display_name.is_empty()
            || display_name.chars().count() > 80
            || display_name.chars().any(char::is_control)
        {
            return Err(ConfigError::InvalidTeamMembersJson {
                detail: format!(
                    "member '{member_key}' name must be 1-80 characters with no control characters"
                ),
            });
        }
        if entry.skills.is_empty() {
            return Err(ConfigError::InvalidTeamMembersJson {
                detail: format!("member '{member_key}' must declare at least one skill"),
            });
        }
        let mut skills = Vec::with_capacity(entry.skills.len());
        for skill in &entry.skills {
            let skill = skill.trim().to_ascii_lowercase();
            if TeamSkill::parse(&skill).is_none() {
                return Err(ConfigError::InvalidTeamMembersJson {
                    detail: format!(
                        "member '{member_key}' skill '{skill}' is not a known team skill"
                    ),
                });
            }
            if !skills.contains(&skill) {
                skills.push(skill);
            }
        }
        let email = NormalizedEmail::parse(entry.email.trim())
            .map(NormalizedEmail::into_inner)
            .map_err(|_| ConfigError::InvalidTeamMembersJson {
                detail: format!("member '{member_key}' email is not a valid normalized email"),
            })?;
        members.push(TeamMemberSpec {
            member_key,
            email,
            display_name,
            skills,
        });
    }
    Ok(members)
}

/// Same grammar as the `viryaos_team_profiles.member_key` CHECK constraint.
fn member_key_ok(key: &str) -> bool {
    (2..=48).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
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

    fn slot_member(slot: usize, local: &str) -> TeamMemberSpec {
        let member_key = format!("member_{slot}");
        let (display_name, skills) = legacy_slot_profile(&member_key).unwrap();
        TeamMemberSpec {
            member_key,
            email: test_email(local),
            display_name: display_name.to_owned(),
            skills,
        }
    }

    fn configured_team() -> TeamOperationsConfig {
        TeamOperationsConfig {
            members: (1..=5)
                .map(|slot| slot_member(slot, &format!("member{slot}")))
                .collect(),
        }
    }

    #[test]
    fn production_autopilot_fails_closed_without_any_contact() {
        let team = TeamOperationsConfig {
            members: Vec::new(),
        };
        assert!(matches!(
            validate_production_team_contacts(&team, true, true),
            Err(ConfigError::MissingProductionTeamContact { .. })
        ));
    }

    #[test]
    fn production_autopilot_accepts_a_single_configured_member() {
        let team = TeamOperationsConfig {
            members: vec![slot_member(1, "member1")],
        };
        assert!(validate_production_team_contacts(&team, true, true).is_ok());
        assert_eq!(team.configured_members().count(), 1);
    }

    #[test]
    fn production_autopilot_accepts_secret_backed_contacts() {
        let team = configured_team();
        assert!(validate_production_team_contacts(&team, true, true).is_ok());
        assert_eq!(team.configured_members().count(), 5);
    }

    #[test]
    fn disabled_or_non_production_autopilot_does_not_require_contacts() {
        let team = TeamOperationsConfig {
            members: Vec::new(),
        };
        assert!(validate_production_team_contacts(&team, true, false).is_ok());
        assert!(validate_production_team_contacts(&team, false, true).is_ok());
    }

    #[test]
    fn json_roster_parses_elastic_members() {
        let mut values = HashMap::new();
        values.insert(
            TEAM_MEMBERS_JSON_KEY.to_owned(),
            r#"[
              {"key":"ops_lead","name":"Ada Ops","email":"ada@example.test","skills":["operations","booking"]},
              {"name":"Ben Social","email":"ben@example.test","skills":["social","visual"]}
            ]"#
            .to_owned(),
        );
        let team = parse_team_operations(&values).unwrap();
        assert_eq!(team.members.len(), 2);
        assert_eq!(team.members[0].member_key, "ops_lead");
        assert_eq!(team.members[0].display_name, "Ada Ops");
        assert_eq!(team.members[0].skills, ["operations", "booking"]);
        // Positional key default keeps a hand-edited roster simple.
        assert_eq!(team.members[1].member_key, "member_2");
        assert_eq!(team.members[1].skills, ["social", "visual"]);
    }

    #[test]
    fn json_roster_and_legacy_slots_merge_without_duplicating_keys() {
        let mut values = HashMap::new();
        values.insert(
            TEAM_MEMBERS_JSON_KEY.to_owned(),
            r#"[{"key":"member_1","name":"Ada Ops","email":"ada@example.test","skills":["booking"]}]"#
                .to_owned(),
        );
        values.insert(
            VIRYA_TEAM_MEMBER_1_EMAIL_KEY.to_owned(),
            test_email("legacy-one"),
        );
        values.insert(
            VIRYA_TEAM_MEMBER_3_EMAIL_KEY.to_owned(),
            test_email("legacy-three"),
        );
        let team = parse_team_operations(&values).unwrap();
        assert_eq!(team.members.len(), 2);
        // The JSON member_1 wins; the slot's email does not override it.
        assert_eq!(team.members[0].email, "ada@example.test");
        assert_eq!(team.members[1].member_key, "member_3");
        assert_eq!(team.members[1].email, test_email("legacy-three"));
        assert_eq!(team.members[1].skills, ["english_copy", "polish_copy"]);
    }

    #[test]
    fn json_roster_and_legacy_slot_cannot_share_an_email() {
        let mut values = HashMap::new();
        values.insert(
            TEAM_MEMBERS_JSON_KEY.to_owned(),
            r#"[{"key":"ops_lead","name":"Ada","email":"shared@example.test","skills":["general"]}]"#
                .to_owned(),
        );
        values.insert(
            VIRYA_TEAM_MEMBER_2_EMAIL_KEY.to_owned(),
            "Shared@Example.Test".to_owned(),
        );
        assert!(matches!(
            parse_team_operations(&values),
            Err(ConfigError::InvalidTeamMembersJson { detail: d })
                if d.contains("share the same email")
        ));
    }

    #[test]
    fn json_roster_rejects_bad_shapes() {
        for (raw, detail) in [
            ("not json", "JSON array"),
            (
                r#"[{"key":"Bad Key!","name":"a","email":"a@b.c","skills":["general"]}]"#,
                "member key",
            ),
            (
                r#"[{"name":"a","email":"a@b.c","skills":[]}]"#,
                "at least one skill",
            ),
            (
                r#"[{"name":"a","email":"a@b.c","skills":["nonsense"]}]"#,
                "not a known team skill",
            ),
            (
                r#"[{"name":"a","email":"not-an-email","skills":["general"]}]"#,
                "normalized email",
            ),
            (
                r#"[{"name":"","email":"a@b.c","skills":["general"]}]"#,
                "1-80 characters",
            ),
            (
                r#"[{"key":"same","name":"a","email":"a@b.c","skills":["general"]},{"key":"same","name":"b","email":"b@b.c","skills":["general"]}]"#,
                "duplicated",
            ),
            (
                r#"[{"key":"one","name":"a","email":"same@b.c","skills":["general"]},{"key":"two","name":"b","email":"SAME@b.c","skills":["general"]}]"#,
                "share the same email",
            ),
        ] {
            let mut values = HashMap::new();
            values.insert(TEAM_MEMBERS_JSON_KEY.to_owned(), raw.to_owned());
            assert!(
                matches!(
                    parse_team_operations(&values),
                    Err(ConfigError::InvalidTeamMembersJson { detail: d }) if d.contains(detail)
                ),
                "expected InvalidTeamMembersJson mentioning '{detail}' for {raw}"
            );
        }
    }
}
