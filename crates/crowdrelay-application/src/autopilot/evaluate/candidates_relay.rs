// Relay fan-out for synced band posts (2.11), split out of `candidates.rs`:
// one push to the owned audience plus one community post per admitted
// community. Lives with the candidate builders — it emits the same
// `DecisionCandidate` shape, just more than one per post.

/// The relay fan-out for one synced band post: one push to the owned
/// audience, plus one community post per admitted community. No version in
/// either key — a caption edit bumps the source version, and re-relaying an
/// edited post would put the same news in front of the same people twice.
fn relay_candidates(
    snapshot: &ContentSupplySnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &crowdrelay_domain::content_supply::ContentSupplyPolicy,
    communities: &[CommunityRelayTarget],
    push_audience: Option<crowdrelay_domain::content_supply::SignalPushAudience>,
    confidence: crowdrelay_domain::autonomy::Confidence,
    evidence: ContextEvidence,
) -> Result<Vec<DecisionCandidate>, serde_json::Error> {
    let Some(post) = &snapshot.social_post else {
        return Ok(Vec::new());
    };
    let disposition = disposition_with_evidence(
        policy.autonomy_level,
        confidence,
        policy.minimum_confidence,
        evidence,
        RATE_FLOOR,
    );
    let input_snapshot = serde_json::to_value(snapshot)?;
    let policy_snapshot = policy_evidence(policy, domain_policy)?;
    let source = snapshot.source_id.into_uuid();
    let platform = post.platform.as_str();

    // Owned channel first: the post's own title, its permalink in the body so
    // the push carries the band's words to fans who opted in. `task_id` is the
    // source id — no agent task produced this, and the dispatch-prediction
    // ref should point at what did.
    let body = match (&post.body, &post.url) {
        (Some(caption), Some(url)) => format!("{caption}\n\n{url}"),
        (Some(caption), None) => caption.clone(),
        (None, Some(url)) => format!("{}\n\n{url}", post.title),
        (None, None) => post.title.clone(),
    };
    let mut out = vec![DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::ContentSource(snapshot.source_id),
        decision_kind: "relay_owned_post",
        confidence,
        disposition,
        reason: "band published a post on an owned account — carried to fans who opted in",
        input_snapshot: input_snapshot.clone(),
        policy_snapshot: policy_snapshot.clone(),
        action: AutopilotActionPayload::RequestSignalPush {
            task_id: source,
            title: post.title.clone(),
            body,
            target_path: None,
            event_id: None,
            segment: None,
            // The approval quotes what the send would deliver today: every
            // consented fan with a live push endpoint, after the workspace's
            // per-step envelope bound. When the count ran short of the
            // eligible set the basis says so rather than letting the smaller
            // number read as the whole audience.
            audience_size: push_audience.map(|audience| audience.reached),
            audience_basis: push_audience.map_or_else(String::new, |audience| {
                if audience.reached < audience.eligible {
                    format!(
                        "fans with notifications on who consented to marketing — the workspace's per-step send envelope caps this push at {}",
                        audience.reached
                    )
                } else {
                    "fans with notifications on who consented to marketing".to_owned()
                }
            }),
        },
        decision_key: format!("decision:relay:v{}:{source}:signal_push", policy.version),
        action_idempotency_key: format!("action:relay:{source}:signal_push"),
    }];

    // Admitted communities: one drafting task per (post × community). The
    // caption used to be pasted verbatim with an English attribution line —
    // a Polish caption plus an English tail on a Polish subreddit read as
    // exactly what it was, a bot reposting itself. Now the repost worker
    // writes the post in the community's own language from the caption's
    // facts (and carries the post's original media — attached by the worker
    // outcome mapping from the source row, never by the model). The drafted
    // text lands on the operator's approval queue as the composed post, not
    // a promise of one.
    //
    // A community not on the admitted list never appears here, and the
    // executor re-checks admission at post time.
    for target in communities {
        let target_id = target.target_id.into_uuid();
        let caption_facts = post.body.as_deref().unwrap_or(post.title.as_str());
        let permalink = post.url.as_deref().unwrap_or("(none)");
        let media_desc = match post.media_type.as_deref() {
            Some("IMAGE") => "a photo (the original picture will be attached)",
            Some("VIDEO") => "a video (its still frame will be attached)",
            Some("CAROUSEL_ALBUM") => "a photo carousel (its first image will be attached)",
            _ if post.media_url.is_some() => "a photo (the original picture will be attached)",
            _ => "no media — the post will be a link post to the source",
        };
        let language = target
            .language
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("(not recorded — infer it from the community's own description)");
        let prompt = format!(
            "Draft one Reddit post carrying the band's own social post into r/{}.\n\n\
            Source post (the ONLY facts you may use — the caption is the band's own words):\n\
            - platform: {platform}\n\
            - caption: {caption_facts}\n\
            - permalink: {permalink}\n\
            - media: {media_desc}\n\
            - source_id: {source}\n\n\
            Target community:\n\
            - target_id: {target_id}\n\
            - subreddit: r/{}\n\
            - language: {language}",
            target.subreddit, target.subreddit
        );
        // The drafting dispatch runs the workspace's autonomy level WITHOUT
        // the evidence floor, upgraded for internal work. The floor exists to
        // stop unattended action a tenant cannot take back on an untested
        // estimator — but this action only queues a draft: the only
        // irreversible step is the post itself, and that gates later, on the
        // outcome's own `require_approval` action, with the composed text and
        // the picture in front of the operator. Gating the dispatch did not
        // protect anything; it parked an "approve the agent run" card in
        // front of a prompt nobody can judge, and the real decision came
        // later anyway.
        let draft_disposition = crowdrelay_domain::autonomy::internal_work_disposition(
            crowdrelay_domain::autonomy::disposition(
                policy.autonomy_level,
                confidence,
                policy.minimum_confidence,
            ),
        );
        out.push(DecisionCandidate {
            context: policy.context,
            // The community is the subject, not the post: the inflight
            // subject index would otherwise read N community candidates for
            // one post as N conflicts of the first, and a post meant for
            // every admitted community would reach exactly one.
            subject: ActionSubject::TargetCommunity(target_id),
            decision_kind: "relay_owned_post",
            confidence,
            disposition: draft_disposition,
            reason: "band's own post drafted for a community the workspace was admitted to",
            input_snapshot: input_snapshot.clone(),
            policy_snapshot: policy_snapshot.clone(),
            action: AutopilotActionPayload::RequestAgentRun {
                template_id: "community-repost".to_owned(),
                prompt,
                priority: 2,
                // A caption adaptation, not a judgment call — the free-tier
                // route is the right spend for it.
                tier: crowdrelay_brain::AgentTier::Basic,
            },
            decision_key: format!(
                "decision:relay:v{}:{source}:community:{target_id}",
                policy.version
            ),
            action_idempotency_key: format!("action:relay:{source}:community:{target_id}"),
        });
    }
    Ok(out)
}

