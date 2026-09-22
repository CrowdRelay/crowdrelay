// The strategy-consult evaluator: `include!`d into `agent_outcomes.rs` so
// it shares that module's scope (see quality_guard.rs for the same split).
//
// `strategy_proposals` outcomes are advice, not authority. Every proposal
// inside the item is judged here, deterministically, inside the outcome's
// own transaction: a proposal that makes sense is implemented through a
// bounded channel and lands an `accepted` verdict; one that does not lands
// `rejected` with the reason the consultant reads back next week — which is
// what makes a rejection teach instead of repeat.
//
// Nothing here trusts the model's bounds. Each action kind re-validates its
// parameters against the columns and policies they touch, and a proposal
// that fails validation is a rejected verdict — never a clamped write.

/// The action vocabulary the agents contract may emit (closed enum — a
/// proposal asking for anything else is rejected as unknown, not ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProposalAction {
    ProposeCommunity,
    Rescan,
    AdjustCadence,
    QueueScanQueries,
    SurfaceToOperator,
}

impl ProposalAction {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "propose_community" => Some(Self::ProposeCommunity),
            "rescan" => Some(Self::Rescan),
            "adjust_cadence" => Some(Self::AdjustCadence),
            "queue_scan_queries" => Some(Self::QueueScanQueries),
            "surface_to_operator" => Some(Self::SurfaceToOperator),
            _ => None,
        }
    }
}

/// A proposal's verdict before its row is written.
struct ProposalVerdict {
    verdict: &'static str,
    reason: String,
    implemented_as: Option<String>,
}

fn accepted(implemented_as: String) -> ProposalVerdict {
    ProposalVerdict {
        verdict: "accepted",
        reason: String::new(),
        implemented_as: Some(implemented_as),
    }
}

fn rejected(reason: impl Into<String>) -> ProposalVerdict {
    ProposalVerdict {
        verdict: "rejected",
        reason: reason.into(),
        implemented_as: None,
    }
}

/// Cadence bounds for `adjust_cadence`. The schema bounds 1–720h; the brain
/// keeps its own floor because nothing in the dispatch machinery is meant
/// to cycle faster than daily — a proposal asking for an hourly rescan of a
/// posting worker is spam-adjacent, and one asking past the schema's 720h
/// ceiling would fail the row's own bound anyway.
const CADENCE_FLOOR_HOURS: i64 = 24;
const CADENCE_CEILING_HOURS: i64 = 720;

/// Per-outcome bounds the wire schema declares — restated here because the
/// payload column is JSONB and nothing structurally stops a larger array
/// from arriving.
const MAX_PROPOSALS: usize = 25;
const MAX_QUERIES_PER_PROPOSAL: usize = 10;

impl AgentOutcomeWorker {
/// Evaluates every proposal inside a `strategy_proposals` outcome item and
/// writes one verdict row per proposal.
async fn evaluate_strategy_proposals(
    &self,
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    item: &Value,
) -> Result<(), AgentOutcomeError> {
    let Some(proposals) = item.get("proposals").and_then(Value::as_array) else {
        // The quality guard already refuses an item with no proposals array;
        // reaching here without one means a malformed row slipped between —
        // fail the outcome loudly rather than record nothing.
        return Err(OutcomeRejection::MissingProposalContent.into());
    };
    let headline = item.get("headline").and_then(Value::as_str).unwrap_or("");
    let detail = item.get("detail").and_then(Value::as_str).unwrap_or("");

    // The schema bounds the array at 10; the bound here is the one the
    // database sees, since a payload that reached the table raw is not the
    // schema's word. Overflow proposals still land a rejected verdict —
    // the consultant reads back that the extra asks never ran.
    for (index, proposal) in proposals.iter().enumerate().take(MAX_PROPOSALS) {
        let verdict = self.evaluate_one_proposal(tx, outcome, proposal).await?;
        // The stored proposal is the model's words verbatim plus the
        // outcome-level headline/detail it was filed under — the consultant
        // reads these back, so the record is what it said, not a summary.
        let record = json!({
            "headline": headline,
            "detail": detail,
            "proposal": proposal,
        });
        // The action column is CHECK-bounded at 64 chars and non-blank;
        // an oversized or whitespace-only action in the payload must land
        // as a bounded label, not a constraint violation that sinks every
        // sibling verdict.
        let action_label: String = proposal
            .get("action")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("unknown")
            .chars()
            .take(64)
            .collect();
        sqlx::query(
            r#"
            INSERT INTO agent_strategy_proposal_verdicts
                (workspace_id, outcome_id, proposal_index, action,
                 verdict, reason, proposal, implemented_as)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
            ON CONFLICT (outcome_id, proposal_index) DO UPDATE SET
                verdict = EXCLUDED.verdict,
                reason = EXCLUDED.reason,
                proposal = EXCLUDED.proposal,
                implemented_as = EXCLUDED.implemented_as
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(outcome.id)
        .bind(index as i32)
        .bind(&action_label)
        .bind(verdict.verdict)
        .bind(&verdict.reason)
        .bind(&record)
        .bind(verdict.implemented_as.as_deref())
        .execute(&mut **tx)
        .await?;
    }
    if proposals.len() > MAX_PROPOSALS {
        sqlx::query(
            r#"
            INSERT INTO agent_strategy_proposal_verdicts
                (workspace_id, outcome_id, proposal_index, action,
                 verdict, reason, proposal)
            VALUES ($1,$2,$3,'overflow','rejected',$4,'{}'::jsonb)
            ON CONFLICT (outcome_id, proposal_index) DO NOTHING
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(outcome.id)
        .bind(MAX_PROPOSALS as i32)
        .bind(format!(
            "only the first {MAX_PROPOSALS} proposals are evaluated — {} arrived",
            proposals.len()
        ))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// One proposal → one verdict. Acceptances implement through the bounded
/// channel the action names; rejections carry the reason.
async fn evaluate_one_proposal(
    &self,
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    proposal: &Value,
) -> Result<ProposalVerdict, AgentOutcomeError> {
    let action_raw = proposal
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(action) = ProposalAction::parse(action_raw) else {
        return Ok(rejected(format!(
            "unknown action {action_raw:?} — outside the closed proposal vocabulary"
        )));
    };
    match action {
        ProposalAction::ProposeCommunity => {
            self.evaluate_propose_community(tx, outcome, proposal).await
        }
        ProposalAction::Rescan => self.evaluate_rescan(tx, outcome, proposal).await,
        ProposalAction::AdjustCadence => {
            self.evaluate_adjust_cadence(tx, outcome, proposal).await
        }
        ProposalAction::QueueScanQueries => {
            self.evaluate_queue_scan_queries(tx, outcome, proposal).await
        }
        // An operator-facing recommendation's implementation is the verdict
        // row itself — the record the operator and the next consult read.
        ProposalAction::SurfaceToOperator => Ok(accepted("surfaced".to_owned())),
    }
}

/// `propose_community`: the community rides the same insert path a
/// fanbase-scout target would — screened, deduped on its platform identity,
/// recorded refused with the reason when screening says no.
async fn evaluate_propose_community(
    &self,
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    proposal: &Value,
) -> Result<ProposalVerdict, AgentOutcomeError> {
    let Some(community) = proposal.get("community").filter(|c| c.is_object()) else {
        return Ok(rejected("no community block"));
    };
    let name = community
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    // A reddit proposal may name the community by `subreddit` instead of a
    // URL — the URL is derivable, and derivable is not missing.
    let community_sub = community
        .get("subreddit")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let url_raw = community
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let derived_url;
    let url = if url_raw.is_empty() && !community_sub.is_empty() {
        derived_url = format!("https://www.reddit.com/r/{community_sub}");
        derived_url.as_str()
    } else {
        url_raw
    };
    let platform = community
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    // Bounds are checked here, not left to the column CHECKs: a violation
    // there fails the whole outcome and takes every sibling verdict down
    // with it, where a rejected verdict records the reason.
    if name.is_empty() {
        return Ok(rejected("community proposal has no name"));
    }
    if name.chars().count() > 200 {
        return Ok(rejected("community name is longer than the column allows"));
    }
    if url.is_empty() {
        return Ok(rejected("community proposal has no url"));
    }
    if url.chars().count() > 500 {
        return Ok(rejected("community url is longer than the column allows"));
    }
    // The agents vocabulary says "reddit" where the place vocabulary says
    // "subreddit" — same translation the ingestion path makes.
    let platform = if platform == "reddit" { "subreddit" } else { platform };
    if !COMMUNITY_PLACE_KINDS.contains(&platform) {
        return Ok(rejected(format!(
            "community platform {platform:?} is outside the place vocabulary"
        )));
    }
    // A Reddit URL carries its subreddit — derive it so the proposal joins
    // the same identity path a scanned subreddit does. A reddit URL that
    // does not name a /r/ path is not a community at all.
    let subreddit = (platform == "subreddit")
        .then(|| extract_subreddit_slug(url))
        .flatten();
    if platform == "subreddit" && subreddit.is_none() {
        return Ok(rejected("a reddit community must name a /r/ subreddit link"));
    }
    let why = community
        .get("why")
        .and_then(Value::as_str)
        .or_else(|| proposal.get("rationale").and_then(Value::as_str))
        .unwrap_or_default();
    // The community's own URL is its evidence: the scout's web tools saw
    // the page, and the row's evidence links back to it.
    let synthesized = json!({
        "target_kind": "community",
        "display_name": name,
        "platform": platform,
        "community_url": url,
        "subreddit": subreddit,
        "language": community.get("language").and_then(Value::as_str),
        "why_fit": why,
        "evidence_urls": [url],
        "contact_domain": community_url_host(url),
    });
    let (_, target_id) = self.insert_outreach_target(tx, outcome, &synthesized).await?;
    // Read back the screening verdict so the consultant learns the real
    // outcome — a community the screener refused is recorded, not joined,
    // and "accepted (refused: too_small)" teaches that honestly.
    // Read back the row's real state so the consultant learns the actual
    // outcome — a community the screener refused is recorded, not joined,
    // and a row an operator discarded stays discarded however the screen
    // reads now; "accepted (promoted)" on either would teach a lie.
    let screened: Option<(Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT screening_verdict, refusal_reason, status FROM agent_outreach_targets
         WHERE id = $1 AND workspace_id = $2",
    )
    .bind(target_id)
    .bind(outcome.workspace_id)
    .fetch_optional(&mut **tx)
    .await?;
    match screened.as_ref() {
        Some((_, _, status)) if status == "discarded" => Ok(rejected(
            "community was previously discarded by the operator — kept discarded",
        )),
        Some((Some(v), reason, _)) if v == "refused" => Ok(rejected(format!(
            "community recorded but screening refused it: {}",
            reason.as_deref().unwrap_or("no reason recorded")
        ))),
        Some((_, _, status)) => Ok(accepted(format!(
            "community target {target_id} ({status})"
        ))),
        _ => Ok(accepted(format!("community target {target_id}"))),
    }
}

/// `rescan`: queue a one-shot run for an intelligence template. Restricted
/// to Intelligence-audience workers — a proposal must never be able to pull
/// a posting worker forward.
///
/// The floor matters as much as the pending-unique: a rescan bypasses the
/// template's cooldown for one dispatch, so without a rate bound a consult
/// that proposes `rescan` on every run converts a weekly premium template
/// into a ~hourly one. Twenty-four hours is the bound — the same order as
/// the fastest cadence the policy allows.
const RESCAN_FLOOR: &str = "24 hours";
async fn evaluate_rescan(
    &self,
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    proposal: &Value,
) -> Result<ProposalVerdict, AgentOutcomeError> {
    let template_id = proposal
        .get("template_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if template_id.is_empty() {
        return Ok(rejected("no template_id"));
    }
    let Some(template) = WorkerTemplate::parse(template_id) else {
        return Ok(rejected(format!("unknown template {template_id:?}")));
    };
    if template.is_disabled() {
        // A disabled template never reaches the snapshot list, so a pending
        // request for it would wait forever — and block every later ask via
        // the one-pending-per-template index.
        return Ok(rejected(format!("{template_id} is disabled and never dispatches")));
    }
    if template.audience() != TemplateAudience::Intelligence {
        return Ok(rejected(format!(
            "rescan only applies to intelligence templates — {template_id} reaches an audience"
        )));
    }
    let reason = proposal
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    // Rate bound before the pending check: a rescan consumed moments ago
    // must not be re-accepted by the very next consult, or the one-shot
    // bypass becomes a continuous loop.
    let floor = Self::RESCAN_FLOOR;
    let recently: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM agent_template_rescan_requests
             WHERE workspace_id = $1 AND template_id = $2
               AND created_at > now() - $3::interval)",
    )
    .bind(outcome.workspace_id)
    .bind(template_id)
    .bind(floor)
    .fetch_one(&mut **tx)
    .await?;
    if recently {
        return Ok(rejected(format!(
            "{template_id} was rescan-requested within the last {floor}"
        )));
    }
    // A pending request the loader will never fire (the template was
    // disabled after the ask, or a deploy ate the dispatch) would otherwise
    // hold the one-pending slot forever — anything older than a week is
    // stale enough to release.
    sqlx::query(
        "UPDATE agent_template_rescan_requests SET consumed_at = now()
         WHERE workspace_id = $1 AND template_id = $2
           AND consumed_at IS NULL AND created_at < now() - interval '7 days'",
    )
    .bind(outcome.workspace_id)
    .bind(template_id)
    .execute(&mut **tx)
    .await?;
    // One pending request per template: a second ask while one is queued is
    // a duplicate, not a stronger ask. The partial unique index backs this.
    let pending: Option<Uuid> = sqlx::query_scalar(
        r#"
        INSERT INTO agent_template_rescan_requests
            (workspace_id, template_id, reason)
        VALUES ($1, $2, $3)
        ON CONFLICT (workspace_id, template_id) WHERE consumed_at IS NULL
            DO NOTHING
        RETURNING id
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(template_id)
    .bind(reason)
    .fetch_optional(&mut **tx)
    .await?;
    match pending {
        Some(id) => Ok(accepted(format!("rescan request {id}"))),
        None => Ok(rejected("a rescan for this template is already pending")),
    }
}

/// `adjust_cadence`: write the new cooldown into the growth-intelligence
/// policy config. Bounded both sides — below the floor is spam-adjacent,
/// above the ceiling silently disables the worker.
async fn evaluate_adjust_cadence(
    &self,
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    proposal: &Value,
) -> Result<ProposalVerdict, AgentOutcomeError> {
    let template_id = proposal
        .get("template_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if template_id.is_empty() {
        return Ok(rejected("no template_id"));
    }
    let Some(template) = WorkerTemplate::parse(template_id) else {
        return Ok(rejected(format!("unknown template {template_id:?}")));
    };
    if template.is_disabled() {
        return Ok(rejected(format!(
            "{template_id} is disabled — adjusting a cooldown that never fires"
        )));
    }
    let Some(hours) = proposal.get("cooldown_hours").and_then(Value::as_i64) else {
        return Ok(rejected("no cooldown_hours"));
    };
    if !(CADENCE_FLOOR_HOURS..=CADENCE_CEILING_HOURS).contains(&hours) {
        return Ok(rejected(format!(
            "cooldown_hours {hours} is outside the {CADENCE_FLOOR_HOURS}–{CADENCE_CEILING_HOURS}h bound"
        )));
    }
    let field = template.cooldown_policy_field();
    // The field name is a closed enum's own constant — safe to embed in the
    // jsonb_set path, and the write is one bounded integer into config.
    let updated: Option<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE autopilot_policies
        SET config = jsonb_set(config, ARRAY[$2]::text[], to_jsonb($3::int), true),
            version = version + 1,
            updated_at = now()
        WHERE workspace_id = $1 AND context = 'growth_intelligence'
        RETURNING workspace_id
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(field)
    .bind(hours as i32)
    .fetch_optional(&mut **tx)
    .await?;
    match updated {
        Some(_) => Ok(accepted(format!("policy {field}={hours}h"))),
        None => Ok(rejected("no growth_intelligence policy row exists")),
    }
}

/// `queue_scan_queries`: land queries the fanbase scout unions into its
/// next run. Per-query bounds match the column; all-duplicates is a
/// rejection, not a quiet success.
async fn evaluate_queue_scan_queries(
    &self,
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    proposal: &Value,
) -> Result<ProposalVerdict, AgentOutcomeError> {
    let reason = proposal
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let Some(queries) = proposal.get("queries").and_then(Value::as_array) else {
        return Ok(rejected("no queries"));
    };
    let mut queued = 0u32;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for query in queries.iter().take(MAX_QUERIES_PER_PROPOSAL) {
        let Some(text) = query.as_str().map(str::trim) else {
            continue;
        };
        if text.is_empty() || text.chars().count() > 200 || !seen.insert(text.to_owned()) {
            continue;
        }
        let inserted: Option<Uuid> = sqlx::query_scalar(
            r#"
            INSERT INTO agent_scan_query_queue (workspace_id, query, reason)
            VALUES ($1, $2, $3)
            ON CONFLICT (workspace_id, lower(btrim(query))) WHERE consumed_at IS NULL
                DO NOTHING
            RETURNING id
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(text)
        .bind(reason)
        .fetch_optional(&mut **tx)
        .await?;
        if inserted.is_some() {
            queued += 1;
        }
    }
    if queued == 0 {
        return Ok(rejected("every proposed query was empty or already queued"));
    }
    Ok(accepted(format!("queued {queued} scan queries")))
}
}

/// The host a community URL points at — the contact_domain the target row
/// carries for non-Reddit communities.
fn community_url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

/// `https://reddit.com/r/deathcore` → `deathcore`. `None` when the URL is
/// not a /r/ path — the proposal then takes the URL-identity path like any
/// other platform.
fn extract_subreddit_slug(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    // `ends_with` alone would accept evilreddit.com — the host must be
    // reddit.com itself or a real subdomain of it.
    let is_reddit =
        host == "reddit.com" || host.ends_with(".reddit.com") || host == "redd.it";
    if !is_reddit {
        return None;
    }
    let mut segments = parsed.path_segments()?;
    if segments.next()? != "r" {
        return None;
    }
    let slug = segments.next()?.trim_matches('/');
    (!slug.is_empty()).then(|| slug.to_owned())
}

#[cfg(test)]
mod proposal_tests {
    use super::*;

    #[test]
    fn unknown_actions_parse_to_none() {
        assert_eq!(ProposalAction::parse("propose_community"), Some(ProposalAction::ProposeCommunity));
        assert_eq!(ProposalAction::parse("post_now"), None);
        assert_eq!(ProposalAction::parse(""), None);
    }

    #[test]
    fn subreddit_slug_extracts_from_reddit_urls() {
        assert_eq!(
            extract_subreddit_slug("https://www.reddit.com/r/deathcore/").as_deref(),
            Some("deathcore")
        );
        assert_eq!(
            extract_subreddit_slug("https://reddit.com/r/Metalcore").as_deref(),
            Some("Metalcore")
        );
        assert_eq!(extract_subreddit_slug("https://reddit.com/u/someone"), None);
        assert_eq!(extract_subreddit_slug("https://discord.gg/metal"), None);
        assert_eq!(extract_subreddit_slug("not a url"), None);
    }

    #[test]
    fn community_url_host_reads_the_domain() {
        assert_eq!(
            community_url_host("https://discord.gg/metal").as_deref(),
            Some("discord.gg")
        );
        assert_eq!(community_url_host("not a url"), None);
    }
}
