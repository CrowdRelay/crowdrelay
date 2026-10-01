// The outreach-linkage conditions, split out of `tests.rs` for the
// source-size ratchet — same `include!` family as `tests_video.rs`. The
// helpers (`healthy`, `publishing`) live in the sibling `tests` module.

#[cfg(test)]
mod outreach_tests {
    use super::tests::{healthy, publishing};
    use super::{OpsSnapshot, conditions};

    /// A letter that went out carrying a link nobody can count is the failure
    /// the composer gate exists to prevent — a path around it (a hand-edited
    /// draft, an executor rewrite) is critical, not a nit. The composer gate
    /// makes the shape unrepresentable going forward; this alarm is what makes
    /// a workaround loud.
    #[test]
    fn an_untracked_letter_link_raises_attention() {
        let find = |snapshot: &OpsSnapshot| {
            conditions(snapshot, publishing())
                .into_iter()
                .find(|c| c.key == "outreach.untracked_link_sent")
                .expect("the condition is evaluated")
        };
        assert!(!find(&healthy()).active);
        let mut snapshot = healthy();
        snapshot.untracked_letter_sends_24h = 3;
        let raised = find(&snapshot);
        assert!(raised.active);
        assert_eq!(raised.severity, "critical");
        assert_eq!(raised.details["letters"], 3);
    }

    /// Somebody who answered yes and then received another pitch: the engine
    /// cannot do this, so a counted row is correspondence from outside it.
    #[test]
    fn a_pitch_after_a_yes_raises_attention() {
        let find = |snapshot: &OpsSnapshot| {
            conditions(snapshot, publishing())
                .into_iter()
                .find(|c| c.key == "outreach.mailed_after_yes")
                .expect("the condition is evaluated")
        };
        assert!(!find(&healthy()).active);
        let mut snapshot = healthy();
        snapshot.mailed_after_yes_14d = 90;
        snapshot.mailed_after_yes_targets_14d = 25;
        let raised = find(&snapshot);
        assert!(raised.active);
        assert_eq!(raised.severity, "warning");
        assert_eq!(raised.details["mails"], 90);
        assert_eq!(raised.details["people"], 25);
    }
}
