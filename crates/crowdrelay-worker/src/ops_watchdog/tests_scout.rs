// The FAN SCOUT lane's tripwire conditions, split out of `tests.rs` for the
// source-size ratchet — same `include!` family as `tests_outreach.rs`.

#[cfg(test)]
mod scout_tests {
    use super::tests::{healthy, publishing};
    use super::conditions;
    use crowdrelay_infra::scout_lane::Breach;

    /// Each breach raises exactly its own critical condition, and a clean lane
    /// raises none: the key is the one the senders' halt reports, so what the
    /// operator reads and what the lane does are the same fact.
    #[test]
    fn each_scout_breach_raises_its_own_critical_condition_and_nothing_else() {
        let active_scout_keys = |breaches: Vec<Breach>| {
            let mut snapshot = healthy();
            snapshot.scout_breaches = breaches;
            conditions(&snapshot, publishing())
                .into_iter()
                .filter(|c| c.key.starts_with("scout.") && c.active)
                .map(|c| {
                    assert_eq!(c.severity, "critical", "{}", c.key);
                    c.key
                })
                .collect::<Vec<_>>()
        };
        assert!(active_scout_keys(Vec::new()).is_empty());
        // The literal keys, so the alarm surface is greppable and the
        // documented-conditions gate sees each one named in a test.
        assert_eq!(
            Breach::ALL.map(Breach::key),
            [
                "scout.contacted_suppressed",
                "scout.over_rate",
                "scout.invite_without_route",
                "scout.untracked_link",
            ]
        );
        for breach in Breach::ALL {
            assert_eq!(active_scout_keys(vec![breach]), [breach.key()], "{breach:?}");
        }
        assert_eq!(active_scout_keys(Breach::ALL.to_vec()).len(), Breach::ALL.len());
        // Every breach has a condition: none can be added without one.
        let evaluated: Vec<_> = conditions(&healthy(), publishing())
            .into_iter()
            .map(|c| c.key)
            .collect();
        for breach in Breach::ALL {
            assert!(evaluated.contains(&breach.key()), "{breach:?}");
        }
    }
}
