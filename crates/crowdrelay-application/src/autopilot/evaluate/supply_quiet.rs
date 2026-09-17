/// §1.6 stop rule — which quiet the supply shelf is in. "Live" is the
/// evaluator's own verdict rather than `active` alone: a shelf of expired
/// sources must not read as activity, so the count asks the same question
/// the candidates do. Three honest cases: nothing on file at all, only
/// retired material, or live material with nothing owed.
fn supply_quiet_reason(
    snapshots: &[ContentSupplySnapshot],
    policy: &AutopilotPolicy,
    produced: usize,
    now: OffsetDateTime,
) -> Option<String> {
    let AutopilotPolicyConfig::ContentSupply(supply_policy) = &policy.config else {
        return None;
    };
    let live = snapshots
        .iter()
        .filter(|snapshot| {
            !matches!(
                evaluate_content_supply(snapshot, *supply_policy, now),
                ContentSupplyDecision::Hold(
                    ContentSupplyHoldReason::InvalidSnapshot
                        | ContentSupplyHoldReason::StaleSource
                )
            )
        })
        .count();
    if live == 0 {
        Some(if snapshots.is_empty() {
            "no live material — no event, release, show, video, post or story is on file".to_owned()
        } else {
            "no live material — everything on file is retired or past its shareable window"
                .to_owned()
        })
    } else {
        (produced == 0).then(|| {
            "material on file has produced everything owed — waiting for something new to share"
                .to_owned()
        })
    }
}
