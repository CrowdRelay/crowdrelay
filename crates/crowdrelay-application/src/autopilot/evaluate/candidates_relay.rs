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
) -> Result<Vec<DecisionCandidate>, serde_json::Error> {
    let Some(post) = &snapshot.social_post else {
        return Ok(Vec::new());
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
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

    // Admitted communities: the band's own words and link, attributed. A
    // community not on the admitted list never appears here, and the executor
    // re-checks admission at post time.
    for target in communities {
        let community_body = match (&post.body, &post.url) {
            (Some(caption), Some(url)) => {
                format!("{caption}\n\nOriginally posted on {platform}: {url}")
            }
            (Some(caption), None) => caption.clone(),
            (None, Some(url)) => {
                format!("{}\n\nOriginally posted on {platform}: {url}", post.title)
            }
            (None, None) => post.title.clone(),
        };
        let target_id = target.target_id.into_uuid();
        out.push(DecisionCandidate {
            context: policy.context,
            // The community is the subject, not the post: the inflight
            // subject index would otherwise read N community candidates for
            // one post as N conflicts of the first, and a post meant for
            // every admitted community would reach exactly one.
            subject: ActionSubject::TargetCommunity(target_id),
            decision_kind: "relay_owned_post",
            confidence,
            disposition,
            reason: "band's own post carried to a community the workspace was admitted to",
            input_snapshot: input_snapshot.clone(),
            policy_snapshot: policy_snapshot.clone(),
            action: AutopilotActionPayload::RequestCommunityEngagement {
                target_id,
                platform: "reddit".to_owned(),
                subreddit: Some(target.subreddit.clone()),
                title: post.title.clone(),
                body: community_body,
                smart_link: None,
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

