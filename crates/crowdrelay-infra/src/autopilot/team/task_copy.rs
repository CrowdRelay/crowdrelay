// The words wrapped around one crew task — the subject, the greeting, the
// intro, and the localised detail the briefing renders into. Split out of
// `team.rs` to keep that file under the source-size ratchet, and included
// rather than declared as a module for the same reason
// `team/reminder_copy.rs` is: these read the file's own private types.

/// The words the n8n workflow wraps around `task_title`/`task_detail` — the
/// subject line, the greeting and the intro sentence.
///
/// They used to be hardcoded Polish inside the workflow, so a crew member on
/// the English locale read a Polish frame around English content — the same
/// seam the briefing overlay exists to remove. Composing them here keeps the
/// whole email on one locale resolved in one place: the workflow only has to
/// interpolate `email_subject`, `email_greeting` and `email_intro`.
pub(super) fn team_email_frame(
    locale: BriefingLocale,
    recipient_name: &str,
    task_title: &str,
    reminder_number: u8,
) -> (String, String, String) {
    let reminder = reminder_number > 0;
    // The last rung of the ladder. Saying so is the difference between a
    // reminder and a nag: the recipient learns that nothing further will
    // arrive, so ignoring it is a decision rather than a delay.
    let final_reminder = reminder && i32::from(reminder_number) >= MAX_REMINDERS_PER_ASSIGNMENT;
    let subject = match (locale, reminder, final_reminder) {
        (BriefingLocale::Pl, false, _) => format!("VIRYA — nowe zadanie: {task_title}"),
        (BriefingLocale::Pl, true, false) => format!("VIRYA — przypomnienie: {task_title}"),
        (BriefingLocale::Pl, true, true) => {
            format!("VIRYA — ostatnie przypomnienie: {task_title}")
        }
        (BriefingLocale::En, false, _) => format!("VIRYA — new task: {task_title}"),
        (BriefingLocale::En, true, false) => format!("VIRYA — reminder: {task_title}"),
        (BriefingLocale::En, true, true) => format!("VIRYA — last reminder: {task_title}"),
    };
    let greeting = match locale {
        BriefingLocale::Pl => format!("Cześć {recipient_name}!"),
        BriefingLocale::En => format!("Hi {recipient_name}!"),
    };
    let intro = match (locale, reminder, final_reminder) {
        (BriefingLocale::Pl, false, _) => "Wpadło do Ciebie nowe zadanie od CrowdRelay.".to_owned(),
        (BriefingLocale::Pl, true, false) => {
            "To zadanie nadal czeka na Ciebie — przypominamy.".to_owned()
        }
        (BriefingLocale::Pl, true, true) => {
            "To ostatnie przypomnienie o tym zadaniu — kolejnych nie wyślemy. \
             Jeśli nie jest już potrzebne, zamknij je w panelu."
                .to_owned()
        }
        (BriefingLocale::En, false, _) => "A new task from CrowdRelay landed for you.".to_owned(),
        (BriefingLocale::En, true, false) => "This task is still waiting for you.".to_owned(),
        (BriefingLocale::En, true, true) => {
            "This is the last reminder for this task — no more will follow. \
             If it is no longer needed, close it in the panel."
                .to_owned()
        }
    };
    (subject, greeting, intro)
}

/// The words around the briefing, in the crew's language.
///
/// The frame used to be Polish while the briefing inside it was English, so a
/// crew member read "Dlaczego to ważne:" followed by an English sentence.
/// Localising only one of the two moves the seam rather than removing it, so
/// the frame and its contents resolve from the same locale.
struct DetailFrame {
    why: &'static str,
    steps: &'static str,
    content: &'static str,
    unreadable: &'static str,
}

const fn detail_frame(locale: BriefingLocale) -> DetailFrame {
    match locale {
        BriefingLocale::Pl => DetailFrame {
            why: "Dlaczego to ważne",
            steps: "Kroki",
            content: "Treść",
            unreadable: "Nie udało się odczytać szczegółów zadania. Otwórz panel operacyjny, aby zobaczyć pełne dane.",
        },
        BriefingLocale::En => DetailFrame {
            why: "Why this matters",
            steps: "Steps",
            content: "Details",
            unreadable: "This task's details could not be read. Open the operations panel to see the full record.",
        },
    }
}

fn enriched_task_detail(
    payload_json: &serde_json::Value,
    approval_expires_at: Option<OffsetDateTime>,
    assignment_due_at: Option<OffsetDateTime>,
    locale: BriefingLocale,
) -> String {
    use crowdrelay_application::autopilot::AutopilotActionPayload;

    let frame = detail_frame(locale);
    let Ok(payload) = serde_json::from_value::<AutopilotActionPayload>(payload_json.clone()) else {
        return frame.unreadable.to_owned();
    };
    let mut briefing = payload.briefing().localized(locale);
    briefing.deadline_note = format_deadline_note(approval_expires_at, assignment_due_at, locale);

    let mut text = format!(
        "{}\n\n{}: {}\n\n{}:",
        briefing.summary, frame.why, briefing.why_it_matters, frame.steps
    );
    for (i, step) in briefing.steps.iter().enumerate() {
        text.push_str(&format!(
            "\n{}. {} — {}",
            i + 1,
            step.what_to_do,
            step.why_it_matters
        ));
    }
    if !briefing.content.is_empty() {
        text.push_str(&format!("\n\n{}:", frame.content));
        for field in &briefing.content {
            text.push_str(&format!("\n{}: {}", field.label, field.value));
        }
    }
    text.push_str(&format!("\n\n{}", briefing.deadline_note));

    // Truncate to fit the n8n workflow's slice(0, 1800) limit — byte 1799 can
    // sit mid-character in Polish copy or a post body, and `truncate` panics
    // rather than rounding down. The ellipsis's own three bytes come out of
    // the same budget, or the result is 1803 and the workflow cuts it anyway.
    if text.len() > 1800 {
        text.truncate(text.floor_char_boundary(1797));
        text.push('…');
    }
    text
}

