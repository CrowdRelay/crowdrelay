// The words one reminder puts in front of a crew member.
//
// Included into `team.rs` rather than compiled as its own module: split out to
// keep that file under the source-size ratchet, and because it is a different
// job from the sweep — the sweep decides who is reminded and when, this decides
// what they read.

/// The reminder's one-line title, in the crew's language.
///
/// Extracted from the sweep so the sweep can compose a digest: it now needs
/// every due assignment's title before it decides how many emails to send, not
/// one title per email it was already committed to sending.
fn reminder_title(row: &ReminderRow, crew_locale: BriefingLocale) -> String {
    if row.source_kind == "show_task" {
        friendly_show_task_title(
            row.source_ref.as_deref().unwrap_or("show_task"),
            crew_locale,
        )
    } else if row.source_kind == "release_making_of" {
        match (row.release_title.as_deref(), crew_locale) {
            (Some(plan_title), BriefingLocale::Pl) => {
                format!("Making-of do wydania: {plan_title}")
            }
            (Some(plan_title), BriefingLocale::En) => {
                format!("Making-of for the release: {plan_title}")
            }
            (None, BriefingLocale::Pl) => "Making-of do wydania".to_owned(),
            (None, BriefingLocale::En) => "Making-of for the release".to_owned(),
        }
    } else if row.source_kind == "capture_plan" {
        match (row.plan_title.as_deref(), crew_locale) {
            (Some(plan_title), BriefingLocale::Pl) => {
                format!("Zabezpiecz materiał: {plan_title}")
            }
            (Some(plan_title), BriefingLocale::En) => {
                format!("Secure the footage: {plan_title}")
            }
            (None, BriefingLocale::Pl) => "Zabezpiecz materiał".to_owned(),
            (None, BriefingLocale::En) => "Secure the footage".to_owned(),
        }
    } else {
        friendly_action_title(
            row.action_kind.as_deref().unwrap_or("approval"),
            crew_locale,
        )
    }
}

/// The reminder's body, in the crew's language. Extracted for the same reason
/// as [`reminder_title`].
fn reminder_detail(row: &ReminderRow, crew_locale: BriefingLocale) -> String {
    if row.source_kind == "show_task" {
        match (row.event_title.as_deref(), crew_locale) {
            (Some(event_title), BriefingLocale::Pl) => {
                format!("To zadanie dotyczące koncertu {event_title} nadal czeka na domknięcie.")
            }
            (Some(event_title), BriefingLocale::En) => {
                format!("The task for the {event_title} show is still waiting to be closed.")
            }
            (None, BriefingLocale::Pl) => "To zadanie nadal czeka na Twoje domknięcie.".to_owned(),
            (None, BriefingLocale::En) => {
                "This task is still waiting for you to close it.".to_owned()
            }
        }
    } else if row.source_kind == "release_making_of" {
        match (row.release_title.as_deref(), crew_locale) {
            (Some(plan_title), BriefingLocale::Pl) => format!(
                "Premiera „{plan_title}” zbliża się — materiał making-of nadal czeka na zarchiwizowanie i oznaczenie."
            ),
            (Some(plan_title), BriefingLocale::En) => format!(
                "\"{plan_title}\" is still inside its making-of window — the material is waiting to be filed and marked."
            ),
            (None, BriefingLocale::Pl) => {
                "Materiał making-of nadal czeka na zarchiwizowanie.".to_owned()
            }
            (None, BriefingLocale::En) => {
                "The making-of material is still waiting to be filed.".to_owned()
            }
        }
    } else if row.source_kind == "capture_plan" {
        // The reminder re-lists the shots — the member should not
        // have to dig the first email out of their inbox.
        let items = row
            .plan_items
            .as_ref()
            .and_then(|items| items.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|entry| entry["item"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        match (
            row.plan_title.as_deref(),
            row.plan_scheduled_for,
            crew_locale,
        ) {
            (Some(plan_title), Some(scheduled_for), _) => {
                super::capture_plans::capture_plan_detail(
                    plan_title,
                    scheduled_for,
                    &items,
                    crew_locale,
                )
            }
            (_, _, BriefingLocale::Pl) => "Lista ujęć nadal czeka na wykonanie.".to_owned(),
            (_, _, BriefingLocale::En) => "The shot list is still waiting to be done.".to_owned(),
        }
    } else if let Some(payload_json) = row.payload.as_ref() {
        enriched_task_detail(payload_json, row.due_at, row.due_at, crew_locale)
    } else {
        match crew_locale {
            BriefingLocale::Pl => {
                "To zadanie nadal czeka na Twoją decyzję lub wykonanie.".to_owned()
            }
            BriefingLocale::En => {
                "This task is still waiting for your decision or action.".to_owned()
            }
        }
    }
}

/// The line that tells a recipient what else of theirs is waiting.
///
/// The digest is the fix for what the inbox actually looked like: four
/// approvals of the same kind produced four emails in the same minute with
/// byte-identical subjects, because the subject comes from the action kind and
/// nothing else. A person cannot tell those apart, cannot tell whether they are
/// duplicates of one task, and learns within a day to ignore all of them. One
/// email that names the others is the same information and one interruption.
fn digest_tail(others: &[String], locale: BriefingLocale) -> String {
    if others.is_empty() {
        return String::new();
    }
    let header = match (locale, others.len()) {
        (BriefingLocale::Pl, 1) => "Czeka na Ciebie jeszcze jedno zadanie:".to_owned(),
        (BriefingLocale::Pl, n) => format!("Czekają na Ciebie jeszcze {n} zadania:"),
        (BriefingLocale::En, 1) => "One more task is waiting for you:".to_owned(),
        (BriefingLocale::En, n) => format!("{n} more tasks are waiting for you:"),
    };
    let mut tail = format!("\n\n{header}");
    for other in others {
        tail.push_str(&format!("\n• {other}"));
    }
    let closing = match locale {
        BriefingLocale::Pl => "\n\nWszystkie są w panelu operacyjnym pod tym samym linkiem.",
        BriefingLocale::En => "\n\nAll of them are in the operations panel behind the same link.",
    };
    tail.push_str(closing);
    tail
}

