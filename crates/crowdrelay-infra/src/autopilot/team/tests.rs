#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_email_frame_follows_the_crew_locale() {
        let (subject, greeting, intro) = team_email_frame(
            BriefingLocale::Pl,
            "Wojtek",
            "Zatwierdź artefakt treści",
            0,
            false,
        );
        assert_eq!(subject, "Zadanie: Zatwierdź artefakt treści");
        assert_eq!(greeting, "Cześć Wojtek!");
        assert!(intro.contains("nowe zadanie"));

        let (subject, greeting, _) = team_email_frame(
            BriefingLocale::En,
            "Wojtek",
            "Approve the content artifact",
            0,
            false,
        );
        assert_eq!(subject, "Task: Approve the content artifact");
        assert_eq!(greeting, "Hi Wojtek!");
    }

    /// A mail with nothing to act on is not "a new task" — the subject is the
    /// title itself, which already says briefing/summary/notice.
    #[test]
    fn an_informational_mail_speaks_for_itself() {
        let (subject, _, intro) = team_email_frame(
            BriefingLocale::Pl,
            "Wojtek",
            "CrowdRelay — poranne podsumowanie",
            0,
            true,
        );
        assert_eq!(subject, "CrowdRelay — poranne podsumowanie");
        assert!(intro.contains("powiadomienie"), "{intro}");
        assert!(!intro.contains("zadanie"), "{intro}");

        let (subject, _, intro) = team_email_frame(
            BriefingLocale::En,
            "Wojtek",
            "CrowdRelay — morning briefing",
            0,
            true,
        );
        assert_eq!(subject, "CrowdRelay — morning briefing");
        assert!(intro.contains("notice"), "{intro}");
    }

    /// The recipient is told when nobody will chase again. The reminder lane
    /// itself is retired — nothing schedules `next_reminder_at` — but the
    /// frame still renders a nonzero `reminder_number` for rows queued before
    /// the retirement or by a rolled-back build.
    #[test]
    fn the_last_reminder_says_it_is_the_last() {
        let (subject, _, intro) = team_email_frame(
            BriefingLocale::En,
            "Wojtek",
            "Approve the content artifact",
            MAX_REMINDERS_PER_ASSIGNMENT as u8,
            false,
        );
        assert!(subject.starts_with("Last reminder:"), "{subject}");
        assert!(intro.contains("no more will follow"), "{intro}");

        let (subject, _, _) =
            team_email_frame(BriefingLocale::En, "Wojtek", "Approve it", 1, false);
        assert!(subject.starts_with("Reminder:"), "{subject}");
    }

    /// The inbox this was reported from: four approvals of one kind, four
    /// emails, same minute, byte-identical subjects. One email has to name the
    /// others or the recipient cannot tell four tasks from one task sent four
    /// times.
    #[test]
    fn the_digest_names_what_else_is_waiting() {
        let tail = digest_tail(
            &[
                "Zatwierdź artefakt treści".to_owned(),
                "Zatwierdź cel outreach".to_owned(),
            ],
            BriefingLocale::Pl,
        );
        assert!(tail.contains("jeszcze 2 zadania"), "{tail}");
        assert!(tail.contains("• Zatwierdź artefakt treści"), "{tail}");
        assert!(tail.contains("• Zatwierdź cel outreach"), "{tail}");

        let one = digest_tail(
            &["Approve the content artifact".to_owned()],
            BriefingLocale::En,
        );
        assert!(one.contains("One more task is waiting"), "{one}");
    }

    /// Nothing else waiting means nothing appended. A digest tail on a single
    /// task would be a sentence about an empty list.
    #[test]
    fn a_lone_task_gets_no_digest_tail() {
        assert_eq!(digest_tail(&[], BriefingLocale::En), "");
        assert_eq!(digest_tail(&[], BriefingLocale::Pl), "");
    }

    #[test]
    fn a_reminder_frame_is_not_the_first_send() {
        let (subject, _, intro) =
            team_email_frame(BriefingLocale::Pl, "Wojtek", "Domknij zadanie", 2, false);
        assert!(subject.starts_with("Przypomnienie:"));
        assert!(intro.contains("przypominamy"));
    }
}
