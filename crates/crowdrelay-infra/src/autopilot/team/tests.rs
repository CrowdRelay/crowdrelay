#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_email_frame_follows_the_crew_locale() {
        let (subject, greeting, intro) =
            team_email_frame(BriefingLocale::Pl, "VIRYA", "Wojtek", "Zatwierdź artefakt treści", 0);
        assert_eq!(subject, "VIRYA — nowe zadanie: Zatwierdź artefakt treści");
        assert_eq!(greeting, "Cześć Wojtek!");
        assert!(intro.contains("nowe zadanie"));

        let (subject, greeting, _) = team_email_frame(
            BriefingLocale::En,
            "VIRYA",
            "Wojtek",
            "Approve the content artifact",
            0,
        );
        assert_eq!(subject, "VIRYA — new task: Approve the content artifact");
        assert_eq!(greeting, "Hi Wojtek!");
    }

    /// The recipient is told when nobody will chase again. The reminder lane
    /// itself is retired — nothing schedules `next_reminder_at` — but the
    /// frame still renders a nonzero `reminder_number` for rows queued before
    /// the retirement or by a rolled-back build.
    #[test]
    fn the_last_reminder_says_it_is_the_last() {
        let (subject, _, intro) = team_email_frame(
            BriefingLocale::En,
            "VIRYA",
            "Wojtek",
            "Approve the content artifact",
            MAX_REMINDERS_PER_ASSIGNMENT as u8,
        );
        assert!(subject.starts_with("VIRYA — last reminder:"), "{subject}");
        assert!(intro.contains("no more will follow"), "{intro}");

        let (subject, _, _) = team_email_frame(BriefingLocale::En, "VIRYA", "Wojtek", "Approve it", 1);
        assert!(subject.starts_with("VIRYA — reminder:"), "{subject}");
    }

    /// A roster's crew serves several acts; each task says whose it is.
    #[test]
    fn a_task_names_the_act_it_belongs_to() {
        let (subject, _, _) = team_email_frame(BriefingLocale::Pl, "Mgła", "Ola", "Zatwierdź post", 0);
        assert_eq!(subject, "Mgła — nowe zadanie: Zatwierdź post");
        assert!(!subject.contains("VIRYA"), "{subject}");
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
            team_email_frame(BriefingLocale::Pl, "VIRYA", "Wojtek", "Domknij zadanie", 2);
        assert!(subject.starts_with("VIRYA — przypomnienie:"));
        assert!(intro.contains("przypominamy"));
    }
}
