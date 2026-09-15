// The gig page's one night: the T-21→T+7 ladder as nine steps, each backed
// by the artifact that records it. `control_plane_events` answers "which
// shows"; this answers "what state is Friday in" — each step's state, the
// owner the handoff index names, and the one action open now, in time order.
//
// Facts load in `timeline_facts.rs`; this file owns the response shape and
// the step builder. Missing evidence is null, not zero — a step whose
// measurement does not exist yet says so by carrying no detail rather than
// a fabricated count.

#[derive(Debug, Serialize)]
struct TimelineActionView {
    kind: &'static str,
    label: &'static str,
}

#[derive(Debug, Serialize)]
struct TimelineStepView {
    key: &'static str,
    label: &'static str,
    /// Where the step sits on the ladder — an anchor off `starts_at`, not a
    /// timestamp, because "T-14" is how a band reads the week.
    anchor: &'static str,
    /// `done` the artifact completed; `active` work is in flight; `due` the
    /// window is open and the step needs doing; `waiting` the window has not
    /// opened; `skipped` the window closed without the step. There is no
    /// "unknown": a step with no evidence reports `due` once its window is
    /// open and `waiting` before it — absence is itself the state.
    state: &'static str,
    owner: Option<String>,
    action: Option<TimelineActionView>,
    detail: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ControlPlaneEventTimelineResponse {
    event: TimelineEventView,
    steps: Vec<TimelineStepView>,
}

#[derive(Debug, Serialize)]
struct TimelineEventView {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    venue_address: Option<String>,
    status: String,
    starts_at: String,
    ends_at: Option<String>,
    /// What the night knows about the room itself — the beacon-campaign
    /// record keyed to this event (relationship status, last reply, notes).
    venue_knowledge: Vec<serde_json::Value>,
}

/// `GET /v1/control-plane/events/{event_slug}/timeline` — the nine-step
/// ladder for one show. Published and completed events both resolve: a
/// played night still owes the band T+1, T+3 and T+7.
pub async fn control_plane_event_timeline(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let facts = match crate::ops::hold(
        &state.read_budget,
        load_timeline_facts(&state.concert_qr, &event_slug),
    )
    .await
    {
        Ok(Some(facts)) => facts,
        Ok(None) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "control-plane event timeline query failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let steps = build_steps(&facts, OffsetDateTime::now_utc());
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ControlPlaneEventTimelineResponse {
            event: TimelineEventView {
                id: facts.event.id,
                slug: facts.event.slug.clone(),
                title: facts.event.title.clone(),
                venue: facts.event.venue.clone(),
                venue_address: facts.event.venue_address.clone(),
                status: facts.event.status.clone(),
                starts_at: format_time(facts.event.starts_at),
                ends_at: facts.event.ends_at.map(format_time),
                venue_knowledge: facts
                    .venue_beacons
                    .iter()
                    .map(|b| serde_json::json!({
                        "name": b.display_name,
                        "kind": b.beacon_kind,
                        "status": b.status,
                        "last_reply": b.last_reply_disposition,
                        "last_outreach_at": b.last_outreach_at.map(format_time),
                        "notes": b.notes,
                    }))
                    .collect(),
            },
            steps,
        }),
    )
        .into_response()
}

fn task_owner<'a>(facts: &'a TimelineFacts, item_key: &str) -> Option<&'a str> {
    facts
        .assignments
        .iter()
        .find(|a| {
            a.status == "open" && a.source_kind == "show_task"
                && a.source_ref.as_deref() == Some(item_key)
        })
        .and_then(|a| a.display_name.as_deref())
}

/// The assignee of one specific open action. Owner slots name the person the
/// displayed action belongs to — an approver of an unrelated action must not
/// surface as the step's owner.
fn action_owner(facts: &TimelineFacts, action_id: Uuid) -> Option<&str> {
    facts
        .assignments
        .iter()
        .find(|a| {
            a.status == "open" && a.source_kind == "autopilot_action"
                && a.action_id == Some(action_id)
        })
        .and_then(|a| a.display_name.as_deref())
}

/// The pending lever that belongs to the pre-show window. Post-show levers
/// (recap, merch follow-up, follow ask) belong to the T+ steps — surfacing
/// one of them under "sales pace" would misattribute it.
fn pending_action(facts: &TimelineFacts) -> Option<&ShowGrowthActionRow> {
    facts
        .growth_actions
        .iter()
        .find(|a| {
            matches!(a.status.as_str(), "awaiting_approval" | "queued" | "processing")
                && !a
                    .lever
                    .as_deref()
                    .is_some_and(|lever| lever.starts_with("post_show"))
        })
}

fn checklist_status<'a>(facts: &'a TimelineFacts, item_key: &str) -> Option<&'a str> {
    facts
        .checklist
        .iter()
        .find(|c| c.item_key == item_key)
        .map(|c| c.status.as_str())
}

fn step(
    key: &'static str,
    label: &'static str,
    anchor: &'static str,
    state: &'static str,
    owner: Option<String>,
    action: Option<TimelineActionView>,
    detail: serde_json::Value,
) -> TimelineStepView {
    TimelineStepView {
        key,
        label,
        anchor,
        state,
        owner,
        action,
        detail,
    }
}

fn build_steps(facts: &TimelineFacts, now: OffsetDateTime) -> Vec<TimelineStepView> {
    let starts = facts.event.starts_at;
    let ends = facts.event.ends_at.unwrap_or(starts + Duration::hours(6));
    // Once the night has ended, a pre-show step with no evidence did not
    // fail — its window closed without it. `skipped`, never a stale `due`
    // offering "Announce it" for a show that already played.
    let show_over = now > ends;
    let mut steps = Vec::with_capacity(9);

    // T-21 — announced. Proof is the lifecycle emission; the surfaces say
    // where the announcement actually landed. The shared bill rides here:
    // a second act makes the crossbill real, the consent edge says whether
    // the system can push it itself, and the cap is the consent's own.
    let announced = facts.emissions.iter().find(|e| e.phase == "announcement");
    let crossbill_state = if facts.crossbill_acts.len() <= 1 {
        "no_support_bill"
    } else if facts.crossbill_edge.is_some() {
        "automated_overlap"
    } else {
        "manual_ask"
    };
    let announced_detail = serde_json::json!({
        "emitted_at": announced.map(|e| format_time(e.emitted_at)),
        "surfaces": facts
            .surfaces
            .iter()
            .map(|s| serde_json::json!({"surface": s.surface_key, "status": s.status}))
            .collect::<Vec<_>>(),
        "crossbill": {
            "state": crossbill_state,
            "acts": facts
                .crossbill_acts
                .iter()
                .map(|a| serde_json::json!({"slug": a.act_slug, "name": a.act_name}))
                .collect::<Vec<_>>(),
            "cap_per_month": facts.crossbill_edge.as_ref().map(|e| e.max_campaigns_per_month),
            "cooldown_days": facts.crossbill_edge.as_ref().map(|e| e.cooldown_days),
            "deliveries_this_month": facts.crossbill_edge.as_ref().map(|e| e.deliveries_this_month),
        },
    });
    let (announced_state, announced_action) = if announced.is_some() {
        ("done", None)
    } else if show_over {
        ("skipped", None)
    } else if now >= starts - Duration::days(21) {
        ("due", Some(TimelineActionView { kind: "announce", label: "Announce it" }))
    } else {
        ("waiting", None)
    };
    steps.push(step("announced", "Announced", "T-21", announced_state, None, announced_action, announced_detail));

    // T-14 — sales pace. The numbers come from the same predicates the
    // brain's snapshot uses; the verdict comes from the brain's last
    // decision, quoted rather than re-derived.
    let pending = pending_action(facts);
    let pace_state = if facts.latest_decision.is_some() {
        "active"
    } else if show_over {
        "skipped"
    } else if now >= starts - Duration::days(14) {
        "due"
    } else {
        "waiting"
    };
    let pace_action = pending.map(|_| TimelineActionView {
        kind: "review",
        label: "Review the move the brain queued",
    });
    let pace_detail = serde_json::json!({
        "paid_tickets": facts.pace.paid_tickets,
        "capacity": facts.pace.capacity,
        "paid_tickets_last_7d": facts.pace.paid_tickets_last_7d,
        "last_read": facts.latest_decision.as_ref().map(|d| serde_json::json!({
            "reason": d.reason,
            "evaluated_at": format_time(d.evaluated_at),
        })),
        "pending_move": pending.map(|a| serde_json::json!({
            "lever": a.lever,
            "status": a.status,
        })),
    });
    let pace_owner = pending.and_then(|a| action_owner(facts, a.id));
    steps.push(step("sales_pace", "Sales pace", "T-14", pace_state, pace_owner.map(str::to_owned), pace_action, pace_detail));

    // T-7 — the bands posting. Plays carry the asks themselves. A skipped
    // step is settled but not done: an all-skipped play means nobody posted,
    // and the badge has to say so.
    let open_steps = facts
        .play_steps
        .iter()
        .filter(|s| s.settled_at.is_none())
        .count();
    let settled_steps = facts.play_steps.len() - open_steps;
    let successful_steps = facts
        .play_steps
        .iter()
        .filter(|s| s.settled_at.is_some() && s.skip_reason.is_none())
        .count();
    let posting_state = if open_steps > 0 && !show_over {
        "active"
    } else if successful_steps > 0 {
        "done"
    } else if settled_steps > 0 || show_over {
        "skipped"
    } else if now >= starts - Duration::days(7) {
        "due"
    } else {
        "waiting"
    };
    let posting_detail = serde_json::json!({
        "open": open_steps,
        "settled": settled_steps,
        "skipped": facts
            .play_steps
            .iter()
            .filter_map(|s| s.skip_reason.as_deref())
            .collect::<Vec<_>>(),
        "open_asks": facts
            .play_steps
            .iter()
            .filter(|s| s.settled_at.is_none())
            .map(|s| serde_json::json!({
                "kind": s.step_kind,
                "due_at": format_time(s.due_at),
            }))
            .collect::<Vec<_>>(),
        "next_assignment_due": facts
            .assignments
            .iter()
            .filter(|a| a.status == "open" && a.source_kind == "show_task")
            .filter_map(|a| a.due_at)
            .min()
            .map(format_time),
    });
    steps.push(step("bands_posting", "Bands posting", "T-7", posting_state, None, None, posting_detail));

    // T-2 — nearby fans. The dedupe ledger is the proof a fan was told; the
    // scheduler's 15-minute poll owns the send, so there is no human action
    // to offer — only the fact and the count.
    let nearby_state = if facts.counts.nearby_notified > 0 {
        "done"
    } else if show_over {
        "skipped"
    } else if now >= starts - Duration::days(2) {
        "due"
    } else {
        "waiting"
    };
    steps.push(step(
        "nearby_fans",
        "Nearby fans",
        "T-2",
        nearby_state,
        None,
        None,
        serde_json::json!({ "notified": facts.counts.nearby_notified }),
    ));

    // T-0 — the capture plan, owned by whoever the show_task handoff named.
    let capture_status = checklist_status(facts, "capture_plan").unwrap_or("pending");
    let capture_state = if capture_status == "done" {
        "done"
    } else if show_over {
        "skipped"
    } else if now >= starts - Duration::hours(30) {
        "due"
    } else {
        "waiting"
    };
    let capture_action = if capture_state == "due" {
        Some(TimelineActionView { kind: "capture_plan", label: "Confirm the capture plan" })
    } else {
        None
    };
    steps.push(step(
        "capture_plan",
        "Capture plan",
        "T-0",
        capture_state,
        task_owner(facts, "capture_plan").map(str::to_owned),
        capture_action,
        serde_json::json!({ "status": capture_status }),
    ));

    // T-0 — the scan itself. An active, unexpired campaign means the door
    // exists; check-ins are the night. The QR only becomes urgent inside the
    // last week — nagging about it three weeks out is noise. After the show
    // ends with no check-ins the window has closed: `skipped`, not failed.
    let scan_state = if facts.counts.checkins > 0 {
        "done"
    } else if show_over {
        "skipped"
    } else if facts.counts.qr_campaigns > 0 {
        "waiting"
    } else if now >= starts - Duration::days(7) {
        "due"
    } else {
        "waiting"
    };
    // The door link must survive the first check-in — the page doubles as
    // the live tally, and mid-show is when a second phone reaches for it.
    // Once the night is over the door is too, so the link retires with it.
    let scan_action = if show_over {
        None
    } else if (scan_state == "done" && facts.counts.qr_campaigns > 0)
        || scan_state == "due"
        || (scan_state == "waiting" && now >= starts - Duration::hours(6))
    {
        Some(TimelineActionView { kind: "qr", label: "Open the QR" })
    } else {
        None
    };
    steps.push(step(
        "the_scan",
        "The scan",
        "T-0",
        scan_state,
        None,
        scan_action,
        serde_json::json!({
            "checkins": facts.counts.checkins,
            "campaign_ready": facts.counts.qr_campaigns > 0,
        }),
    ));

    // T+1 — the recall. The latest recap action is the artifact; the brain
    // anchors the window on `starts_at` (post_show_recap_hours = 30), so the
    // ladder does too. A cancelled or failed recap after the window closed
    // reads `skipped` — the request is not coming back on its own.
    let recap = facts
        .growth_actions
        .iter()
        .rev()
        .find(|a| a.lever.as_deref() == Some("post_show_recap"));
    // The window is the brain's own `since_show <= post_show_recap_hours`,
    // edge included — a recap can only be requested once the show has
    // started, so absence before `starts` is `waiting` and absence inside
    // the open window is `due`, never a silent shrug.
    let recap_window_open = now <= starts + Duration::hours(30);
    let recap_state = match recap {
        Some(a) if a.status == "succeeded" => "done",
        Some(a) if matches!(a.status.as_str(), "awaiting_approval" | "queued" | "processing") => "active",
        _ if !recap_window_open => "skipped",
        Some(_) => "due",
        None if now >= starts => "due",
        None => "waiting",
    };
    let recap_action = match recap {
        Some(a) if a.status == "awaiting_approval" => {
            Some(TimelineActionView { kind: "approve", label: "Approve the recap" })
        }
        _ => None,
    };
    steps.push(step(
        "recall",
        "Recall",
        "T+1",
        recap_state,
        recap.and_then(|a| action_owner(facts, a.id)).map(str::to_owned),
        recap_action,
        serde_json::json!({
            "action_status": recap.map(|a| a.status.as_str()),
            "send_after": recap.map(|a| format_time(a.available_at)),
            "finished_at": recap.and_then(|a| a.finished_at).map(format_time),
            // The campaign is the artifact — subject and receipts ride the
            // step so the band sees the send, not just the action's state.
            "campaign": facts.recap_campaign.as_ref().map(|c| serde_json::json!({
                "slug": c.slug,
                "subject": c.subject,
                "status": c.status,
                "scheduled_at": c.scheduled_at.map(format_time),
                "delivered": c.delivered_count,
                "recipients": c.recipient_count,
            })),
        }),
    ));

    // T+3 — the harvest. The content source appears when the night closes
    // and the brain holds artifact requests for post_show_harvest_hours
    // (72h) after `occurred_at`, so "source exists, nothing requested yet"
    // inside that window is `active`, not `done`. The window closing with
    // nothing pending and nothing collected is `skipped`.
    let harvest_state = if facts.harvest.pending_requests > 0 {
        "active"
    } else if let Some(occurred_at) = facts.harvest.occurred_at {
        if facts.harvest.succeeded_requests > 0 {
            "done"
        } else if now < occurred_at + Duration::hours(72) {
            "active"
        } else {
            "skipped"
        }
    } else if now >= ends + Duration::days(3) {
        "due"
    } else {
        "waiting"
    };
    steps.push(step(
        "harvest",
        "Harvest",
        "T+3",
        harvest_state,
        None,
        None,
        serde_json::json!({
            "occurred_at": facts.harvest.occurred_at.map(format_time),
            "pending_requests": facts.harvest.pending_requests,
            "collected_requests": facts.harvest.succeeded_requests,
        }),
    ));

    // T+7 — the numbers. The checklist row is the band's confirmation; the
    // cost ledger is what the night actually cost, when someone settled it.
    let report_status = checklist_status(facts, "post_show_report").unwrap_or("pending");
    let numbers_state = if report_status == "done" {
        "done"
    } else if now >= starts + Duration::days(7) {
        "due"
    } else {
        "waiting"
    };
    // The artifact stays reachable after issuance — "See the report" is
    // the mailed payload; before T+7 opens the window the preview shows
    // what the send will say. The band never writes it, the system does.
    let numbers_action = match numbers_state {
        "done" => Some(TimelineActionView { kind: "report", label: "See the report" }),
        "due" => Some(TimelineActionView { kind: "report", label: "Preview the report" }),
        _ => None,
    };
    let numbers_detail = serde_json::json!({
        "status": report_status,
        "cost": facts.cost.as_ref().map(|c| serde_json::json!({
            "predicted_total_cost_minor": c.predicted_total_cost_minor,
            "settled_total_cost_minor": c.settled_total_cost_minor,
            "fee_received_minor": c.fee_received_minor,
            "accuracy": c.accuracy,
            "prediction_missing_input": c.prediction_missing_input,
        })),
    });
    steps.push(step(
        "the_numbers",
        "The numbers",
        "T+7",
        numbers_state,
        // The report itself is system-owned; the human handoff on this step
        // is the post-show reconciliation the team index assigns.
        task_owner(facts, "post_show_reconciliation").map(str::to_owned),
        numbers_action,
        numbers_detail,
    ));

    steps
}
