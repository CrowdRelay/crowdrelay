// The `RequestSourceCampaign` briefing — `include!`d as the arm body in
// `briefing.rs`, so the pattern's bindings resolve here. Split for the
// source-size ratchet: `briefing.rs` caps at 1200 lines.
ActionBriefing {
    summary: match audience_size {
        Some(size) => format!(
            "Drop campaign to {size} fans: {}",
            friendly_template(template_key)
        ),
        None => format!("Drop campaign: {}", friendly_template(template_key)),
    },
    why_it_matters: "The campaign reaches fans who consented to email, announcing the drop. A sent campaign cannot be recalled.".into(),
    steps: vec![
        BriefingStep { what_to_do: "Check who this reaches and that the source is the right drop".into(), why_it_matters: "The size and the basis are the part a template key hides".into() },
        BriefingStep { what_to_do: "Click APPROVE to start it".into(), why_it_matters: "Once approved the campaign is sent".into() },
    ],
    content: vec![
        BriefingField { label: "Source".into(), value: short_ref(source_id) },
        BriefingField { label: "Template".into(), value: friendly_template(template_key) },
        BriefingField {
            label: "Reaches".into(),
            value: match audience_size {
                Some(size) => format!("{size} fans"),
                None => "counted when the campaign is built".to_owned(),
            },
        },
        BriefingField { label: "Who they are".into(), value: audience_basis.clone() },
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
