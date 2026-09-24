// Unit tests for the timeline ladder live here rather than at the bottom of
// timeline.rs — the modularity contract's 1000-line chunk budget applies to
// test code the same as product code, and the ladder's state matrix is big
// enough to need its own file.
#[cfg(test)]
mod timeline_tests {
    use super::*;

    fn act(slug: &str, name: &str, position: i32, ticket_url: Option<&str>) -> TimelineActRow {
        TimelineActRow {
            act_slug: slug.to_string(),
            act_name: name.to_string(),
            position,
            ticket_url: ticket_url.map(ToString::to_string),
        }
    }

    /// 4V.5b: the bill renders in the order the night has, with each act's own
    /// link, and a missing link stays missing.
    ///
    /// `event_acts` has carried this since migration 0271 and the gig page
    /// never showed it — a page that described nine steps of work and could
    /// not say who was playing.
    #[test]
    fn the_bill_keeps_the_night_s_own_order_and_links() {
        let rows = vec![
            act("opener", "Opener", 0, None),
            act("virya", "Virya", 1, Some("https://tickets.example/virya")),
        ];
        let bill = bill_view(&rows);
        assert_eq!(bill.len(), 2);
        assert_eq!(bill[0]["name"], "Opener");
        assert_eq!(bill[1]["name"], "Virya");
        // Position travels, so a console can say "opens" and "headlines"
        // without re-deriving the order from the array index.
        assert_eq!(bill[0]["position"], 0);
        assert_eq!(bill[1]["position"], 1);
        // An act with no link of its own is null, never the other act's link.
        assert!(bill[0]["ticket_url"].is_null());
        assert_eq!(bill[1]["ticket_url"], "https://tickets.example/virya");
    }

    /// A solo night is an empty bill, not a bill of one that reads as a
    /// co-headline.
    #[test]
    fn a_solo_night_has_no_support_on_the_page() {
        assert!(bill_view(&[]).is_empty());
    }

    fn facts(now: OffsetDateTime) -> TimelineFacts {
        TimelineFacts {
            event: TimelineEventRow {
                id: Uuid::now_v7(),
                slug: "friday".to_string(),
                title: "Friday".to_string(),
                venue: Some("Klub".to_string()),
                venue_address: None,
                status: "published".to_string(),
                starts_at: now + Duration::days(10),
                ends_at: None,
                counterparty_name: None,
                counterparty_email: None,
                place_event_id: None,
                booking_opportunity_id: None,
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
            room: crowdrelay_infra::concert_room::RoomSplit::default(),
            harvest: TimelineHarvestRow {
                occurred_at: None,
                pending_requests: 0,
                succeeded_requests: 0,
            },
            cost: None,
            booking: None,
            crossbill_acts: Vec::new(),
            crossbill_edge: None,
            venue_beacons: Vec::new(),
            recap_campaign: None,
        }
    }

    /// One step by key.
    ///
    /// These assertions used to index the ladder positionally, so inserting a
    /// rung moved every later assertion onto its neighbour — which still
    /// compiles and still passes for the wrong reason. The key is what the
    /// test is actually about.
    fn at<'a>(steps: &'a [TimelineStepView], key: &str) -> &'a TimelineStepView {
        steps
            .iter()
            .find(|step| step.key == key)
            .unwrap_or_else(|| panic!("the ladder has no {key} step"))
    }

    fn state_of(steps: &[TimelineStepView], key: &str) -> &'static str {
        at(steps, key).state
    }

    #[test]
    fn the_ladder_is_ten_steps_in_time_order() {
        let now = OffsetDateTime::now_utc();
        let steps = build_steps(&facts(now), now);
        assert_eq!(
            steps.iter().map(|s| s.anchor).collect::<Vec<_>>(),
            [
                "before T-21",
                "T-21",
                "T-14",
                "T-7",
                "T-2",
                "T-0",
                "T-0",
                "T+1",
                "T+3",
                "T+7"
            ]
        );
    }

    /// A show exists because somebody got the date, so the first rung is
    /// always done. The detail is what separates a night that was won from one
    /// that was offered.
    #[test]
    fn the_ladder_opens_on_the_booking_and_says_which_kind_it_was() {
        let now = OffsetDateTime::now_utc();
        let steps = build_steps(&facts(now), now);
        let booked = at(&steps, "booked");
        assert_eq!(booked.state, "done");
        assert_eq!(
            booked.detail["origin"], "direct",
            "no negotiation behind this night is an answer, not a gap"
        );

        let mut negotiated = facts(now);
        negotiated.booking = Some(TimelineBookingRow {
            organization: "Klub Y".to_string(),
            state: "accepted".to_string(),
            offered_fee_minor: Some(280_000),
            settled_at: Some(now - Duration::days(40)),
            counter_rounds: Some(2),
            currency: Some("PLN".to_string()),
        });
        let steps = build_steps(&negotiated, now);
        let booked = at(&steps, "booked");
        assert_eq!(booked.detail["origin"], "negotiated");
        assert_eq!(booked.detail["fee_minor"], 280_000);
        assert_eq!(
            booked.detail["counter_rounds"], 2,
            "what the date cost in asks"
        );
    }

    #[test]
    fn a_show_ten_days_out_waits_on_most_of_the_ladder() {
        let now = OffsetDateTime::now_utc();
        let steps = build_steps(&facts(now), now);
        // T-21 passed eleven days ago with no emission: due.
        assert_eq!(state_of(&steps, "announced"), "due");
        // Inside T-14 with no brain read on record: due.
        assert_eq!(state_of(&steps, "sales_pace"), "due");
        assert_eq!(state_of(&steps, "bands_posting"), "waiting");
        assert_eq!(state_of(&steps, "nearby_fans"), "waiting");
        assert_eq!(state_of(&steps, "capture_plan"), "waiting");
        // No campaign ten days out: waiting — the QR becomes urgent inside
        // the last week, not before.
        assert_eq!(state_of(&steps, "the_scan"), "waiting");
        assert_eq!(state_of(&steps, "recall"), "waiting");
        assert_eq!(state_of(&steps, "harvest"), "waiting");
        assert_eq!(state_of(&steps, "the_numbers"), "waiting");
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
        assert_eq!(state_of(&steps, "announced"), "done");
    }

    #[test]
    fn a_checkin_marks_the_scan_done_and_the_count_is_the_detail() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::hours(2);
        facts.counts.checkins = 17;
        facts.counts.qr_campaigns = 1;
        let steps = build_steps(&facts, now);
        assert_eq!(state_of(&steps, "the_scan"), "done");
        assert_eq!(at(&steps, "the_scan").detail["checkins"], 17);
    }

    #[test]
    fn the_scan_says_what_the_room_produced_not_only_how_many_scanned() {
        // Seventeen scans, of which five were new to the band, three of those
        // already reachable and two still unconfirmed — the other twelve
        // were fans the band already had. Only the five are aggregation.
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::hours(2);
        facts.counts.checkins = 17;
        facts.counts.qr_campaigns = 1;
        facts.room = crowdrelay_infra::concert_room::RoomSplit {
            new_fans: 5,
            new_reachable: 3,
            new_unconfirmed: 2,
        };
        let steps = build_steps(&facts, now);
        let detail = &at(&steps, "the_scan").detail;
        assert_eq!(detail["checkins"], 17);
        assert_eq!(detail["new_fans"], 5);
        assert_eq!(detail["new_reachable"], 3);
        assert_eq!(detail["new_unconfirmed"], 2);
    }

    #[test]
    fn a_past_show_with_no_scan_reports_skipped_not_failed() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(2);
        facts.event.status = "completed".to_string();
        let steps = build_steps(&facts, now);
        assert_eq!(state_of(&steps, "the_scan"), "skipped");
        // Every pre-show step whose window closed without evidence reads
        // skipped — a played night must not light five stale "due" badges.
        assert_eq!(state_of(&steps, "announced"), "skipped");
        assert_eq!(state_of(&steps, "sales_pace"), "skipped");
        assert_eq!(state_of(&steps, "bands_posting"), "skipped");
        assert_eq!(state_of(&steps, "nearby_fans"), "skipped");
        assert_eq!(state_of(&steps, "capture_plan"), "skipped");
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
        assert_eq!(state_of(&steps, "bands_posting"), "skipped");
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
        assert_eq!(state_of(&steps, "harvest"), "active");
        // Past the window with nothing collected: skipped.
        facts.harvest.occurred_at = Some(now - Duration::hours(80));
        let steps = build_steps(&facts, now);
        assert_eq!(state_of(&steps, "harvest"), "skipped");
        // A collected artifact is the only thing that says done.
        facts.harvest.succeeded_requests = 2;
        let steps = build_steps(&facts, now);
        assert_eq!(state_of(&steps, "harvest"), "done");
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
        assert_eq!(state_of(&steps, "recall"), "skipped");
    }

    #[test]
    fn a_missing_recap_inside_the_open_window_is_due_not_waiting() {
        let now = OffsetDateTime::now_utc();
        // Before the show starts, absence is `waiting` — the brain cannot
        // have requested a recap for a night that has not happened.
        let steps = build_steps(&facts(now), now);
        assert_eq!(state_of(&steps, "recall"), "waiting");
        // Two hours after doors the recap window is open (since_show <= 30h)
        // and the brain has queued nothing — absence inside an open window is
        // `due`, the same contract every other step keeps.
        let mut during_show = facts(now);
        during_show.event.starts_at = now - Duration::hours(2);
        let steps = build_steps(&during_show, now);
        assert_eq!(state_of(&steps, "recall"), "due");
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
        assert_eq!(state_of(&steps, "the_numbers"), "due");
        assert_eq!(at(&steps, "the_numbers").owner.as_deref(), Some("Ola"));
        assert_eq!(at(&steps, "the_numbers").action.as_ref().unwrap().kind, "report");
    }

    #[test]
    fn a_done_report_keeps_the_artifact_link() {
        let now = OffsetDateTime::now_utc();
        let mut facts = facts(now);
        facts.event.starts_at = now - Duration::days(8);
        facts.event.status = "completed".to_string();
        facts.checklist.push(TimelineChecklistRow {
            item_key: "post_show_report".to_string(),
            status: "done".to_string(),
        });
        let steps = build_steps(&facts, now);
        assert_eq!(state_of(&steps, "the_numbers"), "done");
        let action = at(&steps, "the_numbers").action.as_ref().unwrap();
        assert_eq!(action.kind, "report");
        assert_eq!(action.label, "See the report");
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
        assert_eq!(state_of(&steps, "recall"), "done");
    }
}
