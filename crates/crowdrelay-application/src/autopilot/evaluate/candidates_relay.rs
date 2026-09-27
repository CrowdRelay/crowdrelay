// Relay fan-out for synced band posts (2.11), split out of `candidates.rs`:
// one push to the owned audience plus one community post per admitted
// community. Lives with the candidate builders — it emits the same
// `DecisionCandidate` shape, just more than one per post.

/// Longest push title and body a relay may send, in characters. The same
/// bounds the agents service holds its own push drafts to (`PUSH_TITLE_MAX`,
/// `PUSH_BODY_MAX` in crowdrelay-agents `human-text.ts`): a lock screen shows
/// a line or two, and everything past that is text nobody reads.
const RELAY_PUSH_TITLE_MAX: usize = 80;
const RELAY_PUSH_BODY_MAX: usize = 200;

/// A caption written for Instagram, reduced to what a lock screen carries.
///
/// The relay used to paste the whole caption. On 2026-09-18 fans got a push
/// whose body was a photo credit (`📷 @neohutsul_photo`), a venue handle, and
/// `#virya #modernmetal #polishmetal #livemusic #wroclaw` — words for a feed
/// algorithm, not for a person reading a notification. Hashtags and
/// `@handles` are dropped, lines left with no letters or digits go with them,
/// and the rest is cut at a word boundary to fit beside the permalink. The
/// band's own sentences are kept as written; nothing is added.
fn lock_screen_body(caption: &str, url: Option<&str>) -> String {
    let room = url.map_or(RELAY_PUSH_BODY_MAX, |u| {
        RELAY_PUSH_BODY_MAX.saturating_sub(u.chars().count() + 2)
    });
    let text = lock_screen_line(&strip_feed_markup(caption), room);
    match (text.is_empty(), url) {
        (false, Some(url)) => format!("{text}\n\n{url}"),
        (true, Some(url)) => url.to_owned(),
        (_, None) => text,
    }
}

/// Removes hashtags and `@handles`, drops lines that are left with no letter
/// or digit (an emoji, a separator, a bare credit mark), and joins what is
/// left with single spaces.
fn strip_feed_markup(caption: &str) -> String {
    caption
        .lines()
        .map(|line| {
            line.split_whitespace()
                .filter(|word| !word.starts_with('#') && !word.starts_with('@'))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|line| line.chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` on one line, cut at the last word boundary that fits `max`
/// characters, with an ellipsis when anything was cut.
fn lock_screen_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    for word in flat.split(' ') {
        let next = if out.is_empty() { word.chars().count() } else { out.chars().count() + 1 + word.chars().count() };
        if next > budget {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        out = flat.chars().take(budget).collect();
    }
    out.push('…');
    out
}

#[cfg(test)]
mod relay_push_text_tests {
    use super::{lock_screen_body, lock_screen_line};

    #[test]
    fn the_caption_that_reached_fans_on_2026_09_18_loses_its_feed_markup() {
        let caption = "WROCŁAW // 11.09 // ŁĄCZNIK \n\nKilka klatek z tego, co wydarzyło się na scenie.\n\nDzięki wszystkim, którzy byli z nami pod sceną.\n\n📷 @neohutsul_photo\n\nNext stop: Gorzów Wielkopolski // 17.10 @klub_magnetoffon\n\n#virya #modernmetal #polishmetal #livemusic #wroclaw";
        let url = "https://www.instagram.com/p/DdZSxWZCLhR/";
        let body = lock_screen_body(caption, Some(url));
        assert!(!body.contains('#'), "{body}");
        assert!(!body.contains('@'), "{body}");
        assert!(!body.contains('📷'), "{body}");
        assert!(body.contains("Dzięki wszystkim"), "{body}");
        assert!(body.ends_with(url), "{body}");
        assert!(body.chars().count() <= 200, "{} chars: {body}", body.chars().count());
    }

    #[test]
    fn a_short_caption_is_left_as_the_band_wrote_it() {
        assert_eq!(
            lock_screen_body("Gramy 17.10 w Gorzowie.", None),
            "Gramy 17.10 w Gorzowie."
        );
    }

    #[test]
    fn a_long_line_is_cut_at_a_word_with_an_ellipsis() {
        let cut = lock_screen_line("jeden dwa trzy cztery pięć", 12);
        assert_eq!(cut, "jeden dwa…");
        assert!(cut.chars().count() <= 12);
    }
}

/// The relay fan-out for one synced band post: one push to the owned
/// audience, plus one community post per admitted community. No version in
/// either key — a caption edit bumps the source version, and re-relaying an
/// edited post would put the same news in front of the same people twice.
#[allow(clippy::too_many_arguments)]
fn relay_candidates(
    snapshot: &ContentSupplySnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &crowdrelay_domain::content_supply::ContentSupplyPolicy,
    communities: &[CommunityRelayTarget],
    push_audience: Option<crowdrelay_domain::content_supply::SignalPushAudience>,
    confidence: crowdrelay_domain::autonomy::Confidence,
    evidence: ContextEvidence,
    now: OffsetDateTime,
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
    let body = lock_screen_body(
        post.body.as_deref().unwrap_or(post.title.as_str()),
        post.url.as_deref(),
    );
    // A post about "tonight" is not pushed once the night has passed — see
    // `relative_day_has_passed`. The communities below still get it: their
    // drafts are rewritten from the caption's facts, not pasted.
    let stale_push = crowdrelay_domain::relay_freshness::relative_day_has_passed(
        &format!("{} {}", post.title, post.body.as_deref().unwrap_or_default()),
        snapshot.occurred_at,
        now,
    );
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
            title: lock_screen_line(&post.title, RELAY_PUSH_TITLE_MAX),
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
    if stale_push {
        out.clear();
    }

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
    //
    // Only posts that landed at home go further: the owned push above carries
    // every post; communities get the ones the band's followers answered
    // (`resonates_for_communities`). The keys carry no version, so a post
    // that earns it on a later cycle fans out then, once.
    let communities = if crowdrelay_domain::content_supply::resonates_for_communities(
        post,
        snapshot.occurred_at,
        now,
    ) {
        communities
    } else {
        &[]
    };
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

