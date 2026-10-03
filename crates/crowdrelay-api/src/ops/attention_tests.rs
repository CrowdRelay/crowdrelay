#[cfg(test)]
mod calibration_readout_tests {
    use super::calibration_readout_from;
    use crowdrelay_application::CalibrationByRegime;

    /// No `calibration` key — a checkpoint serialized before the regime
    /// split — reads as "not reported", not zeroed numbers.
    #[test]
    fn absent_calibration_key_is_none() {
        assert!(calibration_readout_from(&serde_json::json!({})).is_none());
        assert!(
            calibration_readout_from(&serde_json::json!({"calibration": "junk"})).is_none()
        );
    }

    /// A tracked-but-unobserved regime reports `null`, so the operator sees
    /// "no predictions scored yet", never a fabricated zero bias.
    #[test]
    fn unobserved_regimes_serialize_null_not_zero() {
        let regimes = CalibrationByRegime::new();
        let state = serde_json::json!({"calibration": regimes});
        let readout = calibration_readout_from(&state).expect("readout");
        let json = serde_json::to_value(&readout).unwrap();
        assert!(json["y30_direct"].is_null());
        assert!(json["y14_bridged"].is_null());
        assert!(json["outcome_model"].is_null());
    }

    /// A populated regime reports the real numbers; empty siblings stay null.
    #[test]
    fn populated_regime_reports_counts_and_bias() {
        let mut regimes = CalibrationByRegime::new();
        regimes.outcome_model.record("t", 10.0, 1.0, 4.0);
        regimes.outcome_model.record("t", 8.0, 1.0, 6.0);
        let state = serde_json::json!({"calibration": regimes});
        let readout = calibration_readout_from(&state).expect("readout");
        let json = serde_json::to_value(&readout).unwrap();
        assert_eq!(json["outcome_model"]["predictions"], 2);
        // (10-4 + 8-6) / 2 = 4.0 — systematic over-prediction.
        assert_eq!(json["outcome_model"]["bias"], 4.0);
        assert!(json["y30_direct"].is_null());
    }
}

#[cfg(test)]
mod growth_readiness_tests {
    use super::{GrowthBlocker, growth_readiness_from};
    use crowdrelay_domain::day_zero::{
        ConnectionFact, PublishPermission, ReadinessFacts, assess,
    };

    fn facts_with<'a>(
        connections: &'a [ConnectionFact],
        platforms: &'a [String],
        auto_post: bool,
    ) -> ReadinessFacts<'a> {
        ReadinessFacts {
            site_root: Some("https://band.example"),
            confirmation_delivery_route: true,
            join_copy: true,
            fresh_asset: true,
            social_publish_runtime: Some(true),
            social_auto_post: auto_post,
            autopost_platforms: platforms,
            connections,
        }
    }

    fn facebook(publish: PublishPermission) -> ConnectionFact {
        ConnectionFact {
            platform: "facebook".to_owned(),
            connected: true,
            working: true,
            publish,
        }
    }

    #[test]
    fn the_board_carries_the_one_blocker_with_who_can_fix_it() {
        let connections = [facebook(PublishPermission::Verified)];
        let platforms = vec!["telegram".to_owned()];
        let board = growth_readiness_from(assess(&facts_with(&connections, &platforms, false)));
        assert!(!board.ready);
        let blocker = board.blocker.expect("a blocker");
        assert_eq!(blocker.code, "standing_authority_not_granted");
        assert!(blocker.owner_action);
    }

    #[test]
    fn a_ready_rail_has_no_blocker_but_keeps_its_caveat() {
        let connections = [facebook(PublishPermission::Unverified)];
        let platforms = vec!["facebook".to_owned()];
        let board = growth_readiness_from(assess(&facts_with(&connections, &platforms, true)));
        assert!(board.ready);
        assert_eq!(board.blocker, None::<GrowthBlocker>);
        assert_eq!(board.caveats.len(), 1, "{:?}", board.caveats);
    }

    #[test]
    fn deployment_work_is_not_offered_to_the_owner() {
        let connections = [facebook(PublishPermission::Verified)];
        let platforms = vec!["facebook".to_owned()];
        let mut facts = facts_with(&connections, &platforms, true);
        facts.social_publish_runtime = Some(false);
        let blocker = growth_readiness_from(assess(&facts)).blocker.expect("blocker");
        assert_eq!(blocker.code, "deployment_publish_gate_off");
        assert!(!blocker.owner_action);
    }
}
