// The `RequestAgentContent` briefing — `include!`d as the arm body in
// `briefing.rs`, so the pattern's bindings resolve here. Split for the
// source-size ratchet: `briefing.rs` caps at 1200 lines.
ActionBriefing {
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
}
