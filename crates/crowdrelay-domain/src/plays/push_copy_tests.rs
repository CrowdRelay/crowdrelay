// Fan-facing play copy, per act. Nothing tested `push_copy` before the
// wordmark stopped being a literal; these pin the first tenant's exact titles
// and prove another act's copy never carries the first tenant's name.
#[cfg(test)]
mod push_copy_tests {
    use super::*;

    fn push_facts(wordmark: &str, polish: bool) -> PlayStepPushFacts<'_> {
        PlayStepPushFacts {
            wordmark,
            polish,
            event_title: Some("Club night"),
            event_date: Some("2026-10-05"),
            event_venue: Some("Firlej"),
            event_path: Some("/my-signal/?event=club-night"),
            follow_link: Some("/l/follow"),
            release_title: Some("Echo"),
            release_date: Some("2026-11-01"),
            release_link: Some("/l/echo"),
        }
    }

    /// Every fan-facing title the first tenant has ever sent, exactly as it
    /// was sent when the wordmark was a string literal. The literal is gone;
    /// these must still come out byte for byte, or real fans see a change.
    const VIRYA_TITLES: [(PlayStepKind, &str, &str); 10] = [
        (
            PlayStepKind::AnnounceAsk,
            "VIRYA · nowy koncert",
            "VIRYA · new show",
        ),
        (
            PlayStepKind::PostShowAsk,
            "VIRYA · dzięki za wczoraj",
            "VIRYA · thanks for being there",
        ),
        (
            PlayStepKind::FollowAskFirst,
            "VIRYA · zostań blisko",
            "VIRYA · stay close",
        ),
        (
            PlayStepKind::FollowAskSecond,
            "VIRYA · jeszcze raz",
            "VIRYA · once more",
        ),
        (
            PlayStepKind::FollowAskFinal,
            "VIRYA · ostatni raz",
            "VIRYA · last ask",
        ),
        (
            PlayStepKind::DormantRevivalFirst,
            "VIRYA · dawno Cię nie było",
            "VIRYA · it has been a while",
        ),
        (
            PlayStepKind::DormantRevivalFinal,
            "VIRYA · ostatni sygnał",
            "VIRYA · last signal",
        ),
        (
            PlayStepKind::ReleaseAudienceAnnounce,
            "VIRYA · nowy materiał",
            "VIRYA · new release",
        ),
        (
            PlayStepKind::ReleaseDayPush,
            "VIRYA · premiera",
            "VIRYA · release day",
        ),
        (
            PlayStepKind::ReleaseSustainAsk,
            "VIRYA · wciąż gra",
            "VIRYA · still playing",
        ),
    ];

    #[test]
    fn the_first_tenants_push_titles_are_unchanged() {
        for (kind, polish_title, english_title) in VIRYA_TITLES {
            let polish = kind.push_copy(&push_facts("VIRYA", true));
            let english = kind.push_copy(&push_facts("VIRYA", false));
            assert_eq!(
                polish.map(|copy| copy.title).as_deref(),
                Some(polish_title),
                "{}",
                kind.as_str()
            );
            assert_eq!(
                english.map(|copy| copy.title).as_deref(),
                Some(english_title),
                "{}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn another_act_speaks_as_itself() {
        // A roster runs several acts at once; a fan of one must never be told
        // about a show under another band's name.
        for (kind, _, _) in VIRYA_TITLES {
            for polish in [true, false] {
                let copy = kind
                    .push_copy(&push_facts("Mgła", polish))
                    .unwrap_or_else(|| panic!("{} has copy", kind.as_str()));
                assert!(copy.title.starts_with("Mgła · "), "{}", copy.title);
                assert!(!copy.title.contains("VIRYA"), "{}", copy.title);
                assert!(!copy.body.contains("VIRYA"), "{}", copy.body);
            }
        }
    }
}
