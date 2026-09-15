// Unit tests for the timeline ladder live here rather than at the bottom of
// timeline.rs — the modularity contract's 1000-line chunk budget applies to
// test code the same as product code, and the ladder's state matrix is big
// enough to need its own file.
#[cfg(test)]
mod timeline_tests {
    use super::*;

    fn facts(now: OffsetDateTime) -> TimelineFacts {
        TimelineFacts {
            event: TimelineEventRow {
                id: Uuid::now_v7(),
                slug: "friday".to_string(),
                title: "Friday".to_string(),
                venue: Some("Klub".to_string()),
                status: "published".to_string(),
                starts_at: now + Duration::days(10),
                ends_at: None,
            },
            emissions: Vec::new(),
            surfaces: Vec::new(),
            pace: TimelinePaceRow {
                capacity: Some(300),
                paid_tickets: 42,
                paid_tickets_last_7d: 5,
            },
            latest_decision: None,
            growth_actions: Vec::new(),
            assignments: Vec::new(),
            play_steps: Vec::new(),
            checklist: Vec::new(),
            counts: TimelineCountsRow {
                nearby_notified: 0,
                qr_campaigns: 0,
                checkins: 0,
            },
            harvest: TimelineHarvestRow {
                occurred_at: None,
                pending_requests: 0,
                succeeded_requests: 0,
            },
            cost: None,
        }
    }

    fn states(steps: &[TimelineStepView]) -> Vec<(&'static str, &'static str)> {
        steps.iter().map(|s| (s.key, s.state)).collect()
    }

    #[test]
    fn the_ladder_is_nine_steps_in_time_order() {
        let now = OffsetDateTime::now_utc();
        let steps = build_steps(&facts(now), now);
        assert_eq!(
            steps.iter().map(|s| s.anchor).collect::<Vec<_>>(),
            ["T-21", "T-14", "T-7", "T-2", "T-0", "T-0", "T+1", "T+3", "T+7"]
        );
    }

    #[test]
    fn a_show_ten_days_out_waits_on_most_of_the_ladder() {
        let now = OffsetDateTime::now_utc();
        let steps = build_steps(&facts(now), now);
        let states = states(&steps);
        // T-21 passed eleven days ago with no emission: due.
        assert_eq!(states[0], ("announced", "due"));
        // Inside T-14 with no brain read on record: due.
        assert_eq!(states[1], ("sales_pace", "due"));
        assert_eq!(states[2], ("bands_posting", "waiting"));
        assert_eq!(states[3], ("nearby_fans", "waiting"));
        assert_eq!(states[4], ("capture_plan", "waiting"));
        // No campaign ten days out: waiting — the QR becomes urgent inside
        // the last week, not before.
        assert_eq!(states[5], ("the_scan", "waiting"));
        assert_eq!(states[6], ("recall", "waiting"));
        assert_eq!(states[7], ("harvest", "waiting"));
        assert_eq!(states[8], ("the_numbers", "waiting"));
    }

    #[test]
    fn the_emission_marks_the_announcement_done() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.emissions.push(LifecycleEmissionRow {
            phase: "announcement".to_string(),
            emitted_at: now - Duration::days(3),
        });
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[0], ("announced", "done"));
    }

    #[test]
    fn a_checkin_marks_the_scan_done_and_the_count_is_the_detail() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::hours(2);
        facts.counts.checkins = 17;
        facts.counts.qr_campaigns = 1;
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[5], ("the_scan", "done"));
        assert_eq!(steps[5].detail["checkins"], 17);
    }

    #[test]
    fn a_past_show_with_no_scan_reports_skipped_not_failed() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(2);
        facts.event.status = "completed".to_string();
        let steps = build_steps(&facts, now);
        let states = states(&steps);
        assert_eq!(states[5], ("the_scan", "skipped"));
        // Every pre-show step whose window closed without evidence reads
        // skipped — a played night must not light five stale "due" badges.
        assert_eq!(states[0], ("announced", "skipped"));
        assert_eq!(states[1], ("sales_pace", "skipped"));
        assert_eq!(states[2], ("bands_posting", "skipped"));
        assert_eq!(states[3], ("nearby_fans", "skipped"));
        assert_eq!(states[4], ("capture_plan", "skipped"));
    }

    #[test]
    fn an_all_skipped_play_reports_skipped_not_done() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(2);
        facts.play_steps.push(TimelinePlayStepRow {
            step_kind: "announce_ask".to_string(),
            due_at: now - Duration::days(3),
            settled_at: Some(now - Duration::days(3)),
            skip_reason: Some("no_member_channel".to_string()),
        });
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[2], ("bands_posting", "skipped"));
    }

    #[test]
    fn the_harvest_window_open_without_requests_is_active_not_done() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(1);
        facts.event.status = "completed".to_string();
        facts.harvest.occurred_at = Some(now - Duration::hours(10));
        let steps = build_steps(&facts, now);
        // The brain holds artifact requests for 72h after the source lands —
        // "source exists, nothing requested yet" is in-flight work.
        assert_eq!(states(&steps)[7], ("harvest", "active"));
        // Past the window with nothing collected: skipped.
        facts.harvest.occurred_at = Some(now - Duration::hours(80));
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[7], ("harvest", "skipped"));
        // A collected artifact is the only thing that says done.
        facts.harvest.succeeded_requests = 2;
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[7], ("harvest", "done"));
    }

    #[test]
    fn a_failed_recap_after_the_window_reads_skipped() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(2);
        facts.event.status = "completed".to_string();
        facts.growth_actions.push(ShowGrowthActionRow {
            id: Uuid::now_v7(),
            status: "failed".to_string(),
            lever: Some("post_show_recap".to_string()),
            available_at: now - Duration::hours(30),
            finished_at: Some(now - Duration::hours(29)),
        });
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[6], ("recall", "skipped"));
    }

    #[test]
    fn the_report_checklist_item_carries_owner_and_state() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(8);
        facts.event.status = "completed".to_string();
        facts.checklist.push(TimelineChecklistRow {
            item_key: "post_show_report".to_string(),
            status: "pending".to_string(),
        });
        facts.assignments.push(TimelineAssignmentRow {
            source_kind: "show_task".to_string(),
            source_ref: Some("post_show_reconciliation".to_string()),
            action_id: None,
            status: "open".to_string(),
            due_at: Some(now + Duration::days(1)),
            display_name: Some("Ola".to_string()),
        });
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[8], ("the_numbers", "due"));
        assert_eq!(steps[8].owner.as_deref(), Some("Ola"));
        assert_eq!(steps[8].action.as_ref().unwrap().kind, "report");
    }

    #[test]
    fn a_succeeded_recap_is_done_with_its_send_time() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(2);
        facts.event.status = "completed".to_string();
        facts.growth_actions.push(ShowGrowthActionRow {
            id: Uuid::now_v7(),
            status: "succeeded".to_string(),
            lever: Some("post_show_recap".to_string()),
            available_at: now - Duration::hours(20),
            finished_at: Some(now - Duration::hours(19)),
        });
        let steps = build_steps(&facts, now);
        assert_eq!(states(&steps)[6], ("recall", "done"));
    }
}
