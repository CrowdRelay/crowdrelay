//! Turning discovered communities into candidates the brain can act on.
//!
//! Discovery imports subreddits into `discovery_places` and the brain reads
//! community candidates out of `agent_outreach_targets`. Nothing connected the
//! two: the only writer of community-kind targets was the agent outcome
//! ingest, which depended on a scanner LLM choosing to emit an
//! `outreach_target` item with `target_kind: "community"`. It never did.
//!
//! The result was a growth loop that could not run. Production held 28 active
//! subreddits — r/Metal at 2.6M members, r/metalcore at 1M, r/progmetal at
//! 296k — zero community targets, zero community-engager dispatches, and zero
//! posts. Every improvement to how communities are ranked was ranking an
//! empty set.
//!
//! Promotion is deterministic and lives here rather than in a prompt. A place
//! the audience graph already holds does not need an LLM to notice it exists;
//! it needs the screening policy applied to it and a row the brain can carry
//! an experiment on.
//!
//! Refusals are written too. A community that fails screening is recorded
//! with its reason so the next sweep does not rediscover, re-screen and
//! re-refuse it every pass, and so an operator can see what was rejected and
//! why.

use crowdrelay_domain::target_discovery::{
    CommunityCandidateSnapshot, ScreeningVerdict, TargetDiscoveryPolicy, community_topic_signal,
    fit_to_act, screen_community_candidate,
};
use sqlx::PgPool;
use uuid::Uuid;

/// How many places one sweep promotes. Bounded like every other sweep in the
/// worker: a large audience graph must not turn one pass into a long
/// transaction.
const PROMOTION_BATCH: i64 = 100;

/// A place considered for promotion, with the rules it published.
/// id, name, subreddit, member_count, activity_bp, status, membership_state,
/// self_promo_ratio_percent, notes, genres — the last three fields feed the
/// topical screen, which reads the community's own description rather than
/// trusting its size.
#[allow(clippy::type_complexity)]
type PlaceRow = (
    Uuid,
    String,
    Option<String>,
    Option<i32>,
    Option<i32>,
    String,
    String,
    Option<i16>,
    Option<String>,
    Vec<String>,
);

/// What one promotion sweep did.
#[derive(Debug, Default)]
pub(super) struct PromotionReport {
    pub(super) admitted: u64,
    pub(super) refused: u64,
}

impl PromotionReport {
    pub(super) fn touched_anything(&self) -> bool {
        self.admitted > 0 || self.refused > 0
    }
}

/// The community surfaces the engagement ledger can name — Discord servers,
/// Telegram groups, Lemmy communities and web forums. Other place kinds
/// (playlists, festivals, feeds) are not post destinations, and Meta groups
/// stay out while that lane is operator-run.
const COMMUNITY_PLACE_KINDS: &[&str] = &["discord", "telegram", "lemmy", "forum"];

/// Promotes screened community places into `agent_outreach_targets`.
///
/// Only places with no target row yet are considered, so the sweep is cheap
/// once it has caught up and re-running it is safe. A place whose target
/// already exists is left alone: re-screening a live target belongs to the
/// outcome-ingest path, which sees the agent's fresh evidence.
///
/// Two identity shapes land in the same table. A subreddit is its slug; any
/// other community is its canonical URL — the same split
/// `agent_outcomes` makes when a scanner proposal arrives. A forum post or a
/// Discord server has no subreddit to carry: its `community_url` is the
/// identity, and its platform is what routes a draft to the right lane.
pub(super) async fn promote_community_places(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<PromotionReport, sqlx::Error> {
    let mut report = promote_subreddit_places(pool, workspace_id).await?;
    let non_reddit = promote_non_subreddit_places(pool, workspace_id).await?;
    report.admitted += non_reddit.admitted;
    report.refused += non_reddit.refused;
    Ok(report)
}

/// The Reddit pass — `r/{slug}` out of the place URL is the target identity.
async fn promote_subreddit_places(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<PromotionReport, sqlx::Error> {
    let act_style: Option<String> = sqlx::query_scalar(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'act_style'",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?;
    let places: Vec<PlaceRow> = sqlx::query_as(
        r#"
        SELECT place.id,
               place.name,
               substring(place.url from '/r/([^/?#]+)') AS subreddit,
               place.member_count,
               place.activity_bp,
               place.status,
               place.membership_state,
               rules.self_promo_ratio_percent,
               place.notes,
               COALESCE(place.genres, '{}'::text[])
        FROM discovery_places AS place
        LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
        WHERE place.workspace_id = $1
          AND place.place_kind = 'subreddit'
          AND place.status = 'active'
          AND substring(place.url from '/r/([^/?#]+)') IS NOT NULL
          -- A place that already has a target row is screened once and never
          -- again — which is right for a verdict about the community itself,
          -- and wrong for one about a past attempt of ours. `previously_refused`
          -- means we failed to reach it, and that can stop being true: the
          -- join executor used to write `rejected` for its own outages, so a
          -- credential blip could refuse a community permanently. Those rows
          -- come back for re-screening; every other verdict stays put.
          AND NOT EXISTS (
              SELECT 1 FROM agent_outreach_targets AS t
              WHERE t.workspace_id = place.workspace_id
                AND t.place_id = place.id
                AND (
                    t.screening_verdict IS DISTINCT FROM 'refused'
                    OR t.refusal_reason IS DISTINCT FROM 'previously_refused'
                )
          )
        ORDER BY place.member_count DESC NULLS LAST
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    .bind(PROMOTION_BATCH)
    .fetch_all(pool)
    .await?;

    // Screening is pure and cheap; the write is one statement for the whole
    // batch. This was an INSERT per place inside the loop — a hundred round
    // trips a sweep to write at most a hundred small rows, on a connection
    // the rest of the worker is also using. The arrays go over in one
    // parameter each and Postgres unnests them.
    let mut place_ids: Vec<Uuid> = Vec::with_capacity(places.len());
    let mut names: Vec<String> = Vec::with_capacity(places.len());
    let mut subreddits: Vec<String> = Vec::with_capacity(places.len());
    let mut why_fits: Vec<String> = Vec::with_capacity(places.len());
    let mut verdicts: Vec<String> = Vec::with_capacity(places.len());
    let mut refusals: Vec<Option<String>> = Vec::with_capacity(places.len());
    let mut report = PromotionReport::default();

    for (
        id,
        name,
        subreddit,
        member_count,
        activity_bp,
        status,
        membership_state,
        self_promo,
        notes,
        genres,
    ) in places
    {
        let Some(subreddit) = subreddit
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let snapshot = CommunityCandidateSnapshot {
            // The place itself is the evidence: it carries the URL discovery
            // read it from, and an operator can open it.
            has_evidence: true,
            member_count: member_count.and_then(|v| u32::try_from(v).ok()),
            activity_basis_points: activity_bp.and_then(|v| u16::try_from(v).ok()),
            self_promo_ratio_percent: self_promo.and_then(|v| u8::try_from(v).ok()),
            sells_placement: false,
            refused_by_us_or_them: status == "blocked"
                || matches!(membership_state.as_str(), "rejected" | "not_a_fit"),
            // r/BlackMetal reached a metalcore band's posting queue through
            // this sweep on "size not yet measured" with nothing reading its
            // scene against the act's. See `community_topic::fit_to_act`.
            topic_signal: fit_to_act(
                community_topic_signal(&name, notes.as_deref(), &genres),
                act_style.as_deref(),
                &name,
                notes.as_deref(),
                &genres,
            ),
        };
        // A refused community is still written, as a refused row. Recording
        // the refusal is what stops the next sweep rediscovering it.
        let (verdict, refusal) =
            match screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()) {
                ScreeningVerdict::Admit { .. } => ("admitted", None),
                ScreeningVerdict::Refuse(reason) => ("refused", Some(reason.as_str().to_owned())),
            };
        let why_fit = match member_count {
            Some(members) => format!(
                "Promoted from the audience graph: {members} members, discovered as a subreddit place."
            ),
            None => "Promoted from the audience graph: subreddit place, size not yet measured."
                .to_owned(),
        };
        if refusal.is_some() {
            report.refused += 1;
        } else {
            report.admitted += 1;
        }
        place_ids.push(id);
        names.push(name);
        subreddits.push(subreddit);
        why_fits.push(why_fit);
        verdicts.push(verdict.to_owned());
        refusals.push(refusal);
    }

    if place_ids.is_empty() {
        return Ok(report);
    }

    sqlx::query(
        r#"
        INSERT INTO agent_outreach_targets
            (workspace_id, target_kind, display_name, why_fit, evidence,
             subreddit, status, place_id, screening_verdict, refusal_reason, screened_at)
        SELECT $1, 'community', candidate.name, candidate.why_fit,
               jsonb_build_array(place.url),
               normalize_subreddit(candidate.subreddit),
               CASE WHEN candidate.verdict = 'admitted' THEN 'promoted' ELSE 'proposed' END,
               candidate.place_id, candidate.verdict, candidate.refusal, now()
        FROM unnest($2::uuid[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[])
             AS candidate(place_id, name, subreddit, why_fit, verdict, refusal)
        JOIN discovery_places AS place ON place.id = candidate.place_id
        -- The conflict target is the normalized subreddit, not the display
        -- name: a place promoted here may share a subreddit with a target
        -- the scanner proposed under a different name, and display-name
        -- dedup let both live (production held eleven doubled subreddits,
        -- each drafted twice per wave). One row per subreddit — a second
        -- proposal updates the same row.
        ON CONFLICT (workspace_id, normalize_subreddit(subreddit))
            WHERE target_kind = 'community'
              AND subreddit IS NOT NULL
              AND normalize_subreddit(subreddit) <> ''
        DO UPDATE SET
            place_id = COALESCE(agent_outreach_targets.place_id, EXCLUDED.place_id),
            -- The status follows the verdict (migration 0375): a refused
            -- community that grew is readmitted here, not left `proposed`.
            status = community_target_status(
                agent_outreach_targets.status, EXCLUDED.screening_verdict),
            screening_verdict = EXCLUDED.screening_verdict,
            refusal_reason = EXCLUDED.refusal_reason,
            screened_at = EXCLUDED.screened_at,
            subreddit = COALESCE(agent_outreach_targets.subreddit, EXCLUDED.subreddit),
            updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .bind(&place_ids)
    .bind(&names)
    .bind(&subreddits)
    .bind(&why_fits)
    .bind(&verdicts)
    .bind(&refusals)
    .execute(pool)
    .await?;

    Ok(report)
}

/// The non-Reddit pass — the same screening and the same target table, but
/// the place's canonical URL is the identity and its platform is what a
/// drafted delivery routes on.
async fn promote_non_subreddit_places(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<PromotionReport, sqlx::Error> {
    let act_style: Option<String> = sqlx::query_scalar(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'act_style'",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?;
    let places: Vec<PlaceRow> = sqlx::query_as(
        r#"
        SELECT place.id,
               place.name,
               NULL::text AS subreddit,
               place.member_count,
               place.activity_bp,
               place.status,
               place.membership_state,
               rules.self_promo_ratio_percent,
               place.notes,
               COALESCE(place.genres, '{}'::text[])
        FROM discovery_places AS place
        LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
        WHERE place.workspace_id = $1
          AND place.place_kind = ANY($2)
          AND place.platform = place.place_kind
          AND place.status = 'active'
          AND place.url IS NOT NULL
          AND btrim(place.url) <> ''
          -- Same leave-it-alone rule as the Reddit pass: a live target's
          -- verdict stands; only `previously_refused` comes back.
          AND NOT EXISTS (
              SELECT 1 FROM agent_outreach_targets AS t
              WHERE t.workspace_id = place.workspace_id
                AND t.place_id = place.id
                AND (
                    t.screening_verdict IS DISTINCT FROM 'refused'
                    OR t.refusal_reason IS DISTINCT FROM 'previously_refused'
                )
          )
        ORDER BY place.member_count DESC NULLS LAST
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(COMMUNITY_PLACE_KINDS)
    .bind(PROMOTION_BATCH)
    .fetch_all(pool)
    .await?;

    let mut place_ids: Vec<Uuid> = Vec::with_capacity(places.len());
    let mut names: Vec<String> = Vec::with_capacity(places.len());
    let mut why_fits: Vec<String> = Vec::with_capacity(places.len());
    let mut verdicts: Vec<String> = Vec::with_capacity(places.len());
    let mut refusals: Vec<Option<String>> = Vec::with_capacity(places.len());
    let mut report = PromotionReport::default();

    for (
        id,
        name,
        _subreddit,
        member_count,
        activity_bp,
        status,
        membership_state,
        self_promo,
        notes,
        genres,
    ) in places
    {
        let snapshot = CommunityCandidateSnapshot {
            has_evidence: true,
            member_count: member_count.and_then(|v| u32::try_from(v).ok()),
            activity_basis_points: activity_bp.and_then(|v| u16::try_from(v).ok()),
            self_promo_ratio_percent: self_promo.and_then(|v| u8::try_from(v).ok()),
            sells_placement: false,
            refused_by_us_or_them: status == "blocked"
                || matches!(membership_state.as_str(), "rejected" | "not_a_fit"),
            topic_signal: fit_to_act(
                community_topic_signal(&name, notes.as_deref(), &genres),
                act_style.as_deref(),
                &name,
                notes.as_deref(),
                &genres,
            ),
        };
        let (verdict, refusal) =
            match screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()) {
                ScreeningVerdict::Admit { .. } => ("admitted", None),
                ScreeningVerdict::Refuse(reason) => ("refused", Some(reason.as_str().to_owned())),
            };
        let why_fit = match member_count {
            Some(members) => format!(
                "Promoted from the audience graph: {members} members, discovered as a community place."
            ),
            None => "Promoted from the audience graph: community place, size not yet measured."
                .to_owned(),
        };
        if refusal.is_some() {
            report.refused += 1;
        } else {
            report.admitted += 1;
        }
        place_ids.push(id);
        names.push(name);
        why_fits.push(why_fit);
        verdicts.push(verdict.to_owned());
        refusals.push(refusal);
    }

    if place_ids.is_empty() {
        return Ok(report);
    }

    sqlx::query(
        r#"
        INSERT INTO agent_outreach_targets
            (workspace_id, target_kind, display_name, why_fit, evidence,
             status, place_id, screening_verdict, refusal_reason, screened_at,
             platform, community_url)
        SELECT $1, 'community', candidate.name, candidate.why_fit,
               jsonb_build_array(place.url),
               CASE WHEN candidate.verdict = 'admitted' THEN 'promoted' ELSE 'proposed' END,
               candidate.place_id, candidate.verdict, candidate.refusal, now(),
               place.platform,
               normalize_community_url(place.url)
        FROM unnest($2::uuid[], $3::text[], $4::text[], $5::text[], $6::text[])
             AS candidate(place_id, name, why_fit, verdict, refusal)
        JOIN discovery_places AS place ON place.id = candidate.place_id
        -- One row per community URL — the same dedup the ingest upsert
        -- uses, so a promoted place and a scanner proposal land on the same
        -- target rather than doubling.
        ON CONFLICT (workspace_id, normalize_community_url(community_url))
            WHERE target_kind = 'community'
              AND community_url IS NOT NULL
              AND btrim(community_url) <> ''
        DO UPDATE SET
            place_id = COALESCE(agent_outreach_targets.place_id, EXCLUDED.place_id),
            status = community_target_status(
                agent_outreach_targets.status, EXCLUDED.screening_verdict),
            screening_verdict = EXCLUDED.screening_verdict,
            refusal_reason = EXCLUDED.refusal_reason,
            screened_at = EXCLUDED.screened_at,
            platform = COALESCE(agent_outreach_targets.platform, EXCLUDED.platform),
            updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .bind(&place_ids)
    .bind(&names)
    .bind(&why_fits)
    .bind(&verdicts)
    .bind(&refusals)
    .execute(pool)
    .await?;

    Ok(report)
}
