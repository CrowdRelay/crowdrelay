//! Standing — adaptive cadence for worker templates.
//!
//! Effective workers get shorter cooldowns, ineffective ones get longer,
//! retired ones never dispatch. Uses the same `Standing` / `StandingPolicy`
//! types as the play learning system (see `crowdrelay_domain::learning`).

use crowdrelay_domain::learning::{Standing, StandingPolicy};
use serde::{Deserialize, Serialize};

/// Intelligent token optimization tier.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTier {
    #[default]
    Basic,
    Premium,
}

impl AgentTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Premium => "premium",
        }
    }
}

/// Computes the effective cooldown for a worker template given its standing.
#[must_use]
pub fn effective_agent_cooldown(base_cooldown_hours: u32, standing: Standing) -> u32 {
    match standing {
        Standing::Untested { .. } => base_cooldown_hours,
        Standing::Weighted { basis_points, .. } => {
            if basis_points == 0 {
                return base_cooldown_hours.saturating_mul(4);
            }
            let factor = 10_000_u32 / basis_points.max(1) as u32;
            base_cooldown_hours
                .saturating_mul(factor)
                .min(base_cooldown_hours.saturating_mul(4))
        }
        Standing::Retired { .. } => u32::MAX,
    }
}

/// Computes the effective tier for a worker dispatch given its standing.
#[must_use]
pub const fn effective_agent_tier(base_tier: AgentTier, standing: Standing) -> AgentTier {
    match standing {
        Standing::Weighted { basis_points, .. } if basis_points >= 8_000 => AgentTier::Premium,
        _ => base_tier,
    }
}

/// Returns Premium unconditionally for human-contact templates (press-pitch,
/// community-engager). Standing still controls cooldown and retirement, but a
/// human-contact dispatch never downgrades to Basic — a bad pitch to a real
/// contact burns a relationship permanently, so model quality is always
/// prioritized over cost.
#[must_use]
pub const fn human_contact_tier() -> AgentTier {
    AgentTier::Premium
}

/// The default standing policy for agent dispatches.
#[must_use]
pub const fn agent_standing_policy() -> StandingPolicy {
    StandingPolicy::agent_defaults()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::learning::{OutcomeRecord, RetirementReason, assess_standing};

    fn policy() -> StandingPolicy {
        StandingPolicy::agent_defaults()
    }

    #[test]
    fn untested_worker_runs_at_base_cadence() {
        let record = OutcomeRecord::default();
        let standing = assess_standing(record, policy());
        assert!(matches!(standing, Standing::Untested { measured: 0 }));
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn one_measurement_does_not_adjust_cadence() {
        let record = OutcomeRecord {
            improved: 1,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(standing, Standing::Untested { measured: 1 }));
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn effective_worker_gets_shorter_cooldown() {
        let record = OutcomeRecord {
            improved: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(standing, Standing::Weighted { .. }));
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn ineffective_worker_gets_longer_cooldown() {
        let record = OutcomeRecord {
            neutral: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        if let Standing::Weighted { basis_points, .. } = standing {
            assert_eq!(basis_points, 5_000);
        } else {
            panic!("expected Weighted standing");
        }
        assert_eq!(effective_agent_cooldown(168, standing), 336);
    }

    #[test]
    fn cooldown_adjustment_is_capped_at_4x() {
        let record = OutcomeRecord {
            worsened: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        if let Standing::Weighted { basis_points, .. } = standing {
            assert_eq!(basis_points, 2_000);
        } else {
            panic!("expected Weighted standing");
        }
        assert_eq!(effective_agent_cooldown(168, standing), 168 * 4);
    }

    #[test]
    fn retired_worker_never_dispatches() {
        let record = OutcomeRecord {
            worsened: 3,
            consecutive_worsened: 3,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(
            standing,
            Standing::Retired {
                reason: RetirementReason::RepeatedlyWorsened
            }
        ));
        assert_eq!(effective_agent_cooldown(168, standing), u32::MAX);
    }

    #[test]
    fn operator_retired_worker_is_retired() {
        let record = OutcomeRecord {
            operator_retired: true,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(matches!(
            standing,
            Standing::Retired {
                reason: RetirementReason::OperatorRetired
            }
        ));
    }

    #[test]
    fn one_worsened_does_not_retire() {
        let record = OutcomeRecord {
            worsened: 1,
            consecutive_worsened: 1,
            improved: 1,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert!(!standing.is_retired());
    }

    #[test]
    fn effective_worker_escalates_to_premium() {
        let record = OutcomeRecord {
            improved: 3,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert_eq!(
            effective_agent_tier(AgentTier::Basic, standing),
            AgentTier::Premium
        );
    }

    #[test]
    fn mediocre_worker_stays_at_base_tier() {
        let record = OutcomeRecord {
            neutral: 2,
            ..OutcomeRecord::default()
        };
        let standing = assess_standing(record, policy());
        assert_eq!(
            effective_agent_tier(AgentTier::Basic, standing),
            AgentTier::Basic
        );
    }

    #[test]
    fn human_contact_tier_is_always_premium() {
        // Even with the worst standing, human-contact tier is Premium.
        let retired = Standing::Retired {
            reason: RetirementReason::RepeatedlyWorsened,
        };
        assert_eq!(human_contact_tier(), AgentTier::Premium);
        // effective_agent_tier with Premium base also stays Premium
        // regardless of standing.
        assert_eq!(
            effective_agent_tier(AgentTier::Premium, retired),
            AgentTier::Premium
        );
        let weighted_low = Standing::Weighted {
            basis_points: 1_000,
            measured: 5,
        };
        assert_eq!(
            effective_agent_tier(AgentTier::Premium, weighted_low),
            AgentTier::Premium
        );
    }
}
