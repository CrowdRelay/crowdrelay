#[cfg(test)]
mod tests {
    use super::*;

    fn venue() -> VenueEvidence {
        VenueEvidence {
            name: "Klub X".to_owned(),
            shows_last_12_months: 14,
            comparable_acts: 3,
            capacity: Some(300),
            typical_draw: Some(180),
            contact_verified_days_ago: Some(18),
            days_since_last_event: Some(11),
            has_booking_route: true,
        }
    }

    fn opportunity() -> CityOpportunity {
        CityOpportunity {
            city_id: crate::CityId::from_uuid(uuid::Uuid::from_u128(0x47)),
            city: "Wrocław".to_owned(),
            latitude: None,
            longitude: None,
            reachable_fans: Some(240),
            active_fans_30d: 60,
            months_since_show: Some(14),
            has_upcoming_show: false,
            converted_fans_90d: 0,
            top_conversion_channel: None,
            top_conversion_channel_fans: 0,
            venue: Some(venue()),
            promoters: vec![PromoterRef {
                key: "anna".to_owned(),
                name: "Anna".to_owned(),
                relationship_score: 70,
                answered_last_time: true,
                has_route: true,
            }],
            co_bill: Vec::new(),
            local_acts: Vec::new(),
        }
    }

    #[test]
    fn a_strong_city_produces_a_plan_with_its_reasons() {
        let plan = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        assert_eq!(plan.city, "Wrocław");
        assert_eq!(plan.venue, "Klub X");
        assert_eq!(
            plan.contact
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Anna"]
        );
        assert!(
            plan.reasons.contains(&Reason::ComparableActsPlayedHere {
                count: 3,
                of_shows: 14
            }),
            "the strongest fact a promoter recognises was not given as a reason"
        );
        assert!(plan.reasons.contains(&Reason::WarmPromoter {
            name: "Anna".to_owned()
        }));
        assert!(!plan.reasons.is_empty());
    }

    /// The failure this module exists to avoid: proposing regardless.
    #[test]
    fn a_city_with_almost_nobody_in_it_is_refused() {
        let mut thin = opportunity();
        thin.reachable_fans = Some(MINIMUM_REACHABLE_FOR_A_GIG - 1);
        assert_eq!(
            plan_gig(&thin, TenantIntent::BookingShows),
            Err(GigRefusal::TooFewReachable {
                reachable: MINIMUM_REACHABLE_FOR_A_GIG - 1,
                floor: MINIMUM_REACHABLE_FOR_A_GIG,
            })
        );
        // Exactly at the floor proposes. An off-by-one here silently removes a
        // band of real cities from every suggestion they ever get.
        let mut at_floor = opportunity();
        at_floor.reachable_fans = Some(MINIMUM_REACHABLE_FOR_A_GIG);
        assert!(plan_gig(&at_floor, TenantIntent::BookingShows).is_ok());
    }

    /// A city that cannot be measured is not a city measured at zero. The
    /// two refusals have to stay distinct: "nobody asked" and "we cannot
    /// tell" send the band to different work.
    #[test]
    fn an_unmeasurable_city_is_not_a_zero_audience() {
        let mut unmeasurable = opportunity();
        unmeasurable.reachable_fans = None;
        assert_eq!(
            plan_gig(&unmeasurable, TenantIntent::BookingShows),
            Err(GigRefusal::AudienceNotMeasurable)
        );

        let mut measured_empty = opportunity();
        measured_empty.reachable_fans = Some(0);
        assert!(matches!(
            plan_gig(&measured_empty, TenantIntent::BookingShows),
            Err(GigRefusal::TooFewReachable { reachable: 0, .. })
        ));
    }

    #[test]
    fn a_city_already_booked_is_not_a_gap() {
        let mut booked = opportunity();
        booked.has_upcoming_show = true;
        assert_eq!(
            plan_gig(&booked, TenantIntent::BookingShows),
            Err(GigRefusal::AlreadyBooked)
        );
    }

    /// The tenant's own plan outranks the evidence. A band that said it is
    /// recording does not want a gig proposal, however good the city looks —
    /// and ignoring that is how a helpful system becomes a noisy one.
    #[test]
    fn a_tenant_who_said_they_are_writing_is_not_pitched_shows() {
        assert_eq!(
            plan_gig(&opportunity(), TenantIntent::HeadsDown),
            Err(GigRefusal::NotWhatTheTenantIsDoing {
                intent: TenantIntent::HeadsDown
            })
        );
        // And the refusal is not a dead end — it says how to turn it back on.
        let message = GigRefusal::NotWhatTheTenantIsDoing {
            intent: TenantIntent::HeadsDown,
        }
        .message();
        assert!(message.contains("Change that"), "no way back: {message}");
    }

    /// Never seen an event and stopped having them need different next steps,
    /// so they must not collapse into one refusal message.
    #[test]
    fn a_dormant_room_and_an_unseen_room_read_differently() {
        let mut quiet = opportunity();
        if let Some(room) = quiet.venue.as_mut() {
            room.days_since_last_event = Some(DORMANT_ROOM_DAYS + 1);
        }
        let stopped = plan_gig(&quiet, TenantIntent::BookingShows).expect_err("refuses");
        assert!(stopped.message().contains("stopped programming"));

        let mut unseen = opportunity();
        if let Some(room) = unseen.venue.as_mut() {
            room.days_since_last_event = None;
        }
        let never = plan_gig(&unseen, TenantIntent::BookingShows).expect_err("refuses");
        assert!(never.message().contains("never seen an event"));
        assert_ne!(stopped.message(), never.message());
    }

    /// A room at the dormancy boundary still proposes. The bound is a bound.
    #[test]
    fn the_dormancy_bound_is_a_boundary_not_a_vibe() {
        let mut edge = opportunity();
        if let Some(room) = edge.venue.as_mut() {
            room.days_since_last_event = Some(DORMANT_ROOM_DAYS);
        }
        assert!(plan_gig(&edge, TenantIntent::BookingShows).is_ok());
    }

    #[test]
    fn a_room_nobody_can_write_to_is_refused_by_name() {
        let mut unreachable = opportunity();
        if let Some(room) = unreachable.venue.as_mut() {
            room.has_booking_route = false;
        }
        unreachable.promoters.clear();
        assert_eq!(
            plan_gig(&unreachable, TenantIntent::BookingShows),
            Err(GigRefusal::NoContactableRoute {
                venue: "Klub X".to_owned()
            })
        );
    }

    /// A promoter with a route rescues a room that has none — the room is the
    /// place, the promoter is the person, and only one of them has to be
    /// reachable.
    #[test]
    fn a_promoter_with_a_route_is_enough_without_the_venues_own() {
        let mut through_promoter = opportunity();
        if let Some(room) = through_promoter.venue.as_mut() {
            room.has_booking_route = false;
        }
        assert!(plan_gig(&through_promoter, TenantIntent::BookingShows).is_ok());
    }

    // ── The bill ────────────────────────────────────────────────────────────

    /// The whole point of a co-bill. An act that brings the same people is one
    /// show with two names on it and a door split two ways.
    #[test]
    fn a_co_bill_that_brings_the_same_people_is_not_invited() {
        let mut same_crowd = opportunity();
        same_crowd.co_bill = vec![CoBillAct {
            workspace: crate::WorkspaceId::new(),
            name: "Twin".to_owned(),
            reachable_here: 200,
            shared_with_tenant: 200,
            audience_overlap_basis_points: 10_000,
            consented_to_share_bills: true,
        }];
        let plan = plan_gig(&same_crowd, TenantIntent::BookingShows).expect("proposes");
        assert!(plan.invite_to_bill.is_empty(), "invited its own audience");
        assert_eq!(plan.reach.added_by_co_bill, 0);
    }

    #[test]
    fn a_co_bill_that_adds_people_is_invited_with_the_number_it_adds() {
        let mut complementary = opportunity();
        complementary.co_bill = vec![CoBillAct {
            workspace: crate::WorkspaceId::new(),
            name: "Other".to_owned(),
            reachable_here: 200,
            // A quarter of their audience is already ours — 50 real people,
            // carried exactly rather than rebuilt from the basis-point share.
            shared_with_tenant: 50,
            audience_overlap_basis_points: 2_500,
            consented_to_share_bills: true,
        }];
        let plan = plan_gig(&complementary, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(plan.invite_to_bill, vec!["Other"]);
        assert_eq!(plan.reach.added_by_co_bill, 150);
        assert!(plan.reasons.contains(&Reason::CoBillAddsAudience {
            act: "Other".to_owned(),
            adds_reachable: 150
        }));
    }

    /// Past the pairing ceiling the audiences are one audience — the same
    /// rule the roster's support picker applies. "Adds a hundred new people"
    /// is still a door split over a crowd that was already coming.
    #[test]
    fn a_co_bill_past_the_overlap_ceiling_is_not_invited() {
        let mut mirror = opportunity();
        mirror.co_bill = vec![CoBillAct {
            workspace: crate::WorkspaceId::new(),
            name: "Mirror".to_owned(),
            reachable_here: 1_000,
            shared_with_tenant: 700,
            audience_overlap_basis_points: 7_000,
            consented_to_share_bills: true,
        }];
        let plan = plan_gig(&mirror, TenantIntent::BookingShows).expect("proposes");
        assert!(
            plan.invite_to_bill.is_empty(),
            "a 70%-shared sibling was invited for its 300 additions"
        );
        assert_eq!(plan.reach.added_by_co_bill, 0);
    }

    /// An act that has not agreed is a suggestion to ask, never a name on a
    /// proposal that somebody might announce.
    #[test]
    fn an_act_that_never_agreed_is_not_put_on_a_bill() {
        let mut unconsented = opportunity();
        unconsented.co_bill = vec![CoBillAct {
            workspace: crate::WorkspaceId::new(),
            name: "Unasked".to_owned(),
            reachable_here: 300,
            shared_with_tenant: 0,
            audience_overlap_basis_points: 0,
            consented_to_share_bills: false,
        }];
        let plan = plan_gig(&unconsented, TenantIntent::BookingShows).expect("proposes");
        assert!(plan.invite_to_bill.is_empty());
        assert_eq!(plan.reach.added_by_co_bill, 0);
    }

    // ── What we do not know, said out loud ──────────────────────────────────

    #[test]
    fn everything_unmeasured_becomes_a_caveat_rather_than_a_silence() {
        let mut unknowns = opportunity();
        if let Some(room) = unknowns.venue.as_mut() {
            room.capacity = None;
            room.typical_draw = None;
            room.contact_verified_days_ago = None;
            room.comparable_acts = 0;
        }
        let plan = plan_gig(&unknowns, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(
            plan.caveats.len(),
            4,
            "a silent unknown: {:?}",
            plan.caveats
        );
        assert!(plan.reach.room_typical_draw.is_none());
        assert!(
            plan.reach.basis.contains("unmeasured"),
            "the basis hid that the room has no measured draw"
        );
        // And an unmeasured room must not claim a draw it does not have.
        assert!(
            !plan
                .reasons
                .iter()
                .any(|reason| matches!(reason, Reason::RoomDraws { .. }))
        );
    }

    #[test]
    fn a_stale_contact_is_flagged_and_a_fresh_one_is_not() {
        let mut stale = opportunity();
        if let Some(room) = stale.venue.as_mut() {
            room.contact_verified_days_ago = Some(CONTACT_STALE_DAYS + 1);
        }
        let flagged = plan_gig(&stale, TenantIntent::BookingShows).expect("proposes");
        assert!(
            flagged
                .caveats
                .iter()
                .any(|note| note.contains("People leave venues")),
            "a four-month-old address was presented as a live route"
        );

        // The default fixture's contact is 18 days old and must not be flagged.
        let fresh = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        assert!(
            !fresh
                .caveats
                .iter()
                .any(|note| note.contains("People leave venues")),
            "a fresh contact was flagged as stale"
        );
    }

    // ── Timing ──────────────────────────────────────────────────────────────

    #[test]
    fn a_city_with_fans_and_no_history_reads_differently_from_an_overdue_one() {
        let mut never = opportunity();
        never.months_since_show = None;
        let first_time = plan_gig(&never, TenantIntent::BookingShows).expect("proposes");
        assert!(
            first_time
                .reasons
                .contains(&Reason::NeverPlayedButHasFans { reachable: 240 })
        );

        let overdue = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        assert!(overdue.reasons.contains(&Reason::OverdueReturn {
            months: 14,
            active_30d: 60
        }));
    }

    /// Attributed arrivals are a reason the room hears: a city whose people
    /// already came through a tracked channel is warmer ground than one that
    /// only counts followers. And the reason names the channel, because "our
    /// marketing works here" is a claim a promoter cannot check.
    #[test]
    fn a_city_that_converted_fans_says_so_with_the_channel() {
        let mut converting = opportunity();
        // Eleven fans arrived in the city across every channel; six of them
        // came through the top one. The reason must claim the six — pairing
        // the all-channel total with one channel's name would assert fans
        // that channel never made.
        converting.converted_fans_90d = 11;
        converting.top_conversion_channel = Some("concert_qr".to_owned());
        converting.top_conversion_channel_fans = 6;
        let plan = plan_gig(&converting, TenantIntent::BookingShows).expect("proposes");
        assert!(
            plan.reasons.contains(&Reason::FansConvertedHere {
                count: 6,
                channel: "concert_qr".to_owned()
            }),
            "the attributed arrivals were measured and never said: {:?}",
            plan.reasons
        );

        // A city nobody converted through is a city with no such reason —
        // zero is a measurement, not a missing field.
        let quiet = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        assert!(
            !quiet
                .reasons
                .iter()
                .any(|reason| matches!(reason, Reason::FansConvertedHere { .. }))
        );
    }

    /// A release-focused tenant still gets the proposal, framed as serving the
    /// release rather than competing with it.
    #[test]
    fn a_release_becomes_the_frame_rather_than_a_blocker() {
        let plan = plan_gig(&opportunity(), TenantIntent::WorkingARelease).expect("proposes");
        assert!(plan.fits_intent.contains("launch night"));
    }

    #[test]
    fn an_unstated_intent_says_the_timing_is_unverified() {
        let plan = plan_gig(&opportunity(), TenantIntent::Unstated).expect("proposes");
        assert!(plan.fits_intent.contains("unverified"));
        assert!(!plan.fits_intent.is_empty());
    }

    #[test]
    fn every_refusal_says_what_would_change_it() {
        for refusal in [
            GigRefusal::AlreadyBooked,
            GigRefusal::NoRoomOnRecord,
            GigRefusal::NoContactableRoute {
                venue: "Klub X".to_owned(),
            },
            GigRefusal::RoomLooksDormant {
                venue: "Klub X".to_owned(),
                days_since_last_event: Some(400),
            },
            GigRefusal::RoomLooksDormant {
                venue: "Klub X".to_owned(),
                days_since_last_event: None,
            },
            GigRefusal::TooFewReachable {
                reachable: 12,
                floor: MINIMUM_REACHABLE_FOR_A_GIG,
            },
            GigRefusal::NotWhatTheTenantIsDoing {
                intent: TenantIntent::HeadsDown,
            },
        ] {
            let message = refusal.message();
            assert!(message.len() > 40, "too terse: {message}");
            assert!(
                !message.contains("Err(") && !message.contains("None"),
                "leaks Rust at the band: {message}"
            );
        }
    }

    /// Two runs over the same evidence must produce the same proposal, or a
    /// band re-reading yesterday's suggestion sees a different one and stops
    /// trusting both.
    #[test]
    fn the_same_evidence_always_produces_the_same_proposal() {
        let mut many = opportunity();
        many.promoters = vec![
            PromoterRef {
                key: "zed".to_owned(),
                name: "Zed".to_owned(),
                relationship_score: 60,
                answered_last_time: true,
                has_route: true,
            },
            PromoterRef {
                key: "ada".to_owned(),
                name: "Ada".to_owned(),
                relationship_score: 60,
                answered_last_time: true,
                has_route: true,
            },
        ];
        let first = plan_gig(&many, TenantIntent::BookingShows).expect("proposes");
        let second = plan_gig(&many, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(first, second);
        assert_eq!(
            first
                .contact
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Ada", "Zed"]
        );
    }

    /// Both directions, over every variant. A stored intent that parses back to
    /// something else is a band told "heads down" that keeps getting gigs, and
    /// the failure would be invisible until somebody complained.
    #[test]
    fn every_intent_round_trips_through_its_stored_form() {
        for intent in TenantIntent::all() {
            assert_eq!(TenantIntent::parse(intent.as_str()), Some(intent));
        }
        // And no two variants share a stored form.
        let mut stored: Vec<&str> = TenantIntent::all()
            .into_iter()
            .map(TenantIntent::as_str)
            .collect();
        stored.sort_unstable();
        let distinct = stored.len();
        stored.dedup();
        assert_eq!(stored.len(), distinct);
    }

    /// Unrecognised is `None`, never a variant. The caller decides, and the two
    /// callers decide differently on purpose.
    #[test]
    fn an_unreadable_value_parses_to_nothing_rather_than_to_unstated() {
        assert_eq!(TenantIntent::parse(""), None);
        assert_eq!(TenantIntent::parse("Booking_Shows"), None);
        assert_eq!(TenantIntent::parse("touring"), None);
        // Whitespace around a real value is a stored-row artefact, not a typo.
        assert_eq!(
            TenantIntent::parse(" heads_down "),
            Some(TenantIntent::HeadsDown)
        );
    }

    /// §4G.4: the letter's first line is the proposal's strongest reason,
    /// rendered once. Two code paths describing one decision drift, and the
    /// one the promoter read is the one that counts.
    #[test]
    fn the_opening_line_states_the_reason_the_proposal_was_made_of() {
        let plan = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        let opening = plan.opening_line();
        match plan.reasons.first().expect("a proposal has reasons") {
            Reason::ComparableActsPlayedHere { count, .. } => {
                assert!(
                    opening.contains(&count.to_string()),
                    "the strongest reason's own number is missing: {opening}"
                );
                assert!(opening.contains(&plan.venue));
            }
            other => panic!("fixture changed shape: {other:?}"),
        }
    }

    /// Every reason renders. A variant added without a sentence would send a
    /// letter that opens with a fallback nobody chose, and nothing would fail.
    #[test]
    fn every_reason_produces_an_opening_a_promoter_can_read() {
        let reasons = [
            Reason::ComparableActsPlayedHere {
                count: 4,
                of_shows: 14,
            },
            Reason::ReachableAudience { reachable: 300 },
            Reason::RoomDraws { typical_draw: 180 },
            Reason::NeverPlayedButHasFans { reachable: 300 },
            Reason::OverdueReturn {
                months: 18,
                active_30d: 90,
            },
            Reason::CoBillAddsAudience {
                act: "Other Band".to_owned(),
                adds_reachable: 120,
            },
            Reason::WarmPromoter {
                name: "Anna".to_owned(),
            },
            Reason::RoomIsActive {
                days_since_last_event: 12,
            },
            Reason::FansConvertedHere {
                count: 9,
                channel: "concert_qr".to_owned(),
            },
        ];
        let base = plan_gig(&opportunity(), TenantIntent::BookingShows).expect("proposes");
        for reason in reasons {
            let mut plan = base.clone();
            plan.reasons = vec![reason.clone()];
            let opening = plan.opening_line();
            assert!(
                opening.len() > 30 && opening.ends_with('.'),
                "{reason:?} rendered as {opening:?}"
            );
            // A fact, not a feeling. The promoter reads forty of these a week.
            assert!(!opening.to_lowercase().contains("love"), "{opening}");
            assert!(!opening.contains('!'), "{opening}");
        }
    }

    /// Two promoters who share a display name are two recipients.
    ///
    /// The booking list is unique on the contact address, not on the name, so
    /// "Anna" is not an identity. Collapsing them by name meant one of the two
    /// was silently never written to while the console showed both — and this
    /// feature's one promise is that everybody who books the room hears about
    /// the night.
    #[test]
    fn two_promoters_with_one_name_are_two_recipients() {
        let mut namesakes = opportunity();
        namesakes.promoters = vec![
            PromoterRef {
                key: "target-1".to_owned(),
                name: "Anna".to_owned(),
                relationship_score: 70,
                answered_last_time: true,
                has_route: true,
            },
            PromoterRef {
                key: "target-2".to_owned(),
                name: "Anna".to_owned(),
                relationship_score: 60,
                answered_last_time: false,
                has_route: true,
            },
        ];
        let plan = plan_gig(&namesakes, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(
            plan.contact.len(),
            2,
            "a namesake was dropped: {:?}",
            plan.contact
        );
        let keys: Vec<&str> = plan.contact.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["target-1", "target-2"]);
    }

    /// The same promoter listed twice is one recipient. Deduplication is by
    /// key, which is what makes the namesake case above work.
    #[test]
    fn one_promoter_listed_twice_is_written_to_once() {
        let mut repeated = opportunity();
        let promoter = PromoterRef {
            key: "target-1".to_owned(),
            name: "Anna".to_owned(),
            relationship_score: 70,
            answered_last_time: true,
            has_route: true,
        };
        repeated.promoters = vec![promoter.clone(), promoter];
        let plan = plan_gig(&repeated, TenantIntent::BookingShows).expect("proposes");
        assert_eq!(plan.contact.len(), 1);
    }

    /// The console renders `describe`; an empty one would be a radio button
    /// with no label.
    #[test]
    fn every_intent_carries_a_sentence_a_band_can_choose_from() {
        for intent in TenantIntent::all() {
            assert!(intent.describe().len() > 30, "{}", intent.as_str());
        }
    }
}
