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
    CommunityCandidateSnapshot, ScreeningVerdict, TargetDiscoveryPolicy, screen_community_candidate,
};
use sqlx::PgPool;
use uuid::Uuid;

/// How many places one sweep promotes. Bounded like every other sweep in the
/// worker: a large audience graph must not turn one pass into a long
/// transaction.
const PROMOTION_BATCH: i64 = 100;

/// A place considered for promotion, with the rules it published.
type PlaceRow = (
    Uuid,
    String,
    Option<String>,
    Option<i32>,
    Option<i32>,
    String,
    String,
    Option<i16>,
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

/// Promotes screened community places into `agent_outreach_targets`.
///
/// Only places with no target row yet are considered, so the sweep is cheap
/// once it has caught up and re-running it is safe. A place whose target
/// already exists is left alone: re-screening a live target belongs to the
/// outcome-ingest path, which sees the agent's fresh evidence.
pub(super) async fn promote_community_places(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<PromotionReport, sqlx::Error> {
    let places: Vec<PlaceRow> = sqlx::query_as(
        r#"
        SELECT place.id,
               place.name,
               substring(place.url from '/r/([^/?#]+)') AS subreddit,
               place.member_count,
               place.activity_bp,
               place.status,
               place.membership_state,
               rules.self_promo_ratio_percent
        FROM discovery_places AS place
        LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
        WHERE place.workspace_id = $1
          AND place.place_kind = 'subreddit'
          AND place.status = 'active'
          AND substring(place.url from '/r/([^/?#]+)') IS NOT NULL
          AND NOT EXISTS (
              SELECT 1 FROM agent_outreach_targets AS t
              WHERE t.workspace_id = place.workspace_id
                AND t.place_id = place.id
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

    for (id, name, subreddit, member_count, activity_bp, status, membership_state, self_promo) in
        places
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
               candidate.subreddit,
               CASE WHEN candidate.verdict = 'admitted' THEN 'promoted' ELSE 'proposed' END,
               candidate.place_id, candidate.verdict, candidate.refusal, now()
        FROM unnest($2::uuid[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[])
             AS candidate(place_id, name, subreddit, why_fit, verdict, refusal)
        JOIN discovery_places AS place ON place.id = candidate.place_id
        ON CONFLICT (workspace_id, display_name, target_kind) DO UPDATE SET
            place_id = COALESCE(agent_outreach_targets.place_id, EXCLUDED.place_id),
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
