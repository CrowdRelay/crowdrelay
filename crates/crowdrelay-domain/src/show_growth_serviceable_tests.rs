//! `evaluate_show_growth_serviceable` — the ladder with levers nothing can
//! run passed over. Split from `show_growth.rs`, which sits at the source
//! size threshold.

use super::*;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
}

fn snapshot(days: i64) -> ShowGrowthSnapshot {
    ShowGrowthSnapshot {
        event_id: EventId::new(),
        published: true,
        communication_enabled: true,
        starts_at: now() + Duration::days(days),
        capacity: 100,
        paid_tickets: 8,
        paid_buyers: 6,
        paid_tickets_last_7d: 2,
        interested_fans: 30,
        city_signal_fans: 20,
        qualified_referrers_in_city: 4,
        beacon_partners: 0,
        attendees: 0,
        morning_after_send_at: None,
        unreciprocated_crossbill_edge: false,
        ladder_approved: false,
        history: ShowGrowthHistory {
            canonical_link_setup_requested: true,
            ..ShowGrowthHistory::default()
        },
    }
}

/// The Gorzów case: three weeks out, no external executor. The ladder
/// must not stop at the free listing sweep it cannot run — the push to
/// fans in the city is still due.
#[test]
fn external_levers_are_passed_over_when_nothing_can_run_them() {
    let data = snapshot(21);
    let (decision, passed_over) =
        evaluate_show_growth_serviceable(data, ShowGrowthPolicy::default(), now(), false);
    let ShowGrowthDecision::Request { lever, .. } = decision else {
        panic!("a first-party lever is due: {decision:?}, passed over {passed_over:?}");
    };
    assert!(lever.is_first_party(), "{lever:?}");
    assert!(passed_over.contains(&ShowGrowthLever::FreeListingSweep));
    assert!(passed_over.iter().all(|lever| !lever.is_first_party()));

    // With an executor live, the ladder is exactly the plain one.
    let (live, none) =
        evaluate_show_growth_serviceable(data, ShowGrowthPolicy::default(), now(), true);
    assert_eq!(
        live,
        evaluate_show_growth(data, ShowGrowthPolicy::default(), now())
    );
    assert!(none.is_empty());
}

#[test]
fn the_tracked_link_is_never_passed_over() {
    let mut data = snapshot(21);
    data.history.canonical_link_setup_requested = false;
    let (decision, passed_over) =
        evaluate_show_growth_serviceable(data, ShowGrowthPolicy::default(), now(), false);
    assert!(matches!(
        decision,
        ShowGrowthDecision::Request {
            lever: ShowGrowthLever::CanonicalLinkSetup,
            ..
        }
    ));
    assert!(passed_over.is_empty());
}
