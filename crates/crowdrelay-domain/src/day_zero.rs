//! Can this tenant start acquiring fans on its own — and if not, what is the
//! one smallest thing standing in the way?
//!
//! The FAN_100 Day-0 contract says the product must name the smallest missing
//! prerequisite instead of hiding it behind a healthy-looking empty dashboard.
//! Until this existed no single place said so. Production on 2026-10-03 had
//! connected Facebook and Instagram pages whose credentials work, fresh content
//! to promote, a signup destination and the tenant's own join-ask words — and
//! `social_auto_post` off with the platform list set to `telegram`. The product
//! was one owner decision away from an autonomous rail and said nothing about
//! it; the brain meanwhile ranked actions for lanes that could only draft.
//!
//! This is a pure function of facts. It decides nothing about publishing,
//! grants nothing and never reads a credential: a connection is not consent.
//! It distinguishes tenant-owner work (for example standing authority) from
//! deployment-operator work (for example a worker kill switch), so the UI
//! cannot offer an owner button for something only a deployment can repair.

use serde::Serialize;

/// The owned-social rails that can carry an action-owned clickable link, in the
/// order they are preferred when equally far from ready. Facebook leads because
/// an Instagram feed caption cannot carry a dependable clickable link (see
/// `platform_yield::preferred_owned_social_platform`).
pub const OWNED_RAILS: [&str; 2] = ["facebook", "instagram"];

/// What was observed about one platform connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionFact {
    pub platform: String,
    /// A connection row exists and is not revoked.
    pub connected: bool,
    /// Its last sync succeeded (`health = working`). A connection that has
    /// never been read, or whose latest read failed, is not known to work.
    pub working: bool,
}

/// Everything the decision reads.
#[derive(Clone, Debug)]
pub struct ReadinessFacts<'a> {
    /// The tenant's own public site root, where signup lives.
    pub site_root: Option<&'a str>,
    /// The tenant has written join-ask words of its own.
    pub join_copy: bool,
    /// A video, release or event inside the freshness window to promote.
    pub fresh_asset: bool,
    /// Whether the running worker reports the deployment-level social
    /// publisher gate as enabled. None means no worker has reported it yet.
    pub social_publish_runtime: Option<bool>,
    /// The tenant's publish-without-asking switch.
    pub social_auto_post: bool,
    /// The platforms that switch is allowed to publish to.
    pub autopost_platforms: &'a [String],
    pub connections: &'a [ConnectionFact],
}

/// How far one rail is from publishing by itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RailState {
    /// Connected, working, and the tenant granted standing authority.
    Executable,
    /// Everything works at deployment level; the owner has not granted standing authority.
    NeedsAuthority,
    /// The worker has not reported whether the deployment publisher exists.
    RuntimeUnknown,
    /// The worker explicitly reports the deployment-level social publisher off.
    DeploymentGateOff,
    /// Connected, but its last read failed or never happened.
    CredentialNotWorking,
    NotConnected,
}

/// One owned-social rail and the first thing it lacks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Rail {
    pub platform: &'static str,
    pub state: RailState,
    /// Prerequisites still missing on this rail, in the order to fix them.
    pub missing_steps: u8,
}

/// The single thing to fix next.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Missing {
    pub code: &'static str,
    pub what: String,
    /// Whether the tenant owner can resolve this from the product. False means
    /// deployment/operator work; the system never grants itself authority or
    /// pretends an environment switch is a tenant preference.
    pub owner_action: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Readiness {
    /// At least one owned rail is executable and nothing earlier is missing.
    pub ready: bool,
    pub rails: Vec<Rail>,
    pub smallest_missing: Option<Missing>,
}

fn rail(facts: &ReadinessFacts<'_>, platform: &'static str) -> Rail {
    let connection = facts
        .connections
        .iter()
        .find(|c| c.platform.eq_ignore_ascii_case(platform) && c.connected);
    let authority = facts.social_auto_post
        && facts
            .autopost_platforms
            .iter()
            .any(|p| p.eq_ignore_ascii_case(platform));
    let (state, missing_steps) = match connection {
        None => (RailState::NotConnected, 4),
        Some(c) if !c.working => (RailState::CredentialNotWorking, 3),
        Some(_) if facts.social_publish_runtime.is_none() => (RailState::RuntimeUnknown, 2),
        Some(_) if facts.social_publish_runtime == Some(false) => (RailState::DeploymentGateOff, 2),
        Some(_) if !authority => (RailState::NeedsAuthority, 1),
        Some(_) => (RailState::Executable, 0),
    };
    Rail {
        platform,
        state,
        missing_steps,
    }
}

/// Decides readiness.
///
/// Order matters and is deliberate: a missing destination or nothing to promote
/// is fixed before any rail, because no rail can compensate for them; among
/// rails the one fewest steps from executable is named, ties going to the
/// clickable one.
#[must_use]
pub fn assess(facts: &ReadinessFacts<'_>) -> Readiness {
    let rails: Vec<Rail> = OWNED_RAILS.iter().map(|p| rail(facts, p)).collect();
    let missing = if facts.site_root.is_none_or(|root| root.trim().is_empty()) {
        Some(Missing {
            code: "no_signup_destination",
            what: "No public site root is set, so a tracked link has nowhere first-party to send anyone."
                .to_owned(),
            owner_action: true,
        })
    } else if !facts.join_copy {
        Some(Missing {
            code: "no_join_ask_copy",
            what: "No join-ask words of the tenant's own are set; the product will not invent a voice."
                .to_owned(),
            owner_action: true,
        })
    } else if !facts.fresh_asset {
        Some(Missing {
            code: "nothing_fresh_to_promote",
            what: "No video, release or show inside the freshness window; there is nothing true to say."
                .to_owned(),
            owner_action: true,
        })
    } else if rails.iter().any(|r| r.state == RailState::Executable) {
        None
    } else {
        // The closest rail, preferring the earlier (clickable) one on a tie.
        rails
            .iter()
            .min_by_key(|r| r.missing_steps)
            .map(|closest| match closest.state {
                RailState::NeedsAuthority => Missing {
                    code: "standing_authority_not_granted",
                    what: format!(
                        "{p} is connected and working, but publishing there without asking is not granted: set social_auto_post on and add {p} to social_autopost_platforms.",
                        p = closest.platform
                    ),
                    owner_action: true,
                },
                RailState::RuntimeUnknown => Missing {
                    code: "deployment_publish_state_unknown",
                    what: "The worker has not reported the social publisher runtime state yet; tenant authority cannot make an unreported executor real.".to_owned(),
                    owner_action: false,
                },
                RailState::DeploymentGateOff => Missing {
                    code: "deployment_publish_gate_off",
                    what: "The running worker reports CROWDRELAY_SOCIAL_AUTO_POST off; enable that deployment gate and restart the worker before granting tenant autopost authority.".to_owned(),
                    owner_action: false,
                },
                RailState::CredentialNotWorking => Missing {
                    code: "rail_credential_not_working",
                    what: format!(
                        "{} is connected but its last read did not succeed; reconnect or fix the credential.",
                        closest.platform
                    ),
                    owner_action: true,
                },
                RailState::NotConnected | RailState::Executable => Missing {
                    code: "no_owned_rail_connected",
                    what: "Neither Facebook nor Instagram is connected; there is no owned net-new rail to publish on."
                        .to_owned(),
                    owner_action: true,
                },
            })
    };
    Readiness {
        ready: missing.is_none(),
        rails,
        smallest_missing: missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(platform: &str, working: bool) -> ConnectionFact {
        ConnectionFact {
            platform: platform.to_owned(),
            connected: true,
            working,
        }
    }

    fn facts<'a>(
        connections: &'a [ConnectionFact],
        platforms: &'a [String],
        auto_post: bool,
    ) -> ReadinessFacts<'a> {
        ReadinessFacts {
            site_root: Some("https://band.example"),
            join_copy: true,
            fresh_asset: true,
            social_publish_runtime: Some(true),
            social_auto_post: auto_post,
            autopost_platforms: platforms,
            connections,
        }
    }

    #[test]
    fn production_shape_is_one_owner_decision_away_and_says_so() {
        // 2026-10-03: pages connected and syncing, auto-post off, list = telegram.
        let connections = [conn("facebook", true), conn("instagram", true)];
        let platforms = vec!["telegram".to_owned()];
        let readiness = assess(&facts(&connections, &platforms, false));
        assert!(!readiness.ready);
        let missing = readiness.smallest_missing.expect("names the blocker");
        assert_eq!(missing.code, "standing_authority_not_granted");
        assert!(missing.owner_action);
        assert!(
            missing.what.starts_with("facebook"),
            "a tie goes to the rail that can carry a clickable link: {}",
            missing.what
        );
    }

    #[test]
    fn a_granted_working_rail_is_ready_with_nothing_missing() {
        let connections = [conn("facebook", true)];
        let platforms = vec!["facebook".to_owned()];
        let readiness = assess(&facts(&connections, &platforms, true));
        assert!(readiness.ready);
        assert_eq!(readiness.smallest_missing, None);
    }

    #[test]
    fn the_global_switch_without_the_platform_is_still_no_authority() {
        let connections = [conn("facebook", true)];
        let platforms = vec!["telegram".to_owned()];
        let readiness = assess(&facts(&connections, &platforms, true));
        assert_eq!(
            readiness.smallest_missing.map(|m| m.code),
            Some("standing_authority_not_granted")
        );
    }

    #[test]
    fn authority_without_a_working_connection_is_not_ready() {
        let connections = [conn("facebook", false)];
        let platforms = vec!["facebook".to_owned()];
        let readiness = assess(&facts(&connections, &platforms, true));
        assert_eq!(
            readiness.smallest_missing.map(|m| m.code),
            Some("rail_credential_not_working")
        );
        let none = assess(&facts(&[], &platforms, true));
        assert_eq!(
            none.smallest_missing.map(|m| m.code),
            Some("no_owned_rail_connected")
        );
    }

    #[test]
    fn the_rail_closest_to_executable_is_the_one_named() {
        // Instagram works and only needs authority; Facebook is not connected.
        let connections = [conn("instagram", true)];
        let platforms: Vec<String> = Vec::new();
        let readiness = assess(&facts(&connections, &platforms, false));
        let missing = readiness.smallest_missing.expect("blocker");
        assert_eq!(missing.code, "standing_authority_not_granted");
        assert!(missing.what.starts_with("instagram"), "{}", missing.what);
    }

    #[test]
    fn tenant_authority_cannot_make_a_disabled_worker_ready() {
        let connections = [conn("facebook", true)];
        let platforms = vec!["facebook".to_owned()];
        let mut f = facts(&connections, &platforms, true);
        f.social_publish_runtime = Some(false);
        let readiness = assess(&f);
        assert!(!readiness.ready);
        assert_eq!(
            readiness.smallest_missing.map(|m| (m.code, m.owner_action)),
            Some(("deployment_publish_gate_off", false))
        );
    }

    #[test]
    fn an_unreported_worker_fails_closed_instead_of_guessing_ready() {
        let connections = [conn("facebook", true)];
        let platforms = vec!["facebook".to_owned()];
        let mut f = facts(&connections, &platforms, true);
        f.social_publish_runtime = None;
        let readiness = assess(&f);
        assert_eq!(
            readiness.smallest_missing.map(|m| m.code),
            Some("deployment_publish_state_unknown")
        );
    }

    #[test]
    fn earlier_prerequisites_are_named_before_any_rail() {
        let connections = [conn("facebook", true)];
        let platforms = vec!["facebook".to_owned()];
        let mut f = facts(&connections, &platforms, true);
        f.fresh_asset = false;
        assert_eq!(
            assess(&f).smallest_missing.map(|m| m.code),
            Some("nothing_fresh_to_promote")
        );
        f.join_copy = false;
        assert_eq!(
            assess(&f).smallest_missing.map(|m| m.code),
            Some("no_join_ask_copy")
        );
        f.site_root = Some("  ");
        assert_eq!(
            assess(&f).smallest_missing.map(|m| m.code),
            Some("no_signup_destination")
        );
    }
}
