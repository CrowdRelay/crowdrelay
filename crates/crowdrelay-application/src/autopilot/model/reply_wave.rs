// The reply lane and the approach wave: one payload answers a person who
// wrote back, the other carries a batch of agent letters behind a single
// approval card. Both are third-party sends; neither may hide inside a
// wildcard grant. They live here rather than in `model.rs` because the
// source-size ratchet owns that file's ceiling and these two payloads are
// the newest growth in it.

/// One agent's approach inside a `RequestBookingAgentApproachWave` — the
/// agent, the version the letter was approved against, and the finished
/// letter itself. The draw evidence sits on the wave, not here: the
/// readings are workspace-level, so carrying them per approach would say
/// the same numbers N times.
///
/// The version is the same optimistic lock the single lane holds: an
/// agent edited between approval and send fails the wave's dispatch
/// rather than being written to under stale terms.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingAgentApproachDraft {
    pub agent_id: BookingAgentId,
    pub agent_version: i64,
    pub agent_name: String,
    /// The agency the agent works for. Absent is honest: an independent is
    /// not an agency with no name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agency: Option<String>,
    /// The letter the approver read — the executor sends `body` verbatim
    /// and dispatch refuses the wave rather than compose here.
    #[serde(default)]
    pub draft: crowdrelay_domain::approach_letter::ApproachLetter,
}

impl AutopilotActionPayload {
    /// The target a standing approval for this action would cover, if there is
    /// one a person could sensibly judge once.
    ///
    /// `None` is the honest answer for most actions and the safe one for all
    /// of them: a standing approval is the operator saying "this *target* is
    /// fine, stop asking", and that sentence only means something where the
    /// action has a target that recurs. A push to the whole audience, a price
    /// change and a budget request each happen to one thing once; granting a
    /// standing approval over them would be granting it over the action kind,
    /// which is the wildcard `standing_approval` deliberately cannot express.
    ///
    /// A community is the case that does recur. The band posts to the same
    /// subreddit again next month, and by then the operator has read three
    /// drafts from it and knows the answer. The same is true of the
    /// outward-facing counterparties: the radio the band writes each
    /// release, the promoter behind every local show, the booking agent a
    /// season letter reaches every year, and the person whose reply is
    /// waiting — "stop asking me about this one" is a sentence an operator
    /// can mean about any of them.
    ///
    /// The key is the target's own id rather than the action's `subject_id`:
    /// an agent-outcome action carries the outcome id there, so a grant keyed
    /// on the subject would cover one draft and never the next.
    ///
    /// `RequestGigOutreach` stays `None`: its reach is a recipient *list*,
    /// and a grant over one action cannot name which of them it meant.
    /// `RequestOutreachReply` stays `None` for the harder reason: the action's
    /// draft is a scaffold a person completes against the real thread, and a
    /// grant would send it unedited — an answer to a person's own words is
    /// the one letter that must always be seen.
    #[must_use]
    pub fn standing_approval_target(&self) -> Option<String> {
        match self {
            Self::RequestCommunityEngagement { target_id, .. } => Some(target_id.to_string()),
            Self::RequestOutreach { target_id, .. } => Some(target_id.to_string()),
            Self::RequestBookingOutreach { target_id, .. } => Some(target_id.to_string()),
            Self::RequestBookingAgentApproach { agent_id, .. } => Some(agent_id.to_string()),
            Self::RequestBeaconOutreach { beacon_id, .. } => Some(beacon_id.to_string()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    /// The reply payload round-trips with its conversation pin intact, and
    /// it lands in the ledger as `outreach.reply.request` — a third-party
    /// send like the pitch it answers, and never standing-approvable: an
    /// answer is one letter to one conversation, not a posture.
    #[test]
    fn outreach_reply_payload_round_trips_and_stays_third_party()
    -> Result<(), Box<dyn std::error::Error>> {
        let target_id = OutreachTargetId::new();
        let payload = AutopilotActionPayload::RequestOutreachReply {
            target_id,
            target_version: 4,
            target_name: "Radiowa Trójka".to_owned(),
            reply_interaction_id: 912,
            reply_disposition: "received".to_owned(),
            sheet_verdict: Some("GMAIL_REPLY".to_owned()),
            draft: crowdrelay_domain::outreach_letter::OutreachLetter {
                subject: "Re: VIRYA — Technophobia".to_owned(),
                body: "Hi,\n\nThank you for getting back to us.".to_owned(),
            },
        };
        let json = serde_json::to_value(&payload)?;
        assert_eq!(json["kind"], serde_json::json!("request_outreach_reply"));
        let back: AutopilotActionPayload = serde_json::from_value(json)?;
        assert_eq!(back, payload);
        assert_eq!(payload.action_kind(), "outreach.reply.request");
        assert_eq!(payload.action_class(), ActionClass::ThirdParty);
        // A reply is never standing-approvable: the draft is a scaffold a
        // person completes against the real thread, and a grant would send
        // it unedited.
        assert_eq!(payload.standing_approval_target(), None);
        Ok(())
    }

    /// The agent-lane reply round-trips with its conversation pin intact and
    /// lands as `booking_agent.reply.request` — a third-party send like the
    /// approach it answers, and never standing-approvable for the same reason
    /// the outreach reply is not: a scaffold nobody completed must never
    /// auto-send to a person.
    #[test]
    fn booking_agent_reply_payload_round_trips_and_stays_third_party()
    -> Result<(), Box<dyn std::error::Error>> {
        let agent_id = BookingAgentId::new();
        let payload = AutopilotActionPayload::RequestBookingAgentReply {
            agent_id,
            agent_version: 3,
            agent_name: "Wielcy Agency".to_owned(),
            agency: Some("Wielcy".to_owned()),
            reply_interaction_id: 41,
            reply_disposition: "positive".to_owned(),
            draft: crowdrelay_domain::approach_letter::ApproachLetter {
                subject: "Re: VIRYA — season dates".to_owned(),
                body: "Hi,\n\nThank you for the quick answer.".to_owned(),
            },
        };
        let json = serde_json::to_value(&payload)?;
        assert_eq!(
            json["kind"],
            serde_json::json!("request_booking_agent_reply")
        );
        let back: AutopilotActionPayload = serde_json::from_value(json)?;
        assert_eq!(back, payload);
        assert_eq!(payload.action_kind(), "booking_agent.reply.request");
        assert_eq!(payload.action_class(), ActionClass::ThirdParty);
        assert_eq!(payload.standing_approval_target(), None);
        Ok(())
    }

    /// Every recurring outreach payload with a concrete counterpart exposes
    /// that counterpart's id to `standing_approval_target` — a grant is how
    /// the operator says "keep asking this promoter" without an approval per
    /// wave. Variants without a single counterpart (`RequestGigOutreach`'s
    /// recipient list) stay `None`: a grant must never widen into a wildcard.
    #[test]
    fn standing_approval_target_names_the_counterparty() {
        let target = OutreachTargetId::new();
        let agent = BookingAgentId::new();
        let beacon = BeaconId::new();
        let booking_target = BookingTargetId::new();
        let cases: Vec<(AutopilotActionPayload, String)> = vec![
            (
                AutopilotActionPayload::RequestOutreach {
                    opportunity_id: OutreachOpportunityId::new(),
                    target_id: target,
                    target_version: 1,
                    target_name: "Radio".to_owned(),
                    phase: OutreachPhase::Initial,
                    template_key: "press".to_owned(),
                    wave_id: None,
                    draft: crowdrelay_domain::outreach_letter::OutreachLetter {
                        subject: String::new(),
                        body: String::new(),
                    },
                },
                target.to_string(),
            ),
            (
                AutopilotActionPayload::RequestBookingOutreach {
                    city_id: CityId::new(),
                    target_id: booking_target,
                    target_version: 1,
                    target_name: "Klub".to_owned(),
                    score: 70,
                    phase: BookingOutreachPhase::Initial,
                    proposed_window: None,
                    additional_recipients: Vec::new(),
                    venue_evidence: None,
                    draft: crowdrelay_domain::booking_letter::BookingLetter {
                        subject: String::new(),
                        body: String::new(),
                    },
                },
                booking_target.to_string(),
            ),
            (
                AutopilotActionPayload::RequestBookingAgentApproach {
                    agent_id: agent,
                    agent_version: 1,
                    agent_name: "Agent".to_owned(),
                    agency: None,
                    note: None,
                    evidence: AgentDrawEvidence::default(),
                    draft: crowdrelay_domain::approach_letter::ApproachLetter {
                        subject: String::new(),
                        body: String::new(),
                    },
                },
                agent.to_string(),
            ),
            (
                AutopilotActionPayload::RequestBeaconOutreach {
                    beacon_id: beacon,
                    event_id: EventId::new(),
                    beacon_version: 1,
                    phase: BeaconOutreachPhase::Initial,
                    template_key: "intro".to_owned(),
                },
                beacon.to_string(),
            ),
        ];
        for (payload, expected) in cases {
            assert_eq!(payload.standing_approval_target(), Some(expected));
        }
    }

    /// A queued reply action from before the letter rode along parses to an
    /// empty draft — dispatch refuses it rather than composing an answer to
    /// a person nobody read.
    #[test]
    fn outreach_reply_payload_without_draft_still_parses() -> Result<(), Box<dyn std::error::Error>>
    {
        let AutopilotActionPayload::RequestOutreachReply { draft, .. } =
            serde_json::from_value(serde_json::json!({
                "kind": "request_outreach_reply",
                "target_id": uuid::Uuid::now_v7(),
                "target_version": 1,
                "target_name": "Metal Playlists Weekly",
                "reply_interaction_id": 5,
                "reply_disposition": "positive",
            }))?
        else {
            panic!("the payload must still parse as RequestOutreachReply")
        };
        assert!(draft.subject.is_empty() && draft.body.is_empty());
        Ok(())
    }

    /// The reply briefing names the person, the sheet's verdict and the
    /// scaffolded words — and it says out loud that the draft is a starting
    /// point, because nobody here has read the reply it answers.
    #[test]
    fn outreach_reply_briefing_shows_who_and_what_waits() {
        let briefing = AutopilotActionPayload::RequestOutreachReply {
            target_id: OutreachTargetId::new(),
            target_version: 2,
            target_name: "Dobrze Rockują".to_owned(),
            reply_interaction_id: 41,
            reply_disposition: "positive".to_owned(),
            sheet_verdict: Some("POSITIVE".to_owned()),
            draft: crowdrelay_domain::outreach_letter::OutreachLetter {
                subject: "Re: VIRYA".to_owned(),
                body: "Hi Dobrze Rockują,\n\nThank you —".to_owned(),
            },
        }
        .briefing();

        assert_eq!(briefing.summary, "Answer Dobrze Rockują's reply");
        let field = |label: &str| {
            briefing
                .content
                .iter()
                .find(|f| f.label == label)
                .map(|f| f.value.as_str())
        };
        assert_eq!(field("Their answer"), Some("positive"));
        assert_eq!(field("Sheet verdict"), Some("POSITIVE"));
        assert_eq!(field("Subject"), Some("Re: VIRYA"));
        assert!(
            briefing.why_it_matters.contains("edit"),
            "the briefing must say the scaffold needs editing"
        );
    }

    /// The wave is the batch form of the same ask: third-party class, no
    /// standing-approval target (a card naming several counterparties can
    /// never be covered by a per-target grant), and a briefing that shows
    /// every letter rather than a count of drafts.
    #[test]
    fn approach_wave_is_one_card_of_season_letters() {
        let wave = uuid::Uuid::now_v7();
        let agent = BookingAgentId::new();
        let payload = AutopilotActionPayload::RequestBookingAgentApproachWave {
            wave_id: wave,
            note: None,
            evidence: AgentDrawEvidence::default(),
            approaches: vec![
                BookingAgentApproachDraft {
                    agent_id: agent,
                    agent_version: 3,
                    agent_name: "Nuclear Blast".to_owned(),
                    agency: Some("NB Agency".to_owned()),
                    draft: crowdrelay_domain::approach_letter::ApproachLetter {
                        subject: "Representation".to_owned(),
                        body: "the season".to_owned(),
                    },
                },
                BookingAgentApproachDraft {
                    agent_id: BookingAgentId::new(),
                    agent_version: 1,
                    agent_name: "Solo Agent".to_owned(),
                    agency: None,
                    draft: crowdrelay_domain::approach_letter::ApproachLetter {
                        subject: "Representation".to_owned(),
                        body: "the season".to_owned(),
                    },
                },
            ],
        };
        assert_eq!(payload.action_kind(), "booking_agent.approach_wave.request");
        assert_eq!(payload.action_class(), ActionClass::ThirdParty);
        assert_eq!(payload.standing_approval_target(), None);

        let encoded = serde_json::to_value(&payload).expect("wave encodes");
        assert_eq!(encoded["kind"], "request_booking_agent_approach_wave");
        let decoded: AutopilotActionPayload =
            serde_json::from_value(encoded).expect("wave round-trips");
        assert_eq!(decoded, payload);

        let briefing = payload.briefing();
        assert_eq!(briefing.summary, "Apply to 2 booking agents");
        assert!(
            briefing
                .content
                .iter()
                .any(|field| field.label == "Nuclear Blast — body"),
            "the briefing must show each letter, not just the count"
        );
    }

    /// Outreach payloads queued before the letter rode along must still
    /// parse — the draft field is serde-defaulted and decodes empty, which
    /// dispatch then refuses rather than composing on the band's behalf.
    #[test]
    fn outreach_payload_without_draft_still_parses() -> Result<(), Box<dyn std::error::Error>> {
        let legacy = serde_json::json!({
            "kind": "request_outreach",
            "opportunity_id": uuid::Uuid::now_v7(),
            "target_id": uuid::Uuid::now_v7(),
            "target_version": 1,
            "target_name": "Metal Playlists Weekly",
            "phase": "initial",
            "template_key": "outreach.press.v1",
        });
        let AutopilotActionPayload::RequestOutreach { draft, .. } = serde_json::from_value(legacy)?
        else {
            panic!("the legacy payload must still parse as RequestOutreach")
        };
        assert!(draft.subject.is_empty() && draft.body.is_empty());
        Ok(())
    }

    /// Approach payloads queued before the letter rode along must still
    /// parse — the draft deserializes empty and dispatch refuses it rather
    /// than composing after the approval.
    #[test]
    fn approach_payloads_without_draft_still_parse() -> Result<(), Box<dyn std::error::Error>> {
        let AutopilotActionPayload::RequestRepresentationApproach { draft, .. } =
            serde_json::from_value(serde_json::json!({
                "kind": "request_representation_approach",
                "target_id": uuid::Uuid::now_v7(),
                "target_version": 1,
                "target_name": "Agent X",
            }))?
        else {
            panic!("the legacy payload must still parse as RequestRepresentationApproach")
        };
        assert!(draft.subject.is_empty() && draft.body.is_empty());
        let AutopilotActionPayload::RequestBookingAgentApproach { draft, .. } =
            serde_json::from_value(serde_json::json!({
                "kind": "request_booking_agent_approach",
                "agent_id": uuid::Uuid::now_v7(),
                "agent_version": 1,
                "agent_name": "Agent Y",
                "evidence": {},
            }))?
        else {
            panic!("the legacy payload must still parse as RequestBookingAgentApproach")
        };
        assert!(draft.subject.is_empty() && draft.body.is_empty());
        Ok(())
    }

    /// Application payloads queued before the letter rode along must still
    /// parse — the draft deserializes empty and dispatch refuses it rather
    /// than composing after the approval.
    #[test]
    fn apply_payload_without_draft_still_parses() -> Result<(), Box<dyn std::error::Error>> {
        let AutopilotActionPayload::ApplyLiveOpportunity { draft, .. } =
            serde_json::from_value(serde_json::json!({
                "kind": "apply_live_opportunity",
                "opportunity_id": uuid::Uuid::now_v7(),
                "opportunity_kind": "festival",
                "score": 42,
            }))?
        else {
            panic!("the legacy payload must still parse as ApplyLiveOpportunity")
        };
        assert!(draft.subject.is_empty() && draft.body.is_empty());
        Ok(())
    }
}
