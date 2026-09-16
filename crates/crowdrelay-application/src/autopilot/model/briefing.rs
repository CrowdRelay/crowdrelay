// Operator briefings for every autopilot action payload.
//
// `include!`d into the `impl AutopilotActionPayload` block in `model.rs`, so
// it keeps that block's scope. Split out because the match is the largest
// thing in the file by a wide margin and was holding it at the source-size
// ratchet's ceiling.

impl AutopilotActionPayload {
    /// Generates a human-readable briefing for this action — what to do,
    /// why it matters, concrete steps, and the content being approved.
    ///
    /// Exhaustive on purpose: a new payload variant must not compile until
    /// somebody writes its briefing. The `deadline_note` is left empty here
    /// and filled by the caller from the action's deadline fields.
    #[must_use]
    pub fn briefing(&self) -> super::control::ActionBriefing {
        use super::control::{ActionBriefing, BriefingField, BriefingStep};

        // A raw UUID in a briefing tells a band member nothing — it is eight
        // lines of hex where a name belongs. `briefing` is a pure function on
        // the payload with no repository, so it cannot resolve an id to a
        // title; what it can do is stop pretending the id is content. The
        // short form stays useful to an operator matching a row in the console
        // and stops dominating the panel for everyone else.
        let short_ref = |id: &dyn std::fmt::Display| {
            let full = id.to_string();
            full.split('-').next().unwrap_or(&full).to_owned()
        };

        let truncate = |s: String, max: usize| {
            if s.len() > max {
                let mut truncated = s.chars().take(max.saturating_sub(1)).collect::<String>();
                truncated.push('…');
                truncated
            } else {
                s
            }
        };

        // A template key is a versioned contract name (`content.social_feed.v1`),
        // not prose — the crew reads the name and the version, not the
        // machine namespace. `content.` carries nothing the artifact label
        // does not already say, so it drops; every other segment stays.
        let friendly_template = |key: &str| {
            let key = key.strip_prefix("content.").unwrap_or(key);
            let mut version = "";
            let mut name_parts = Vec::new();
            for segment in key.split('.') {
                let is_version = segment
                    .strip_prefix('v')
                    .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
                if is_version {
                    version = segment;
                } else {
                    name_parts.extend(segment.split(['_', '-']));
                }
            }
            let name = name_parts.join(" ");
            let mut chars = name.chars();
            let name = match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => name,
            };
            if version.is_empty() { name } else { format!("{name} {version}") }
        };

        // `{:?}` on a domain enum is the variant name the compiler knows —
        // `PostShowThanks`, `ReleaseAssetsMissing` — one word where a person
        // reads three. The variant names are already the right words, they
        // only need their boundaries back.
        let friendly_enum = |value: &dyn std::fmt::Debug| {
            let debug = format!("{value:?}");
            let mut words = String::with_capacity(debug.len() + 4);
            for (i, ch) in debug.chars().enumerate() {
                if ch.is_uppercase() && i > 0 {
                    words.push(' ');
                    words.extend(ch.to_lowercase());
                } else {
                    words.push(ch);
                }
            }
            let mut chars = words.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => words,
            }
        };

        // The RFC3339 wire form is the worst date a person can be shown.
        // Values are not localized, so the format is the language-neutral one
        // everyone already reads: 2026-09-19 06:42 UTC.
        let friendly_datetime = |at: OffsetDateTime| {
            format!("{} {:02}:{:02} UTC", at.date(), at.hour(), at.minute())
        };

        match self {
            Self::ChangeTicketPrice { ticket_type_id, from_minor, to_minor } => ActionBriefing {
                summary: format!("Change ticket price: {} → {}", format_minor(*from_minor), format_minor(*to_minor)),
                why_it_matters: "Changing the ticket price affects both revenue and attendance. Someone may already have paid the old price, and this cannot be undone.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the new price and the ticket type".into(), why_it_matters: "Make sure the change is deliberate".into() },
                    BriefingStep { what_to_do: "Click APPROVE to apply it".into(), why_it_matters: "Once approved the price is live immediately".into() },
                ],
                content: vec![
                    BriefingField { label: "Ticket type".into(), value: short_ref(ticket_type_id) },
                    BriefingField { label: "Previous price".into(), value: format_minor(*from_minor) },
                    BriefingField { label: "New price".into(), value: format_minor(*to_minor) },
                ],
                deadline_note: String::new(),
            },
            Self::ChangeTicketCapacity { ticket_type_id, from_capacity, to_capacity, .. } => ActionBriefing {
                summary: format!("Change ticket capacity: {} → {}", from_capacity, to_capacity),
                why_it_matters: "Capacity decides how many tickets can be sold. Lowering it can invalidate seats that are already reserved.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the new capacity".into(), why_it_matters: "Make sure the venue can hold it".into() },
                    BriefingStep { what_to_do: "Click APPROVE to apply it".into(), why_it_matters: "Once approved the capacity is live".into() },
                ],
                content: vec![
                    BriefingField { label: "Ticket type".into(), value: short_ref(ticket_type_id) },
                    BriefingField { label: "Previous capacity".into(), value: from_capacity.to_string() },
                    BriefingField { label: "New capacity".into(), value: to_capacity.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestFanLifecycleMessage { fan_id, template_key } => ActionBriefing {
                summary: format!("Send a message to a fan: {}", friendly_template(template_key)),
                why_it_matters: "This goes to a fan who consented to be contacted. A sent message cannot be recalled.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the message body and its template".into(), why_it_matters: "Make sure the tone fits".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it".into(), why_it_matters: "The message is delivered to the fan".into() },
                ],
                content: vec![
                    BriefingField { label: "Fan".into(), value: short_ref(fan_id) },
                    BriefingField { label: "Template".into(), value: friendly_template(template_key) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestMerchReorder { variant_id, quantity } => ActionBriefing {
                summary: format!("Reorder merch: {} units", quantity),
                why_it_matters: "A merch order costs money and takes time to arrive. Confirm the stock is actually needed.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the quantity and the product variant".into(), why_it_matters: "Confirm the stock is genuinely short".into() },
                    BriefingStep { what_to_do: "Click APPROVE to place the order".into(), why_it_matters: "Once approved the order is placed".into() },
                ],
                content: vec![
                    BriefingField { label: "Variant".into(), value: short_ref(variant_id) },
                    BriefingField { label: "Quantity".into(), value: quantity.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::ChangeMerchPrice { product_id, from_minor, to_minor, .. } => ActionBriefing {
                summary: format!("Change merch price: {} → {}", format_minor(*from_minor), format_minor(*to_minor)),
                why_it_matters: "Merch price affects both margin and volume. Someone may already have paid the old price.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the new price".into(), why_it_matters: "Make sure the change is deliberate".into() },
                    BriefingStep { what_to_do: "Click APPROVE to apply it".into(), why_it_matters: "Once approved the price is live immediately".into() },
                ],
                content: vec![
                    BriefingField { label: "Product".into(), value: short_ref(product_id) },
                    BriefingField { label: "Previous price".into(), value: format_minor(*from_minor) },
                    BriefingField { label: "New price".into(), value: format_minor(*to_minor) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBookingOutreach { target_name, score, phase, .. } => ActionBriefing {
                summary: format!("Booking contact: {}", target_name),
                why_it_matters: "This is the first approach to a promoter. You get one chance at contact, so the message has to be right.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the target name and the contact phase".into(), why_it_matters: "Make sure this is the right promoter".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it".into(), why_it_matters: "Once approved the message is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Target".into(), value: target_name.clone() },
                    BriefingField { label: "Phase".into(), value: friendly_enum(phase) },
                    BriefingField { label: "Outcome".into(), value: format!("{}", score) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestAudienceCampaign { event_id, phase, template_key } => ActionBriefing {
                summary: format!("Audience campaign: {}", friendly_template(template_key)),
                why_it_matters: "The campaign reaches fans tied to this event. A sent campaign cannot be recalled.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the template and the campaign phase".into(), why_it_matters: "Make sure the content suits this phase".into() },
                    BriefingStep { what_to_do: "Click APPROVE to start it".into(), why_it_matters: "Once approved the campaign is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Event".into(), value: short_ref(event_id) },
                    BriefingField { label: "Phase".into(), value: friendly_enum(phase) },
                    BriefingField { label: "Template".into(), value: friendly_template(template_key) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestMerchBundle { product_a, product_b, bundle_price_minor, affinity_basis_points } => ActionBriefing {
                summary: format!("Create a merch bundle: {}", format_minor(*bundle_price_minor)),
                why_it_matters: "A bundle sells two products under one price. Confirm the affinity between them is strong enough.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the products and the bundle price".into(), why_it_matters: "Make sure the margin still works".into() },
                    BriefingStep { what_to_do: "Click APPROVE to create it".into(), why_it_matters: "Once approved the bundle goes on sale".into() },
                ],
                content: vec![
                    BriefingField { label: "Product A".into(), value: product_a.to_string() },
                    BriefingField { label: "Product B".into(), value: product_b.to_string() },
                    BriefingField { label: "Bundle price".into(), value: format_minor(*bundle_price_minor) },
                    BriefingField { label: "Affinity".into(), value: format!("{}%", affinity_basis_points / 100) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestOutreach { target_name, phase, template_key, .. } => ActionBriefing {
                summary: format!("Outreach contact: {}", target_name),
                why_it_matters: "This approaches an outside target. You get one chance at contact.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the target, the phase and the template".into(), why_it_matters: "Make sure the message is personal to them".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it".into(), why_it_matters: "Once approved the message is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Target".into(), value: target_name.clone() },
                    BriefingField { label: "Phase".into(), value: friendly_enum(phase) },
                    BriefingField { label: "Template".into(), value: friendly_template(template_key) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestRepresentationApproach { target_name, note, .. } => ActionBriefing {
                summary: format!("Approach for representation: {}", target_name),
                why_it_matters: "The platform sends this on the band's behalf — the agent never sees the address until they reply. The published listing is the pitch, and it costs one of the month's few approaches.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the listing is published and the opt-in basis is true".into(), why_it_matters: "Dispatch refuses without both".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it".into(), why_it_matters: "The approach goes out once, and cannot be recalled".into() },
                ],
                content: vec![
                    BriefingField { label: "Contact".into(), value: target_name.clone() },
                    BriefingField { label: "Note".into(), value: note.clone().unwrap_or_else(|| "—".into()) },
                ],
                deadline_note: String::new(),
            },
            Self::VerifyPlaylistPlacement { playlist_external_id, track_external_id, checkpoint, .. } => ActionBriefing {
                summary: format!("Verify the playlist (check {})", checkpoint),
                why_it_matters: "This checks whether the track is on the playlist. It reads public data and contacts nobody.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Click APPROVE to run the check".into(), why_it_matters: "The system reads the public playlist and checks for the track".into() },
                ],
                content: vec![
                    BriefingField { label: "Playlist ID".into(), value: playlist_external_id.clone() },
                    BriefingField { label: "Track ID".into(), value: track_external_id.clone() },
                    BriefingField { label: "Checkpoint".into(), value: checkpoint.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBeaconDiscovery { event_id, target_count } => ActionBriefing {
                summary: format!("Find {} local Beacons", target_count),
                why_it_matters: "The system searches local Beacons near the event. It reads data and contacts nobody.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Click APPROVE to run the search".into(), why_it_matters: "The system finds candidate Beacons for the event".into() },
                ],
                content: vec![
                    BriefingField { label: "Event".into(), value: short_ref(event_id) },
                    BriefingField { label: "Targets".into(), value: target_count.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBeaconInviteBatch { beacon_id, event_id, requested_count, .. } => ActionBriefing {
                summary: format!("Ask a Beacon for {} invite codes", requested_count),
                why_it_matters: "This asks a partner Beacon to hand out invite codes in their community. The codes are ours, so every signup stays attributed and consented.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the Beacon and the number of codes".into(), why_it_matters: "Make sure this is the right partner".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send the request".into(), why_it_matters: "Once approved the request goes to the Beacon".into() },
                ],
                content: vec![
                    BriefingField { label: "Beacon".into(), value: short_ref(beacon_id) },
                    BriefingField { label: "Event".into(), value: short_ref(event_id) },
                    BriefingField { label: "Codes".into(), value: requested_count.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestOutreachDiscovery { requested_candidates } => ActionBriefing {
                summary: format!("Find {} outreach candidates", requested_candidates),
                why_it_matters: "The system searches published sources for submission routes. It reads public data and contacts nobody.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Click APPROVE to run the search".into(), why_it_matters: "The system finds candidate outreach targets".into() },
                ],
                content: vec![
                    BriefingField { label: "Candidates".into(), value: requested_candidates.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBookingTargetDiscovery { requested_count } => ActionBriefing {
                summary: format!("Find {} booking targets", requested_count),
                why_it_matters: "The system searches published venue and promoter routes. It reads public data and contacts nobody.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Click APPROVE to run the search".into(), why_it_matters: "The system finds candidate booking targets".into() },
                ],
                content: vec![
                    BriefingField { label: "Targets".into(), value: requested_count.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBeaconOutreach { beacon_id, event_id, phase, template_key, .. } => ActionBriefing {
                summary: format!("Beacon contact: {}", friendly_template(template_key)),
                why_it_matters: "This approaches a Beacon about an event. You get one chance at contact.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the Beacon, the phase and the template".into(), why_it_matters: "Make sure the message fits".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it".into(), why_it_matters: "Once approved the message is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Beacon".into(), value: short_ref(beacon_id) },
                    BriefingField { label: "Event".into(), value: short_ref(event_id) },
                    BriefingField { label: "Phase".into(), value: friendly_enum(phase) },
                    BriefingField { label: "Template".into(), value: friendly_template(template_key) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestShowGrowth { event_id, lever, template_key, send_at } => ActionBriefing {
                summary: format!("Boost attendance: {}", lever.as_str()),
                why_it_matters: "This is an attendance push for a show. It may contact outside parties or message fans directly.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the lever and the template".into(), why_it_matters: "Make sure the action suits the event".into() },
                    BriefingStep { what_to_do: "Click APPROVE to start it".into(), why_it_matters: "Once approved the action is carried out".into() },
                ],
                content: vec![
                    BriefingField { label: "Event".into(), value: short_ref(event_id) },
                    BriefingField { label: "Lever".into(), value: lever.as_str().into() },
                    BriefingField { label: "Template".into(), value: friendly_template(template_key) },
                    BriefingField {
                        label: "Reaches fans".into(),
                        value: send_at.map_or_else(
                            || "on approval".to_owned(),
                            friendly_datetime,
                        ),
                    },
                ],
                deadline_note: String::new(),
            },
            Self::RequestContentArtifact { source_id, artifact, template_key, .. } => ActionBriefing {
                summary: format!("Content artifact: {}", artifact.label()),
                why_it_matters: "The system generates a content artefact — an image, a piece of copy — from a source. Internal only.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Click APPROVE to generate it".into(), why_it_matters: "The system builds an artefact from the named source".into() },
                ],
                content: vec![
                    BriefingField { label: "Source".into(), value: short_ref(source_id) },
                    BriefingField { label: "Artifact".into(), value: artifact.label().into() },
                    BriefingField { label: "Template".into(), value: friendly_template(template_key) },
                ],
                deadline_note: String::new(),
            },
            Self::AdjustExperiment { experiment_id, winner_variant_id, allocations, complete, .. } => ActionBriefing {
                summary: if *complete { "End the experiment and declare a winner".into() } else { "Adjust experiment allocation".into() },
                why_it_matters: "Allocation decides which variant fans see. Ending the experiment fixes the winner for good.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the winning variant and the allocations".into(), why_it_matters: "Make sure the decision follows the data".into() },
                    BriefingStep { what_to_do: "Click APPROVE to apply it".into(), why_it_matters: "Once approved the allocations change".into() },
                ],
                content: {
                    let mut fields = vec![
                        BriefingField { label: "Experiment".into(), value: short_ref(experiment_id) },
                        BriefingField { label: "Winning variant".into(), value: short_ref(winner_variant_id) },
                    ];
                    for alloc in allocations {
                        fields.push(BriefingField {
                            label: format!("Variant {}", alloc.variant_id),
                            value: format!("{}%", alloc.allocation_basis_points / 100),
                        });
                    }
                    fields.push(BriefingField { label: "Finish".into(), value: if *complete { "yes" } else { "no" }.into() });
                    fields
                },
                deadline_note: String::new(),
            },
            Self::CompleteShowTask { event_id, task } => ActionBriefing {
                summary: format!("Show task: {}", friendly_enum(task)),
                why_it_matters: "This is an operational task for a show. Marking it done closes that item on the checklist.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Confirm the task is actually done".into(), why_it_matters: "Tick this only if the work was actually done".into() },
                    BriefingStep { what_to_do: "Click APPROVE to close it out".into(), why_it_matters: "Once approved the task is marked complete".into() },
                ],
                content: vec![
                    BriefingField { label: "Event".into(), value: short_ref(event_id) },
                    BriefingField { label: "Task".into(), value: friendly_enum(task) },
                ],
                deadline_note: String::new(),
            },
            Self::EscalateShowTask { event_id, task } => ActionBriefing {
                summary: format!("Escalate the show task: {}", friendly_enum(task)),
                why_it_matters: "Escalating marks the task as needing urgent attention and raises its priority in the queue.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read why the task needs escalating".into(), why_it_matters: "Understand the problem before acting".into() },
                    BriefingStep { what_to_do: "Click APPROVE to escalate it".into(), why_it_matters: "Once approved the priority is raised".into() },
                ],
                content: vec![
                    BriefingField { label: "Event".into(), value: short_ref(event_id) },
                    BriefingField { label: "Task".into(), value: friendly_enum(task) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestPromotionBudgetChange { campaign_id, from_minor, to_minor, roas_basis_points } => ActionBriefing {
                summary: format!("Change promotion budget: {} → {}", format_minor(*from_minor), format_minor(*to_minor)),
                why_it_matters: "The budget decides ad spend. ROAS is the return that spend has produced so far.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the new budget against ROAS".into(), why_it_matters: "Make sure the change is justified".into() },
                    BriefingStep { what_to_do: "Click APPROVE to apply it".into(), why_it_matters: "Once approved the budget changes".into() },
                ],
                content: vec![
                    BriefingField { label: "Campaign".into(), value: short_ref(campaign_id) },
                    BriefingField { label: "Previous budget".into(), value: format_minor(*from_minor) },
                    BriefingField { label: "New budget".into(), value: format_minor(*to_minor) },
                    BriefingField { label: "ROAS".into(), value: format!("{}%", roas_basis_points / 100) },
                ],
                deadline_note: String::new(),
            },
            Self::ExecuteReleaseMilestone { title, release_at, milestone, .. } => ActionBriefing {
                summary: format!("Release milestone: {}", title),
                why_it_matters: "This is a milestone in the release plan. Completing it triggers the promotional actions scheduled behind it.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the title, the date and the milestone type".into(), why_it_matters: "Make sure everything is ready".into() },
                    BriefingStep { what_to_do: "Click APPROVE to carry it out".into(), why_it_matters: "Once approved the milestone is carried out".into() },
                ],
                content: vec![
                    BriefingField { label: "Title".into(), value: title.clone() },
                    BriefingField { label: "Release date".into(), value: release_at.date().to_string() },
                    BriefingField { label: "Milestone".into(), value: friendly_enum(milestone) },
                ],
                deadline_note: String::new(),
            },
            Self::EscalateEditorialPitch { title, due_at, .. } => ActionBriefing {
                summary: format!("Escalate the editorial pitch: {}", title),
                why_it_matters: "This is a reminder about an unsent Spotify Editorial pitch. A nudge inside the workspace; it contacts nobody outside.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the title and the deadline".into(), why_it_matters: "Understand what is overdue".into() },
                    BriefingStep { what_to_do: "Click APPROVE to escalate it".into(), why_it_matters: "Once approved the reminder is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Title".into(), value: title.clone() },
                    BriefingField { label: "Deadline".into(), value: friendly_datetime(*due_at) },
                ],
                deadline_note: String::new(),
            },
            Self::ApplyLiveOpportunity { opportunity_id, opportunity_kind, score } => ActionBriefing {
                summary: format!("Send a show application: {}", friendly_enum(opportunity_kind)),
                why_it_matters: "This applies to a show or festival. Applying commits the calendar.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the application type and the result".into(), why_it_matters: "Make sure this is the right opportunity".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send the application".into(), why_it_matters: "Once approved the application is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Opportunity".into(), value: short_ref(opportunity_id) },
                    BriefingField { label: "Type".into(), value: friendly_enum(opportunity_kind) },
                    BriefingField { label: "Outcome".into(), value: score.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::CounterLiveOpportunityTerms { opportunity_id, ask_minor, currency, round } => ActionBriefing {
                summary: format!("Counter the terms: {} {} (round {})", format_minor(*ask_minor), currency, round),
                why_it_matters: "This counters a promoter's fee. Sending it changes the terms under negotiation.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the amount and the currency".into(), why_it_matters: "Make sure the amount is acceptable".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send the counter-offer".into(), why_it_matters: "Once approved the counter-offer is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Opportunity".into(), value: short_ref(opportunity_id) },
                    BriefingField { label: "Amount".into(), value: format_minor(*ask_minor) },
                    BriefingField { label: "Currency".into(), value: currency.clone() },
                    BriefingField { label: "Round".into(), value: round.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::AcceptLiveOpportunityTerms { opportunity_id, fee_minor, currency } => ActionBriefing {
                summary: format!("Accept the terms: {} {}", format_minor(*fee_minor), currency),
                why_it_matters: "Accepting a fee commits both the calendar and the money. It cannot be undone.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the amount and the currency".into(), why_it_matters: "This is a commitment — make sure the terms are good".into() },
                    BriefingStep { what_to_do: "Click APPROVE to accept the terms".into(), why_it_matters: "Once approved the terms are binding".into() },
                ],
                content: vec![
                    BriefingField { label: "Opportunity".into(), value: short_ref(opportunity_id) },
                    BriefingField { label: "Fee".into(), value: format_minor(*fee_minor) },
                    BriefingField { label: "Currency".into(), value: currency.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::PrepareFundingPackage { opportunity_id } => ActionBriefing {
                summary: "Prepare the funding package".into(),
                why_it_matters: "This assembles the funding application documents. Internal only; it contacts nobody outside.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Click APPROVE to assemble the package".into(), why_it_matters: "The system assembles the required documents".into() },
                ],
                content: vec![
                    BriefingField { label: "Opportunity".into(), value: short_ref(opportunity_id) },
                ],
                deadline_note: String::new(),
            },
            Self::SubmitFundingApplication { opportunity_id } => ActionBriefing {
                summary: "Send the funding application".into(),
                why_it_matters: "Submitting the application is a formal commitment and cannot be withdrawn.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the application is complete".into(), why_it_matters: "Make sure every document is ready".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it".into(), why_it_matters: "Once approved the application is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Opportunity".into(), value: short_ref(opportunity_id) },
                ],
                deadline_note: String::new(),
            },
            Self::RaiseGrowthOpportunity { platform, metric_key, signal, recommended_action, deviation_basis_points, .. } => ActionBriefing {
                summary: format!("Growth opportunity: {} — {}", platform_label(platform), metric_key),
                why_it_matters: "An external metric moved. That is a signal something is happening and may deserve a response.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the recommended action".into(), why_it_matters: "Understand what the system proposes and why".into() },
                    BriefingStep { what_to_do: "Click APPROVE to schedule the action".into(), why_it_matters: "Once approved the action joins the queue".into() },
                ],
                content: vec![
                    BriefingField { label: "Platform".into(), value: platform_label(platform) },
                    BriefingField { label: "Metric".into(), value: metric_key.clone() },
                    BriefingField { label: "Signal".into(), value: friendly_enum(signal) },
                    BriefingField { label: "Deviation".into(), value: format!("{}%", deviation_basis_points / 100) },
                    BriefingField { label: "Recommended action".into(), value: recommended_action.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::RaiseDeclineAdvisory { subreddit, posts_considered, avg_score_tenths, window_days, alternative_label, alternative_detail, .. } => ActionBriefing {
                summary: format!("Stop engaging {subreddit} — it engages but produces zero fans"),
                why_it_matters: "Effort spent on a room that never converts is effort taken from one that might. Approving parks the community; disagreeing keeps it and tells the system it is wrong.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the evidence — the posts, their average score, and the zero fan conversions".into(), why_it_matters: "The claim is only as good as the numbers behind it".into() },
                    BriefingStep { what_to_do: "Click APPROVE to park the community, or CANCEL if the room is worth the effort anyway".into(), why_it_matters: "Approving stops the engager posting there; cancelling records your disagreement and the advisory will not be re-raised for a month".into() },
                ],
                content: vec![
                    BriefingField { label: "Community".into(), value: subreddit.clone() },
                    BriefingField { label: "Posts measured".into(), value: posts_considered.to_string() },
                    BriefingField { label: "Average score".into(), value: format!("{:.1}", *avg_score_tenths as f64 / 10.0) },
                    BriefingField { label: "Fan conversions".into(), value: "0".into() },
                    BriefingField { label: "Window".into(), value: format!("{window_days} days") },
                    BriefingField { label: "Alternative".into(), value: format!("{alternative_label} — {alternative_detail}") },
                ],
                deadline_note: String::new(),
            },
            Self::IssueReferralCode { fan_id } => ActionBriefing {
                summary: "Issue a referral code to a fan".into(),
                why_it_matters: "A referral code is growth that scales with the audience. The fan must have consented.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Click APPROVE to issue the code".into(), why_it_matters: "Once approved the fan receives their referral code".into() },
                ],
                content: vec![
                    BriefingField { label: "Fan".into(), value: short_ref(fan_id) },
                ],
                deadline_note: String::new(),
            },
            Self::RaiseGrowthDebt { debt_kind, recommended_action, overdue_basis_points, outstanding_items, tracked_items, .. } => ActionBriefing {
                summary: format!("Growth debt: {}", friendly_enum(debt_kind)),
                why_it_matters: "This is work that was committed to and never done. The longer it waits, the harder it is to catch up.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the recommended action".into(), why_it_matters: "Understand what is overdue, and why".into() },
                    BriefingStep { what_to_do: "Click APPROVE to schedule the catch-up".into(), why_it_matters: "Once approved the action joins the queue".into() },
                ],
                content: vec![
                    BriefingField { label: "Debt type".into(), value: friendly_enum(debt_kind) },
                    BriefingField { label: "Recommended action".into(), value: recommended_action.clone() },
                    BriefingField { label: "Overdue".into(), value: format!("{}%", overdue_basis_points / 100) },
                    BriefingField { label: "Overdue items".into(), value: format!("{} / {}", outstanding_items, tracked_items) },
                ],
                deadline_note: String::new(),
            },
            Self::RaiseContentSuggestion { format_key, concept, reason, distribution_promise, .. } => ActionBriefing {
                summary: format!("Make this: {}", concept),
                why_it_matters: "The content engine ranked this beat against everything else the band could make — it is the suggestion, not one of thirty.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the reason and who it reaches".into(), why_it_matters: "A suggestion with no real audience is not raised; check the promise is still true".into() },
                    BriefingStep { what_to_do: "Click APPROVE to commit to the beat".into(), why_it_matters: "Approved stays open until the band reports done, declined, or done differently — or the beat's day passes unreported".into() },
                ],
                content: {
                    let mut fields = vec![
                        BriefingField { label: "Concept".into(), value: concept.clone() },
                        BriefingField { label: "Why now".into(), value: reason.clone() },
                    ];
                    if let Some(key) = format_key {
                        fields.push(BriefingField { label: "Format".into(), value: key.clone() });
                    }
                    fields.push(BriefingField {
                        label: "Who sees it".into(),
                        value: promise_to_text(distribution_promise),
                    });
                    fields
                },
                deadline_note: String::new(),
            },
            Self::RaiseContentArc { title, summary: arc_summary, horizon_start, horizon_end, beats, .. } => ActionBriefing {
                summary: format!("Commit to the season: {}", title),
                why_it_matters: "The arc is the shape every beat for the next weeks serves. Approving it once replaces dozens of smaller asks — beats inside it still surface, but under a plan the band already chose.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the spine and the anchor it is built around".into(), why_it_matters: "The band approves the plan, not each step — this is the one creative decision".into() },
                    BriefingStep { what_to_do: "Click APPROVE to commit to the arc".into(), why_it_matters: "Beats in the spine then surface as ordinary suggestions under the same policy".into() },
                ],
                content: {
                    let mut fields = vec![
                        BriefingField { label: "Arc".into(), value: title.clone() },
                        BriefingField { label: "Plan".into(), value: arc_summary.clone() },
                        BriefingField { label: "Beats".into(), value: beats.to_string() },
                    ];
                    if let (Some(start), Some(end)) = (horizon_start, horizon_end) {
                        fields.push(BriefingField {
                            label: "Horizon".into(),
                            value: format!("{start} → {end}"),
                        });
                    }
                    fields
                },
                deadline_note: String::new(),
            },
            Self::RunPlayStep { play_id, play_kind, step_index, step_kind, event_id, fan_id, template_key } => ActionBriefing {
                summary: format!("Play step: {} (step {})", friendly_enum(play_kind), step_index),
                why_it_matters: "This is one step of a play campaign, for one fan. A sent message cannot be recalled.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the step type and the template".into(), why_it_matters: "Make sure the content suits this step".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it".into(), why_it_matters: "Once approved the step is carried out".into() },
                ],
                content: {
                    let mut fields = vec![
                        BriefingField { label: "Play".into(), value: short_ref(play_id) },
                        BriefingField { label: "Play type".into(), value: friendly_enum(play_kind) },
                        BriefingField { label: "Step".into(), value: format!("{}: {}", step_index, friendly_enum(step_kind)) },
                        BriefingField { label: "Template".into(), value: friendly_template(template_key) },
                    ];
                    if let Some(eid) = event_id {
                        fields.push(BriefingField { label: "Event".into(), value: short_ref(eid) });
                    }
                    if let Some(fid) = fan_id {
                        fields.push(BriefingField { label: "Fan".into(), value: short_ref(fid) });
                    }
                    fields
                },
                deadline_note: String::new(),
            },
            Self::SendTeamAssignmentEmail { task_title, task_detail, reminder_number, .. } => ActionBriefing {
                summary: if *reminder_number > 0 { format!("Reminder: {}", task_title) } else { task_title.clone() },
                why_it_matters: "This emails a task assignment to a crew member. Reminders keep going until the task is closed.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the task body".into(), why_it_matters: "Make sure the task is unambiguous".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send the email".into(), why_it_matters: "Once approved the email is sent".into() },
                ],
                content: vec![
                    BriefingField { label: "Task title".into(), value: task_title.clone() },
                    BriefingField { label: "Details".into(), value: truncate(task_detail.clone(), 2000) },
                    BriefingField { label: "Reminder".into(), value: reminder_number.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestAgentContent {
                template_id,
                task_id,
                draft,
                recipient_email,
                recipient_name,
                ..
            } => ActionBriefing {
                summary: match template_id.as_deref() {
                    Some("press-pitch") | Some("press_pitch") => "Approve the agent's press pitch".into(),
                    Some("social-post") | Some("social_post") => "Approve the agent's social post".into(),
                    Some(tid) => format!("Approve the agent's content draft ({tid})"),
                    None => "Approve the agent's content draft".into(),
                },
                why_it_matters: "The agent wrote this draft from intelligence the system gathered. Approving publishes it to the channel it was written for.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the draft below".into(), why_it_matters: "Check tone, facts, and that it sounds like the brand".into() },
                    BriefingStep { what_to_do: "Click APPROVE if the draft is good, REJECT if it needs work".into(), why_it_matters: "Once approved the content is published, and cannot be unpublished".into() },
                ],
                content: {
                    let mut fields = Vec::new();
                    if let Some(tid) = template_id {
                        fields.push(BriefingField { label: "Template".into(), value: tid.clone() });
                    }
                    fields.push(BriefingField { label: "Task".into(), value: short_ref(task_id) });
                    // Approving a pitch without seeing the recipient is
                    // approving half the decision.
                    if let Some(name) = recipient_name {
                        fields.push(BriefingField { label: "Recipient".into(), value: name.clone() });
                    }
                    if let Some(email) = recipient_email {
                        fields.push(BriefingField { label: "Address".into(), value: email.clone() });
                    }
                    // Extract channel/destination from draft if present so the
                    // operator knows where the content will be published.
                    if let Some(obj) = draft.as_object() {
                        if let Some(platform) = obj.get("platform").and_then(|v| v.as_str()) {
                            fields.push(BriefingField { label: "Channel".into(), value: platform.to_owned() });
                        }
                        if let Some(subject) = obj.get("subject").and_then(|v| v.as_str()) {
                            fields.push(BriefingField { label: "Subject".into(), value: subject.to_owned() });
                        }
                    }
                    fields.push(BriefingField { label: "Draft".into(), value: truncate(draft_to_text(draft), 2000) });
                    fields
                },
                deadline_note: String::new(),
            },
            Self::RequestOutreachTarget { target_kind, display_name, contact_email, contact_domain, why_fit, evidence_urls, subreddit, .. } => ActionBriefing {
                summary: format!("Approve an outreach target: {}", display_name),
                why_it_matters: format!("The agent discovered this target (type: {}). Approving promotes it, and the growth loop may then use it in campaigns.", target_kind),
                steps: vec![
                    BriefingStep { what_to_do: "Check the contact details and the rationale".into(), why_it_matters: "Make sure the target is real and fits the brand".into() },
                    BriefingStep { what_to_do: "Click APPROVE to promote the target, REJECT if it does not fit".into(), why_it_matters: "Once approved the growth loop may use this target in campaigns".into() },
                ],
                content: {
                    let mut fields = vec![
                        BriefingField { label: "Target type".into(), value: target_kind.clone() },
                        BriefingField { label: "Name".into(), value: display_name.clone() },
                    ];
                    if let Some(email) = contact_email {
                        fields.push(BriefingField { label: "Contact email".into(), value: email.clone() });
                    }
                    if let Some(domain) = contact_domain {
                        fields.push(BriefingField { label: "Domain".into(), value: domain.clone() });
                    }
                    if let Some(sub) = subreddit {
                        fields.push(BriefingField { label: "Subreddit".into(), value: sub.clone() });
                    }
                    if !why_fit.is_empty() {
                        fields.push(BriefingField { label: "Why it fits".into(), value: truncate(why_fit.clone(), 500) });
                    }
                    if let Some(urls) = evidence_urls.as_array() {
                        let url_list = urls.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ");
                        if !url_list.is_empty() {
                            fields.push(BriefingField { label: "Evidence".into(), value: truncate(url_list, 500) });
                        }
                    }
                    fields
                },
                deadline_note: String::new(),
            },
            Self::RequestAgentRun { template_id, prompt, priority, tier } => ActionBriefing {
                summary: format!("Run the agent: {}", template_id),
                why_it_matters: "The deterministic brain dispatches an LLM worker to gather intelligence or draft content. The agent decides nothing — it only supplies material.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Check the template and the priority".into(), why_it_matters: "Make sure the task makes sense".into() },
                    BriefingStep { what_to_do: "Click APPROVE to dispatch the agent".into(), why_it_matters: "Once approved the agent runs and gathers intelligence".into() },
                ],
                content: vec![
                    BriefingField { label: "Template".into(), value: template_id.clone() },
                    BriefingField { label: "Priority".into(), value: priority.to_string() },
                    BriefingField { label: "Tier".into(), value: friendly_enum(tier) },
                    BriefingField { label: "Prompt".into(), value: truncate(prompt.clone(), 2000) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestCommunityEngagement { platform, subreddit, title, body, smart_link, .. } => ActionBriefing {
                summary: format!("Social post: {} — {}", platform, title),
                why_it_matters: "This posts to an outside platform such as Reddit. It cannot be unposted, and it lands in someone else's community.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the post title and body".into(), why_it_matters: "Check tone, facts, and that it meets the platform's rules".into() },
                    BriefingStep { what_to_do: "Click APPROVE to publish it".into(), why_it_matters: "Once approved the post goes live on the platform".into() },
                ],
                content: vec![
                    BriefingField { label: "Platform".into(), value: platform.clone() },
                    BriefingField { label: "Subreddit".into(), value: subreddit.clone().unwrap_or("—".into()) },
                    BriefingField { label: "Title".into(), value: title.clone() },
                    BriefingField { label: "Body".into(), value: truncate(body.clone(), 2000) },
                    BriefingField { label: "Smart link".into(), value: smart_link.clone().unwrap_or("—".into()) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestSignalPush { title, body, target_path, event_id, segment, .. } => ActionBriefing {
                summary: format!("Push notification: {}", title),
                why_it_matters: "The push reaches fans who consented to notifications. A sent push cannot be recalled.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Read the notification title and body".into(), why_it_matters: "A sent push cannot be recalled — read it closely".into() },
                    BriefingStep { what_to_do: "Click APPROVE to send it to the segment".into(), why_it_matters: "Once approved the push goes to the chosen fan segment".into() },
                ],
                content: vec![
                    BriefingField { label: "Title".into(), value: title.clone() },
                    BriefingField { label: "Body".into(), value: truncate(body.clone(), 2000) },
                    BriefingField { label: "Link".into(), value: target_path.clone().unwrap_or("—".into()) },
                    BriefingField { label: "Segment".into(), value: segment.clone().unwrap_or("everyone".into()) },
                    BriefingField { label: "Event".into(), value: event_id.map(|id| id.to_string()).unwrap_or("—".into()) },
                ],
                deadline_note: String::new(),
            },
        }
    }
}
