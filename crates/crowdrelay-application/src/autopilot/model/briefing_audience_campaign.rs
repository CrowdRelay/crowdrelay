// The `RequestAudienceCampaign` briefing — `include!`d as the arm body in
// `briefing.rs`, so the pattern's bindings resolve here. Split for the
// source-size ratchet: `briefing.rs` caps at 1200 lines.
ActionBriefing {
    summary: match audience_size {
        Some(size) => format!(
            "Audience campaign to {size} fans: {}",
            friendly_template(template_key)
        ),
        None => format!("Audience campaign: {}", friendly_template(template_key)),
    },
    why_it_matters: "The campaign reaches fans tied to this event. A sent campaign cannot be recalled.".into(),
    steps: vec![
        BriefingStep { what_to_do: "Check who this reaches and the campaign phase".into(), why_it_matters: "The size and the basis are the part a template key hides".into() },
        BriefingStep { what_to_do: "Click APPROVE to start it".into(), why_it_matters: "Once approved the campaign is sent".into() },
    ],
    content: vec![
        BriefingField { label: "Event".into(), value: short_ref(event_id) },
        BriefingField { label: "Phase".into(), value: friendly_enum(phase) },
        BriefingField { label: "Template".into(), value: friendly_template(template_key) },
        // Absent is said out loud rather than printed as a zero:
        // the announcement's audience is counted when the campaign
        // is built, and "0 fans" would read as "nobody".
        BriefingField {
            label: "Reaches".into(),
            value: match audience_size {
                Some(size) => format!("{size} fans"),
                None => "counted when the campaign is built".to_owned(),
            },
        },
        BriefingField { label: "Who they are".into(), value: audience_basis.clone() },
        // O.3: the approval shows the words the mailer sends,
        // not a template key that resolves outside the repo.
        BriefingField {
            label: "Subject".into(),
            value: match draft.subject.trim().is_empty() {
                true => "not composed".to_owned(),
                false => draft.subject.clone(),
            },
        },
        BriefingField {
            label: "Body".into(),
            value: match draft.body.trim().is_empty() {
                true => "not composed".to_owned(),
                false => truncate(draft.body.clone(), 2000),
            },
        },
    ],
    deadline_note: String::new(),
}
