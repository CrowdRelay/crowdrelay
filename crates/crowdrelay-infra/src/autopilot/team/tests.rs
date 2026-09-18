#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_email_frame_follows_the_crew_locale() {
        let (subject, greeting, intro) =
            team_email_frame(BriefingLocale::Pl, "Wojtek", "Zatwierdź artefakt treści", 0);
        assert_eq!(subject, "VIRYA — nowe zadanie: Zatwierdź artefakt treści");
        assert_eq!(greeting, "Cześć Wojtek!");
        assert!(intro.contains("nowe zadanie"));

        let (subject, greeting, _) = team_email_frame(
            BriefingLocale::En,
            "Wojtek",
            "Approve the content artifact",
            0,
        );
        assert_eq!(subject, "VIRYA — new task: Approve the content artifact");
        assert_eq!(greeting, "Hi Wojtek!");
    }

    /// The measurement that made this a bug: the old rule spaced reminders
    /// from the last send — 24 hours, then 12, then every 6 — so a task with a
    /// week's runway produced somewhere near thirty identical emails. Four is
    /// the ceiling now: the first email plus three rungs.
    #[test]
    fn a_weeks_runway_produces_four_emails_not_thirty() {
        let now = datetime!(2026-09-18 09:00 UTC);
        let due = now + TimeDuration::days(7);
        let mut sends = 1; // the original email
        let mut count = 0;
        let mut at = first_reminder_at(now, Some(due)).expect("a nudge is scheduled");
        while count < 20 {
            sends += 1;
            count += 1;
            match next_reminder_at(at, Some(due), count) {
                Some(next) => at = next,
                None => break,
            }
        }
        assert_eq!(sends, 4, "one email and three reminders, no more");
    }

    /// Anchored to the deadline, not to the last send. Each rung has to land
    /// where it changes what the recipient would do.
    #[test]
    fn reminders_land_at_fixed_distances_from_the_deadline() {
        let now = datetime!(2026-09-18 09:00 UTC);
        let due = datetime!(2026-09-25 18:00 UTC);
        assert_eq!(
            next_reminder_at(now, Some(due), 0),
            Some(due - TimeDuration::hours(48))
        );
        assert_eq!(
            next_reminder_at(now, Some(due), 1),
            Some(due - TimeDuration::hours(24))
        );
        assert_eq!(
            next_reminder_at(now, Some(due), 2),
            Some(due - TimeDuration::hours(6))
        );
    }

    /// The ceiling is the point. A fourth reminder has never been the thing
    /// that made somebody do a task.
    #[test]
    fn the_ladder_stops_after_three_reminders() {
        let now = datetime!(2026-09-18 09:00 UTC);
        let due = now + TimeDuration::days(30);
        assert_eq!(next_reminder_at(now, Some(due), 3), None);
        assert_eq!(next_reminder_at(now, Some(due), 9), None);
    }

    /// A rung already behind us is skipped rather than fired immediately. A
    /// task assigned thirty hours before it is due never gets a "two days
    /// left" reminder, because there are not two days left.
    #[test]
    fn a_rung_in_the_past_is_skipped_not_sent_now() {
        let now = datetime!(2026-09-18 09:00 UTC);
        let due = now + TimeDuration::hours(30);
        assert_eq!(
            next_reminder_at(now, Some(due), 0),
            Some(due - TimeDuration::hours(24)),
            "the 48-hour rung is in the past and must not fire at once"
        );
    }

    /// Nothing to count down to, so nothing to chase with. The single nudge
    /// `first_reminder_at` schedules is the whole of it.
    #[test]
    fn an_assignment_with_no_deadline_is_chased_once() {
        let now = datetime!(2026-09-18 09:00 UTC);
        assert_eq!(
            first_reminder_at(now, None),
            Some(now + TimeDuration::hours(24))
        );
        assert_eq!(next_reminder_at(now, None, 1), None);
    }

    /// The recipient is told when nobody will chase again. Without it, the
    /// third reminder reads exactly like the second and ignoring it feels like
    /// a delay rather than a decision.
    #[test]
    fn the_last_reminder_says_it_is_the_last() {
        let (subject, _, intro) = team_email_frame(
            BriefingLocale::En,
            "Wojtek",
            "Approve the content artifact",
            MAX_REMINDERS_PER_ASSIGNMENT as u8,
        );
        assert!(subject.starts_with("VIRYA — last reminder:"), "{subject}");
        assert!(intro.contains("no more will follow"), "{intro}");

        let (subject, _, _) = team_email_frame(BriefingLocale::En, "Wojtek", "Approve it", 1);
        assert!(subject.starts_with("VIRYA — reminder:"), "{subject}");
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
            team_email_frame(BriefingLocale::Pl, "Wojtek", "Domknij zadanie", 2);
        assert!(subject.starts_with("VIRYA — przypomnienie:"));
        assert!(intro.contains("przypominamy"));
    }
}
