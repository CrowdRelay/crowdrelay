// Community identity and screening for `include!`d into `agent_outcomes.rs`
// — shares that module's scope (see quality_guard.rs for the same split).
//
// A community's identity is its subreddit or, off Reddit, its URL — never
// its display name. Everything in this file exists to land one row per real
// community, filed under the audience-graph place the join executor reads,
// screened before the growth loop may post there.

/// The place_kind vocabulary a community's `platform` field may name. The
/// agents template binds to this list; anything else refuses rather than
/// guessing a kind.
const COMMUNITY_PLACE_KINDS: [&str; 14] = [
    "subreddit",
    "discord",
    "telegram",
    "lemmy",
    "forum",
    "facebook_group",
    "instagram",
    "tiktok",
    "youtube",
    "playlist",
    "zine",
    "festival",
    "x_account",
    "other",
];

/// What a community proposal's identity fields normalize into — the exact
/// triple the row stores. Building it as one value keeps the three columns
/// consistent: a community participates in exactly one dedup index, so the
/// moment a subreddit wins, the URL must not be stored beside it.
struct CommunityIdentity {
    /// The normalized subreddit slug — `None` when the community is not a
    /// subreddit or the field held nothing reddit could have issued.
    subreddit: Option<String>,
    /// The community's URL, `None` once a subreddit carried the identity.
    community_url: Option<String>,
    /// 'reddit' whenever a subreddit won — a subreddit is a reddit place
    /// whatever the model labelled it — else the platform as proposed.
    platform: Option<String>,
}

/// Reads a target item's community identity into the stored triple.
///
/// `subreddit` is normalized the way `normalize_subreddit()` does —
/// lowercase, one leading `r/` or `/r/` stripped — and then checked against
/// reddit's own name rules (the same `[a-z0-9_]{1,21}` bound
/// `reddit_subreddit_name` applies to URLs). A slug reddit could not have
/// issued is not an identity: the proposal falls to URL identity or refusal
/// instead of filing a normalized-but-impossible slug.
///
/// A `community_url` that names a subreddit IS a subreddit however the
/// model labelled it — folding it keeps one row per community even when the
/// proposal arrives URL-shaped. The roundtrip is the validity test:
/// `canonical_place_url` returns its input unchanged for a URL it does not
/// recognise, so a 'slug' that cannot canonicalise itself never was one.
fn community_identity(item: &Value) -> CommunityIdentity {
    let platform = item
        .get("platform")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.chars().count() <= 64);
    // A community_url is only an identity if it is a real fetchable web
    // address: an unparseable string, a non-http(s) scheme, or a host that
    // cannot fit contact_domain's CHECK is not a joinable community — the
    // proposal falls to the no-address refusal instead of storing a row
    // that says "promoted" beside a place that was never filed.
    let community_url = item
        .get("community_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| u.chars().count() <= 512)
        .and_then(|u| {
            url::Url::parse(u).ok().and_then(|p| {
                let http = p.scheme() == "http" || p.scheme() == "https";
                let host_ok = p
                    .host_str()
                    .is_some_and(|h| !h.is_empty() && h.chars().count() <= 200);
                (http && host_ok).then_some(u)
            })
        });
    let field_subreddit = item.get("subreddit").and_then(Value::as_str).and_then(|s| {
        let lower = s.trim().to_lowercase();
        let stripped = lower
            .strip_prefix("/r/")
            .or_else(|| lower.strip_prefix("r/"))
            .unwrap_or(&lower);
        let valid = !stripped.is_empty()
            && stripped.len() <= 21
            && stripped
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        valid.then(|| stripped.to_owned())
    });
    let url_subreddit = community_url.and_then(|u| {
        let canonical = canonical_place_url(u);
        let slug = canonical.strip_prefix("https://www.reddit.com/r/")?;
        (canonical_place_url(&format!("r/{slug}")) == canonical).then(|| slug.to_owned())
    });
    let subreddit = field_subreddit.or(url_subreddit);
    if subreddit.is_some() {
        CommunityIdentity {
            subreddit,
            community_url: None,
            platform: Some("reddit".to_owned()),
        }
    } else {
        CommunityIdentity {
            subreddit: None,
            community_url: community_url.map(str::to_owned),
            platform: platform.map(str::to_owned),
        }
    }
}

/// The `discovery_places.platform` column holds the short platform name
/// ('reddit', 'discord'), where place kinds like `facebook_group` name the
/// room, not the service. Mapping keeps the place's `(workspace, platform,
/// url)` identity consistent with what discovery writes.
fn community_platform(place_kind: &str) -> &str {
    match place_kind {
        "subreddit" => "reddit",
        "facebook_group" => "facebook",
        "x_account" => "x",
        other => other,
    }
}

impl AgentOutcomeWorker {
    /// Resolves the audience-graph place for a proposed community and screens
    /// it, returning (place_id, screening verdict, refusal reason).
    ///
    /// A subreddit or URL discovery has never seen still files as a place —
    /// "add as a fanbase to join" needs the row the join executor reads, not
    /// only a promoted target. Communities with no joinable identity at all
    /// still land a refused row: the verdict is the record that stops the
    /// same dead proposal returning every week.
    async fn screen_community_target(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        display_name: &str,
        identity: &CommunityIdentity,
        evidence: &Value,
        language: Option<&str>,
    ) -> Result<(Option<Uuid>, Option<&'static str>, Option<&'static str>), sqlx::Error> {
        // place_kind the audience graph files this under. Reddit keeps its
        // own branch because the existing discovery rows key on the subreddit
        // slug, not the URL — and the agents vocabulary calls the platform
        // "reddit" where the graph calls the kind "subreddit", so translate
        // before validating.
        let place_kind = identity.platform.as_deref().and_then(|p| {
            let kind = if p == "reddit" { "subreddit" } else { p };
            COMMUNITY_PLACE_KINDS.contains(&kind).then_some(kind)
        });
        let place = if let Some(subreddit) = identity.subreddit.as_deref() {
            match community_place(tx, workspace_id, Some(subreddit)).await? {
                Some(place) => Some(place),
                None => {
                    // canonical_place_url only rewrites a URL that names a
                    // real subreddit — a slug reddit could not have issued
                    // comes back unchanged and files no place, which is the
                    // same screened-but-unjoinable shape this branch had
                    // before fresh places existed.
                    let derived = format!("r/{subreddit}");
                    let canonical = canonical_place_url(&derived);
                    if canonical != derived {
                        ensure_community_place(
                            tx,
                            workspace_id,
                            "subreddit",
                            "reddit",
                            display_name,
                            &canonical,
                            language,
                        )
                        .await?
                    } else {
                        None
                    }
                }
            }
        } else if let Some(url) = identity.community_url.as_deref() {
            match place_kind {
                // A reddit URL that reached here is not a /r/ link — real
                // subreddit URLs folded into subreddit identity above. A
                // user profile or post URL names no joinable community, so
                // it refuses rather than filing a 'subreddit' place that is
                // not one.
                Some("subreddit") => None,
                Some(kind) => {
                    match community_place_by_url(tx, workspace_id, community_platform(kind), url)
                        .await?
                    {
                        Some(place) => Some(place),
                        None => {
                            ensure_community_place(
                                tx,
                                workspace_id,
                                kind,
                                community_platform(kind),
                                display_name,
                                url,
                                language,
                            )
                            .await?
                        }
                    }
                }
                // A community URL without a known platform cannot be filed
                // or deduped honestly — refuse with the reason rather than
                // guess a place_kind.
                None => None,
            }
        } else {
            None
        };
        let no_url = identity.community_url.is_none();
        let unscreenable = identity.subreddit.is_none() && no_url;
        // A reddit.com URL that is not a /r/ link is the same refusal as
        // having no address: the link exists but names no community.
        let not_a_community_url =
            identity.subreddit.is_none() && !no_url && place_kind == Some("subreddit");
        let unsupported_platform =
            identity.subreddit.is_none() && !no_url && place_kind.is_none();
        if unscreenable || not_a_community_url {
            Ok((None, Some("refused"), Some("no_joinable_address")))
        } else if unsupported_platform {
            Ok((None, Some("refused"), Some("unsupported_platform")))
        } else {
            let snapshot = community_snapshot(evidence, place.as_ref());
            Ok(match screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()) {
                ScreeningVerdict::Admit { .. } => {
                    (place.map(|p| p.id), Some("admitted"), None)
                }
                ScreeningVerdict::Refuse(reason) => {
                    (place.map(|p| p.id), Some("refused"), Some(reason.as_str()))
                }
            })
        }
    }
}
