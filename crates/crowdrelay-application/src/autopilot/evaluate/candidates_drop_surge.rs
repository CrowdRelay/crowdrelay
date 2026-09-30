// Drop-surge fan-out for fresh videos and releases, split out of
// `candidates.rs`: the moment a new video lands in the supply, every owned
// lane gets its own idempotent action instead of waiting on the generic
// posting cadence — the failure this lane closes was measured on
// 2026-09-28, when a premiere got fifty-nine artifact requests and zero
// fan-facing posts in its first day.
//
// The copy is composed here, in deterministic Rust, from the source's own
// title and description — no model call, no fabricated facts, and the
// model-text approval gate does not apply because no model wrote a word.
// Outward lanes still answer the workspace's posture, confidence floor and
// per-class hold like every other action.

/// Confidence a surge candidate reports. The eligibility check is boolean —
/// the source either just dropped or it did not — so the number states
/// "deterministic rule, not an estimator's guess" rather than pretending a
/// measurement exists.
const DROP_SURGE_CONFIDENCE: u16 = 9_500;

/// A channel draft's caption budget, in characters. The executor appends
/// the tracked link itself; the text leaves room for it.
const SURGE_CAPTION_MAX: usize = 800;
/// X's 280-character body has to hold caption and link together.
const SURGE_CAPTION_X_MAX: usize = 230;

/// The `agent_service_tasks.template_id` each channel lane's synthetic task
/// row carries — the value the channel executors join on to claim work.
fn surge_template_id(lane: &str) -> Option<&'static str> {
    match lane {
        "telegram" => Some("telegram-poster"),
        "discord" => Some("discord-poster"),
        "instagram" | "facebook" | "x" => Some("social-post"),
        _ => None,
    }
}

/// The caption a channel draft carries: the band's own title and
/// description with feed markup stripped, bounded for the platform. The
/// tracked `/l/` link is deliberately not in the text — the executors mint
/// and append it, which is what makes the click countable.
fn surge_caption(snapshot: &ContentSupplySnapshot, max: usize) -> String {
    let title = snapshot.title.trim();
    let body = snapshot
        .source_body
        .as_deref()
        .map(strip_feed_markup)
        .unwrap_or_default();
    let text = if body.is_empty() {
        title.to_owned()
    } else if title.is_empty() {
        body
    } else {
        format!("{title}\n\n{body}")
    };
    lock_screen_line(&text, max)
}

/// The retry suffix once this source and destination may try again.
/// `None` means backoff or the existing attempt ceiling holds this lane.
fn surge_retry(
    snapshot: &ContentSupplySnapshot,
    lane: &str,
    now: OffsetDateTime,
) -> Option<String> {
    let Some(failure) = snapshot.drop_surge_failures.iter().find(|failure| failure.lane == lane) else {
        return Some(String::new());
    };
    if failure.failures >= crowdrelay_domain::content_supply::DROP_SURGE_MAX_ATTEMPTS
        || now < failure.retry_due()
    {
        return None;
    }
    Some(format!(":attempt{}", failure.failures))
}

/// The drop fan-out for one fresh video or release: one candidate per lane,
/// each keyed `action:drop_surge:{source}:{lane}` so a provider outage in
/// Telegram retries Telegram alone and the channels that already posted are
/// never touched twice.
///
/// Lanes that exhaust [`DROP_SURGE_MAX_ATTEMPTS`] stop — a channel that
/// keeps failing gets its silence rather than a spam loop. The community
/// lane is a `community-engager` dispatch, not a post: its drafts come back
/// as their own approval-gated actions, in each community's language.
#[allow(clippy::too_many_arguments)]
fn drop_surge_candidates(
    snapshot: &ContentSupplySnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &crowdrelay_domain::content_supply::ContentSupplyPolicy,
    communities: &[CommunityRelayTarget],
    push_audience: Option<crowdrelay_domain::content_supply::SignalPushAudience>,
    now: OffsetDateTime,
) -> Result<Vec<DecisionCandidate>, serde_json::Error> {
    if !crowdrelay_domain::content_supply::drop_surge_eligible(snapshot, domain_policy, now) {
        return Ok(Vec::new());
    }
    let input_snapshot = serde_json::to_value(snapshot)?;
    let policy_snapshot = policy_evidence(policy, domain_policy)?;
    let source = snapshot.source_id.into_uuid();
    // Outward lanes answer the normal gate — posture and confidence floor,
    // the same `disposition` every other content-supply candidate takes.
    // At bounded-auto they execute on their own; under require_approval
    // they park with the copy in front of the operator, which is exactly
    // what that posture asks for. An extra evidence floor is *not* applied
    // here: the sibling artifact lane does not carry one either, and a
    // drop that waits on observed history has already lost its window.
    let confidence = crowdrelay_domain::autonomy::Confidence::saturating_from_basis_points(
        DROP_SURGE_CONFIDENCE,
    );
    let outward = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);

    let mut out = Vec::new();
    for &lane in crowdrelay_domain::content_supply::DROP_SURGE_LANES {
        if snapshot
            .promotion_excluded_platforms
            .iter()
            .any(|excluded| excluded == lane)
        {
            continue;
        }
        if lane == "community" {
            if snapshot.source_kind != crowdrelay_domain::content_supply::ContentSourceKind::Video {
                continue;
            }
            for target in communities.iter().filter(|target| {
                !snapshot
                    .promotion_excluded_platforms
                    .contains(&target.platform)
            }) {
                let target_id = target.target_id.into_uuid();
                let Some(retry) = surge_retry(snapshot, &format!("community:{target_id}"), now) else {
                    continue;
                };
                let address = if target.platform == "reddit" {
                    format!("r/{}", target.subreddit)
                } else {
                    target
                        .community_url
                        .clone()
                        .unwrap_or_else(|| target.subreddit.clone())
                };
                out.push(DecisionCandidate {
                    context: policy.context,
                    subject: ActionSubject::TargetCommunity(target_id),
                    decision_kind: "drop_surge_fanout",
                    confidence,
                    disposition: crowdrelay_domain::autonomy::internal_work_disposition(outward),
                    reason: "fresh video drafted for one admitted community; publication requires approval",
                    input_snapshot: input_snapshot.clone(),
                    policy_snapshot: policy_snapshot.clone(),
                    action: AutopilotActionPayload::RequestAgentRun {
                        template_id: "community-engager".to_owned(),
                        prompt: format!(
                            "Draft exactly one post for the named community. No other targets or sources.\n\n\
                             source_id: {source}\ntarget_id: {target_id}\nplatform: {}\naddress: {address}\n\
                             language: {}\n\nSource title: {}\nSource URL: {}\nDescription: {}\n\
                             Use only source facts. Follow the community's verified rules.\n\
                             Missing credentials or a manual-only platform means a draft for a person, not permission to publish.",
                            target.platform, target.language.as_deref().unwrap_or("not recorded"),
                            snapshot.title, snapshot.source_url.as_deref().unwrap_or(""),
                            surge_caption(snapshot, 400),
                        ),
                        priority: 1,
                        tier: crowdrelay_brain::AgentTier::Basic,
                    },
                    decision_key: format!("decision:drop_surge:v{}:{source}:community:{target_id}{retry}", policy.version),
                    action_idempotency_key: format!("action:drop_surge:{source}:community:{target_id}{retry}"),
                });
            }
            continue;
        }
        let Some(retry) = surge_retry(snapshot, lane, now) else {
            continue;
        };
        // The email lane needs an absolute link — `cta_url` is relative.
        // Without the tenant's site origin there is nothing a reader can
        // click, so the lane waits for the setting rather than sending a
        // body with a hole where the link goes.
        if lane == "email" && snapshot.site_origin.is_none() {
            continue;
        }
        let Some(link_slug) =
            crowdrelay_domain::content_supply::drop_surge_link_slug(&snapshot.source_key, lane)
        else {
            continue;
        };
        let subject = crate::autopilot::drop_surge_subject(snapshot.source_id, lane);
        let decision_key = format!(
            "decision:drop_surge:v{}:{source}:{lane}{retry}",
            policy.version
        );
        let action_idempotency_key = format!("action:drop_surge:{source}:{lane}{retry}");
        let reason = "fresh drop — the first hours are where a new video earns its fans";
        let action = match lane {
            "signal_push" => AutopilotActionPayload::RequestSignalPush {
                task_id: source,
                title: lock_screen_line(snapshot.title.trim(), RELAY_PUSH_TITLE_MAX),
                body: surge_caption(snapshot, RELAY_PUSH_BODY_MAX),
                // `/l/{slug}` resolves on the member site straight into the
                // video — the tap is the click, so the tracked route counts.
                target_path: Some(format!("/l/{link_slug}")),
                event_id: None,
                segment: None,
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
                drop_surge_lane: Some(lane.to_owned()),
            },
            "email" => AutopilotActionPayload::RequestSourceCampaign {
                source_id: snapshot.source_id,
                template_key: "content.drop_surge.v1".to_owned(),
                draft: crowdrelay_domain::campaign_lifecycle::EventCampaignCopy {
                    subject: snapshot.title.trim().to_owned(),
                    body: format!(
                        "{}\n\nWatch: {}/l/{link_slug}",
                        surge_caption(snapshot, 1200),
                        snapshot.site_origin.as_deref().unwrap_or_default()
                    ),
                },
                audience_size: None,
                audience_basis: "fans who consented to marketing email".to_owned(),
            },
            _ => {
                // Channel lanes: telegram, discord, instagram, facebook, x —
                // one `agent.content.request` action each, exactly the shape
                // a model draft would land as. `task_id` is deterministic
                // (source + lane) and execution materializes the synthetic
                // completed task row the channel executors join on.
                let Some(template_id) = surge_template_id(lane) else {
                    continue;
                };
                let max = if lane == "x" {
                    SURGE_CAPTION_X_MAX
                } else {
                    SURGE_CAPTION_MAX
                };
                AutopilotActionPayload::RequestAgentContent {
                    template_id: Some(template_id.to_owned()),
                    task_id: crate::autopilot::drop_surge_task_uuid(snapshot.source_id, lane),
                    draft: serde_json::json!({
                        "platform": lane,
                        "text": surge_caption(snapshot, max),
                        "cta_url": format!("/l/{link_slug}"),
                        // `artifact_outcome` and friends join a post back to
                        // its source by exactly this field — carry it flat,
                        // not nested, so the measurements already written
                        // count these posts without a new join.
                        "source_id": source,
                        "drop_surge": {
                            "source_id": source,
                            "lane": lane,
                            "origin": "deterministic",
                        },
                    }),
                    recipient_email: None,
                    recipient_name: None,
                    recipient_target_id: None,
                }
            }
        };
        let disposition = outward;
        out.push(DecisionCandidate {
            context: policy.context,
            subject,
            decision_kind: "drop_surge_fanout",
            confidence,
            disposition,
            reason,
            input_snapshot: input_snapshot.clone(),
            policy_snapshot: policy_snapshot.clone(),
            action,
            decision_key,
            action_idempotency_key,
        });
    }
    Ok(out)
}
