// The reply-rescue candidate: a person answered the band's outreach and
// nobody has written back — the warmest thing on the board. The decision
// is pinned RequireApproval on purpose: an answer to a person's words is
// the one place a composed-by-machine letter can do real damage, so the
// scaffold is always a draft for a human, never a send.

/// One reply draft per conversation still waiting on the band.
///
/// The disposition is pinned to `RequireApproval` no matter the policy's
/// autonomy: the system never read the reply's actual words — the sheet
/// recorded a verdict — so the draft it emits is a scaffold the operator
/// completes against the real thread. A `bounded_auto` posture that
/// auto-sent it would be sending a guess at what they said, which is the
/// one failure a reply lane must never commit.
fn reply_rescue_candidate(
    snapshot: crowdrelay_domain::reply_rescue::UnansweredReplySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    use crowdrelay_domain::reply_rescue::{ReplyRescueDecision, evaluate_reply_rescue};

    let AutopilotPolicyConfig::Outreach(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let ReplyRescueDecision::Request { confidence } =
        evaluate_reply_rescue(&snapshot, now)
    else {
        return Ok(None);
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::OutreachTarget(snapshot.target_id),
        decision_kind: "request_outreach_reply",
        confidence,
        // Pinned — see the fn comment. An answer to a person always shows
        // its words to a human first.
        disposition: PolicyDisposition::RequireApproval,
        reason: "their reply has been waiting unanswered — draft an answer for review",
        input_snapshot: serde_json::to_value(&snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestOutreachReply {
            target_id: snapshot.target_id,
            target_version: snapshot.target_version,
            target_name: snapshot.target_name.clone(),
            reply_interaction_id: snapshot.interaction_id,
            reply_disposition: snapshot.reply_disposition.as_str().to_owned(),
            sheet_verdict: snapshot.sheet_verdict.clone(),
            // Composed when the action persists — the evaluator is pure and
            // the sender identity and pitch live in Postgres.
            draft: crowdrelay_domain::outreach_letter::OutreachLetter::default(),
        },
        decision_key: format!(
            "decision:outreach-reply:v{}:{}:{}",
            policy.version, snapshot.target_id, snapshot.interaction_id
        ),
        // One answer per conversation, and the target version is part of the
        // key for the same reason the booking lanes carry it: a dispatch that
        // fails terminally on a stale version (the sheet importer bumps it on
        // every recorded send) must not strand the reply — the next cycle
        // re-mints the card under the new version.
        action_idempotency_key: format!(
            "action:outreach-reply:{}:tv{}",
            snapshot.interaction_id, snapshot.target_version
        ),
    }))
}

