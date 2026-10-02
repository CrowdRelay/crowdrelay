use super::*;

fn slug(value: &str) -> crate::SmartLinkSlug {
    crate::SmartLinkSlug::parse(value).unwrap()
}

fn sender() -> SenderIdentity {
    SenderIdentity {
        act_name: "VIRYA".to_owned(),
        style: Some("modern metal".to_owned()),
        home_city: Some("Wrocław".to_owned()),
        site_url: Some(TrackedLink::for_site("https://virya.music", &slug("site"))),
    }
}

fn input<'a>(
    sender: &'a SenderIdentity,
    kind: OutreachTargetKind,
    phase: OutreachPhase,
) -> OutreachLetterInput<'a> {
    OutreachLetterInput {
        sender,
        target_name: "Metal Playlists Weekly",
        target_kind: kind,
        pitch_title: "our new single \"Rytuał\"",
        pitch_url: Some(TrackedLink::for_site(
            "https://virya.music",
            &slug("rytual"),
        )),
        phase,
        language: LetterLanguage::English,
        next_show: None,
    }
}

#[test]
fn a_playlist_ask_is_short_human_and_not_a_crm_template() {
    let sender = sender();
    let en = compose_outreach_letter(&input(
        &sender,
        OutreachTargetKind::Playlist,
        OutreachPhase::Initial,
    ))
    .expect("a complete input composes");
    assert!(en.body.starts_with("Hi Metal Playlists Weekly,"));
    assert!(
        en.body
            .contains("We have a new track — our new single \"Rytuał\".")
    );
    assert!(en.body.contains("If it fits the playlist, here it is:"));
    assert!(en.body.contains("https://virya.music/l/rytual"));
    for bot in [
        "I am writing from",
        "we would love to submit",
        "playlist consideration",
        "just say so",
        "will not follow up",
    ] {
        assert!(!en.body.contains(bot), "{bot}: {}", en.body);
    }
    assert!(
        en.body
            .ends_with("Best,\nVIRYA\nhttps://virya.music/l/site")
    );

    let pl = compose_outreach_letter(&OutreachLetterInput {
        language: LetterLanguage::Polish,
        ..input(
            &sender,
            OutreachTargetKind::Playlist,
            OutreachPhase::Initial,
        )
    })
    .expect("a complete Polish playlist ask composes");
    assert!(pl.body.starts_with("Dzień dobry, Metal Playlists Weekly,"));
    assert!(
        pl.body
            .contains("Mamy nowy numer — our new single \"Rytuał\".")
    );
    assert!(
        pl.body
            .contains("Jeśli pasuje do profilu playlisty, zostawiamy go tutaj:")
    );
    for bot in [
        "Piszemy w imieniu",
        "chcielibyśmy zaproponować",
        "do Waszej playlisty",
        "nie będziemy się więcej odzywać",
    ] {
        assert!(!pl.body.contains(bot), "{bot}: {}", pl.body);
    }
    assert!(
        pl.body
            .ends_with("Pozdrawiamy,\nVIRYA\nhttps://virya.music/l/site")
    );
}

#[test]
fn a_followup_re_asks_without_rewriting_the_pitch() {
    let sender = sender();
    let letter = compose_outreach_letter(&input(
        &sender,
        OutreachTargetKind::Press,
        OutreachPhase::FollowUp,
    ))
    .expect("a follow-up composes");
    assert!(letter.body.contains("quick follow-up"));
    assert!(letter.body.contains("for coverage"));
    assert!(letter.subject.starts_with("Follow-up:"));
}

#[test]
fn missing_facts_refuse_instead_of_filling_in() {
    let sender = sender();
    let no_target = OutreachLetterInput {
        target_name: " ",
        ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
    };
    assert_eq!(
        compose_outreach_letter(&no_target),
        Err(OutreachLetterRefusal::NoTarget)
    );
    let no_pitch = OutreachLetterInput {
        pitch_url: None,
        ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
    };
    assert_eq!(
        compose_outreach_letter(&no_pitch),
        Err(OutreachLetterRefusal::NoPitch)
    );
    let no_name = SenderIdentity {
        act_name: String::new(),
        ..sender.clone()
    };
    let no_sender = input(&no_name, OutreachTargetKind::Press, OutreachPhase::Initial);
    assert_eq!(
        compose_outreach_letter(&no_sender),
        Err(OutreachLetterRefusal::NoSenderName)
    );
}

fn thread_input<'a>(sender: &'a SenderIdentity, email: &'a str) -> ThreadFollowUpInput<'a> {
    ThreadFollowUpInput {
        sender,
        target_name: "Radio Zet",
        contact_email: email,
        thread_started_at: OffsetDateTime::from_unix_timestamp(1_758_000_000)
            .expect("a fixed timestamp"),
    }
}

#[test]
fn a_thread_followup_names_the_message_date_and_no_more() {
    let sender = sender();
    // 1_758_000_000 is 2025-09-16.
    let letter = compose_thread_followup_letter(&thread_input(&sender, "music@zine.example"))
        .expect("a complete input composes");
    assert!(letter.body.contains("Hi Radio Zet,"));
    assert!(letter.body.contains("my message from 16 Sep"));
    assert!(!letter.body.contains("Listen:"));
    assert_eq!(letter.subject, "Following up — VIRYA");
}

#[test]
fn a_polish_mailbox_gets_the_polish_followup() {
    let sender = sender();
    let letter = compose_thread_followup_letter(&thread_input(&sender, "redakcja@radio.pl"))
        .expect("a complete input composes");
    assert!(letter.body.contains("Cześć Radio Zet,"));
    assert!(letter.body.contains("wiadomości z 16 września"));
    assert!(letter.body.contains("Pozdrawiam,"));
    assert_eq!(letter.subject, "Nawiązanie — VIRYA");
    // And the same sender, an international mailbox: English.
    let en = compose_thread_followup_letter(&thread_input(&sender, "desk@station.fm"))
        .expect("a complete input composes");
    assert!(en.body.contains("Following up on my message"));
}

#[test]
fn a_thread_followup_still_refuses_anonymous() {
    let sender = sender();
    let no_target = ThreadFollowUpInput {
        target_name: " ",
        ..thread_input(&sender, "music@zine.example")
    };
    assert_eq!(
        compose_thread_followup_letter(&no_target),
        Err(OutreachLetterRefusal::NoTarget)
    );
}

#[test]
fn representation_targets_are_never_pitched() {
    let sender = sender();
    for kind in [OutreachTargetKind::Agent, OutreachTargetKind::Label] {
        assert_eq!(
            compose_outreach_letter(&input(&sender, kind, OutreachPhase::Initial)),
            Err(OutreachLetterRefusal::RepresentationTarget),
            "{kind:?} must refuse — the listing carries that approach"
        );
    }
}

#[test]
fn a_pl_address_reads_polish_and_everything_else_english() {
    assert_eq!(
        language_for_contact("redakcja@metalrulez.pl"),
        LetterLanguage::Polish
    );
    assert_eq!(
        language_for_contact("Radio@Onet.PL."),
        LetterLanguage::Polish
    );
    assert_eq!(
        language_for_contact("zine@gmail.com"),
        LetterLanguage::English
    );
    assert_eq!(
        language_for_contact("booking@club.de"),
        LetterLanguage::English
    );
    // A `.pl` that is not the domain says nothing.
    assert_eq!(
        language_for_contact("jan.pl@example.com"),
        LetterLanguage::English
    );
    assert_eq!(
        language_for_contact("no-at-sign.pl"),
        LetterLanguage::English
    );
}

#[test]
fn the_greeting_drops_the_role_the_contact_list_appended() {
    assert_eq!(
        salutation_name("Kamil Dráb — hudební dramaturg"),
        "Kamil Dráb"
    );
    assert_eq!(salutation_name("Metal Noise - review blog"), "Metal Noise");
    assert_eq!(salutation_name("  Radio 357  "), "Radio 357");
    assert_eq!(
        salutation_name("Rock-Radio"),
        "Rock-Radio",
        "a hyphen inside a name stays"
    );
    let sender = sender();
    let letter = compose_outreach_letter(&OutreachLetterInput {
        target_name: "Kamil Dráb — hudební dramaturg",
        ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
    })
    .expect("composes");
    assert!(
        letter.body.starts_with("Hi Kamil Dráb,\n"),
        "{}",
        letter.body
    );
}

#[test]
fn a_polish_pitch_is_polish_end_to_end() {
    let sender = sender();
    let letter = compose_outreach_letter(&OutreachLetterInput {
        language: LetterLanguage::Polish,
        ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
    })
    .expect("a complete input composes");
    assert!(
        letter
            .body
            .starts_with("Dzień dobry, Metal Playlists Weekly,\n")
    );
    assert!(letter.body.contains(
        "Piszemy w imieniu VIRYA (Wrocław) — zespołu grającego modern metal — i chcielibyśmy \
         zaproponować Wam our new single \"Rytuał\" do omówienia lub recenzji."
    ));
    assert!(
        letter
            .body
            .contains("Do posłuchania: https://virya.music/l/rytual")
    );
    assert!(
        letter
            .body
            .ends_with("Pozdrawiamy,\nVIRYA\nhttps://virya.music/l/site")
    );
    assert!(
        !letter.body.contains("Hi "),
        "no English left in a Polish letter"
    );
    assert!(letter.body.contains("i nie będziemy się więcej odzywać"));
    assert_eq!(letter.subject, "VIRYA — our new single \"Rytuał\"");
}

#[test]
fn a_polish_follow_up_says_so_in_the_subject() {
    let sender = sender();
    let letter = compose_outreach_letter(&OutreachLetterInput {
        language: LetterLanguage::Polish,
        ..input(&sender, OutreachTargetKind::Radio, OutreachPhase::FollowUp)
    })
    .expect("a complete input composes");
    assert!(letter.subject.starts_with("Przypomnienie: VIRYA"));
    assert!(
        letter
            .body
            .contains("Krótko wracamy do poprzedniej wiadomości")
    );
}

#[test]
fn every_pitchable_kind_has_a_polish_ask() {
    for kind in [
        OutreachTargetKind::Playlist,
        OutreachTargetKind::Radio,
        OutreachTargetKind::Press,
        OutreachTargetKind::Creator,
        OutreachTargetKind::SupportSlot,
        OutreachTargetKind::Endorsement,
        OutreachTargetKind::MediaPatronage,
    ] {
        assert!(purpose(kind).is_some());
        assert!(!purpose_pl(kind).is_empty(), "{kind:?} has no Polish ask");
    }
    // An organiser answers to its own templates, not the release ask —
    // the loops above cover the kinds `purpose` and `purpose_pl` serve.
    assert!(purpose(OutreachTargetKind::Organiser).is_none());
    assert!(purpose_pl(OutreachTargetKind::Organiser).is_empty());
}

#[test]
fn an_organiser_is_asked_to_play_not_to_review() {
    let sender = sender();
    let show = Date::from_calendar_date(2026, time::Month::October, 17).unwrap();
    let letter = compose_outreach_letter(&OutreachLetterInput {
        target_name: "uROCK Młodych — organizator",
        target_kind: OutreachTargetKind::Organiser,
        next_show: Some((show, "Gorzów Wielkopolski".to_owned())),
        ..input(
            &sender,
            OutreachTargetKind::Organiser,
            OutreachPhase::Initial,
        )
    })
    .expect("a complete input composes");
    assert!(
        letter.body.starts_with("Hi uROCK Młodych,"),
        "{}",
        letter.body
    );
    assert!(
        letter
            .body
            .contains("VIRYA, a modern metal act from Wrocław")
    );
    assert!(letter.body.contains("considered for a slot on your bill"));
    assert!(
        letter
            .body
            .contains("Next confirmed show: 17 Oct 2026, Gorzów Wielkopolski."),
        "{}",
        letter.body
    );
    assert!(letter.body.contains("Listen: https://virya.music/l/rytual"));
    assert!(letter.body.contains("will not follow up"));
    assert!(letter.body.ends_with("https://virya.music/l/site"));
    assert_eq!(letter.subject, "VIRYA — gig request");
    // The failure this kind exists to end: no review request to a
    // festival organiser.
    let lower = letter.body.to_lowercase();
    assert!(!lower.contains("review"), "{lower}");
    assert!(!lower.contains("coverage"), "{lower}");
    assert!(!lower.contains("submit"), "{lower}");
    // The city named in the letter is where the band is *from* — the
    // show city appears only inside the citation.
    assert!(!letter.body.contains("act from Gorzów"), "{}", letter.body);
}

#[test]
fn an_organiser_letter_without_a_booked_show_still_reads_true() {
    let sender = sender();
    let letter = compose_outreach_letter(&input(
        &sender,
        OutreachTargetKind::Organiser,
        OutreachPhase::Initial,
    ))
    .expect("a gig ask needs no citation");
    assert!(letter.body.contains("considered for a slot on your bill"));
    assert!(!letter.body.contains("confirmed show"), "{}", letter.body);
    // A show with no city on record cites the date alone.
    let show = Date::from_calendar_date(2026, time::Month::October, 17).unwrap();
    let letter = compose_outreach_letter(&OutreachLetterInput {
        next_show: Some((show, String::new())),
        ..input(
            &sender,
            OutreachTargetKind::Organiser,
            OutreachPhase::Initial,
        )
    })
    .expect("composes");
    assert!(
        letter.body.contains("Next confirmed show: 17 Oct 2026."),
        "{}",
        letter.body
    );
}

#[test]
fn a_polish_organiser_is_asked_for_a_slot_in_polish() {
    let sender = sender();
    let show = Date::from_calendar_date(2026, time::Month::October, 17).unwrap();
    let letter = compose_outreach_letter(&OutreachLetterInput {
        language: LetterLanguage::Polish,
        next_show: Some((show, "Gorzów Wielkopolski".to_owned())),
        ..input(
            &sender,
            OutreachTargetKind::Organiser,
            OutreachPhase::Initial,
        )
    })
    .expect("a complete input composes");
    assert!(
        letter
            .body
            .starts_with("Dzień dobry, Metal Playlists Weekly,\n")
    );
    assert!(letter.body.contains(
        "Piszemy w imieniu VIRYA (Wrocław) — zespołu grającego modern metal — i chcielibyśmy \
         zapytać o możliwość zagrania u Was."
    ));
    assert!(
        letter.body.contains(
            "Najbliższy potwierdzony koncert: 17 października 2026, Gorzów Wielkopolski."
        )
    );
    assert!(
        letter
            .body
            .contains("Do posłuchania: https://virya.music/l/rytual")
    );
    assert!(letter.body.contains("i nie będziemy się więcej odzywać"));
    assert!(
        letter
            .body
            .ends_with("Pozdrawiamy,\nVIRYA\nhttps://virya.music/l/site")
    );
    assert!(!letter.body.contains("recenzj"), "{}", letter.body);
    assert!(!letter.body.contains("omówienia"), "{}", letter.body);
    assert_eq!(letter.subject, "VIRYA — propozycja koncertu");
}

#[test]
fn an_organiser_follow_up_re_asks_the_slot() {
    let sender = sender();
    let letter = compose_outreach_letter(&input(
        &sender,
        OutreachTargetKind::Organiser,
        OutreachPhase::FollowUp,
    ))
    .expect("a follow-up composes");
    assert!(letter.body.contains("the offer to play stands"));
    assert_eq!(letter.subject, "Follow-up: VIRYA — gig request");
    let polish = compose_outreach_letter(&OutreachLetterInput {
        language: LetterLanguage::Polish,
        ..input(
            &sender,
            OutreachTargetKind::Organiser,
            OutreachPhase::FollowUp,
        )
    })
    .expect("composes");
    assert!(polish.body.contains("propozycja zagrania"));
    assert_eq!(polish.subject, "Przypomnienie: VIRYA — propozycja koncertu");
}

#[test]
fn an_organiser_still_refuses_without_a_listen_link() {
    let sender = sender();
    let no_pitch = OutreachLetterInput {
        pitch_url: None,
        ..input(
            &sender,
            OutreachTargetKind::Organiser,
            OutreachPhase::Initial,
        )
    };
    assert_eq!(
        compose_outreach_letter(&no_pitch),
        Err(OutreachLetterRefusal::NoPitch)
    );
}

/// Each contact's letter is its own (see `greeting_pl`).
#[test]
fn polish_letters_to_two_contacts_differ() {
    let sender = sender();
    let compose = |name: &'static str| {
        compose_outreach_letter(&OutreachLetterInput {
            language: LetterLanguage::Polish,
            target_name: name,
            ..input(&sender, OutreachTargetKind::Press, OutreachPhase::Initial)
        })
        .expect("composes")
    };
    let radio = compose("Radio Gorzów — redakcja");
    let paper = compose("Tarnow.net.pl");
    assert!(
        radio.body.starts_with("Dzień dobry, Radio Gorzów,\n"),
        "{}",
        radio.body
    );
    assert_ne!(radio.body, paper.body);
    assert_eq!(greeting_pl("  "), "Dzień dobry,");
}
