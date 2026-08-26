//! Read-only policy replay over recorded autopilot decisions.
//!
//! Answers one operator question from history instead of production
//! audiences: "how would each posture have disposed of the decisions I
//! actually faced?" Every counterfactual recomputes the pure authority funnel
//! (`GrowthPosture::context_level` + `autonomy::disposition`) over the stored
//! decision-time evidence — the confidence the engine had and the minimum
//! confidence its policy demanded at that moment. Nothing is written; no
//! action is executed; this is the backtest half of policy evaluation.

use anyhow::{Context, Result, bail};
use crowdrelay_application::autopilot::{AutopilotContext, GrowthPosture};
use crowdrelay_domain::autonomy::{Confidence, PolicyDisposition, disposition};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const DEFAULT_SINCE_DAYS: u32 = 30;
const MAX_SINCE_DAYS: u32 = 365;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayOptions {
    pub since_days: u32,
    pub json: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            since_days: DEFAULT_SINCE_DAYS,
            json: false,
        }
    }
}

pub fn parse_replay_options(args: impl IntoIterator<Item = String>) -> Result<ReplayOptions> {
    let mut options = ReplayOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--since-days" => {
                let value = args.next().context("--since-days requires a value")?;
                options.since_days = value.parse::<u32>().map_err(|_| {
                    anyhow::anyhow!("`--since-days` expects a number of days, got `{value}`")
                })?;
                if options.since_days == 0 || options.since_days > MAX_SINCE_DAYS {
                    bail!("--since-days must be between 1 and {MAX_SINCE_DAYS}");
                }
            }
            "--json" => options.json = true,
            other => {
                bail!("unknown replay option `{other}`; expected `--since-days <n>` or `--json`")
            }
        }
    }
    Ok(options)
}

/// One historical decision reduced to the inputs the authority funnel needs.
#[derive(Debug, Clone)]
pub struct RecordedDecision {
    pub context: Option<AutopilotContext>,
    pub confidence_bp: u16,
    pub recorded_disposition: &'static str,
    pub minimum_confidence_bp: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DispositionCounts {
    deny: u64,
    observe_only: u64,
    recommend_only: u64,
    require_approval: u64,
    auto_execute: u64,
}

impl DispositionCounts {
    const EMPTY: Self = Self {
        deny: 0,
        observe_only: 0,
        recommend_only: 0,
        require_approval: 0,
        auto_execute: 0,
    };

    fn record(&mut self, outcome: PolicyDisposition) {
        match outcome {
            PolicyDisposition::Deny => self.deny += 1,
            PolicyDisposition::ObserveOnly => self.observe_only += 1,
            PolicyDisposition::RecommendOnly => self.recommend_only += 1,
            PolicyDisposition::RequireApproval => self.require_approval += 1,
            PolicyDisposition::AutoExecute => self.auto_execute += 1,
        }
    }

    fn label_of(outcome: PolicyDisposition) -> &'static str {
        match outcome {
            PolicyDisposition::ObserveOnly => "observe_only",
            PolicyDisposition::RecommendOnly => "recommend_only",
            PolicyDisposition::RequireApproval => "require_approval",
            PolicyDisposition::AutoExecute => "auto_execute",
            PolicyDisposition::Deny => "deny",
        }
    }

    fn parse(label: &str) -> Option<PolicyDisposition> {
        match label {
            "observe_only" => Some(PolicyDisposition::ObserveOnly),
            "recommend_only" => Some(PolicyDisposition::RecommendOnly),
            "require_approval" => Some(PolicyDisposition::RequireApproval),
            "auto_execute" => Some(PolicyDisposition::AutoExecute),
            "deny" => Some(PolicyDisposition::Deny),
            _ => None,
        }
    }

    fn as_array(self) -> [u64; 5] {
        [
            self.deny,
            self.observe_only,
            self.recommend_only,
            self.require_approval,
            self.auto_execute,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostureReplay {
    pub posture: &'static str,
    counts: DispositionCounts,
    /// Decisions whose recomputed disposition differs from what was recorded.
    pub flips_vs_recorded: u64,
    /// Rows that could not be replayed (unparsable context).
    pub unparsed: u64,
}

impl PostureReplay {
    #[must_use]
    pub fn as_counts(self) -> [u64; 5] {
        self.counts.as_array()
    }
}

pub(crate) const DISPOSITION_LABELS: [&str; 5] = [
    "deny",
    "observe_only",
    "recommend_only",
    "require_approval",
    "auto_execute",
];

#[cfg(test)]
fn counterfactual(decisions: &[RecordedDecision]) -> Vec<PostureReplay> {
    let mut replays = GrowthPosture::ALL
        .iter()
        .map(|posture| PostureReplay {
            posture: posture.as_str(),
            counts: DispositionCounts::EMPTY,
            flips_vs_recorded: 0_u64,
            unparsed: 0_u64,
        })
        .collect::<Vec<_>>();
    fold_counterfactual(&mut replays, decisions);
    replays
}

/// Folds one streamed page of decisions into the per-posture accumulators.
/// Counting is order- and chunk-independent, so pages compose exactly like a
/// single pass over the whole history would.
fn fold_counterfactual(replays: &mut [PostureReplay], decisions: &[RecordedDecision]) {
    for decision in decisions {
        for replay in replays.iter_mut() {
            let Some(posture) = GrowthPosture::ALL
                .iter()
                .find(|p| p.as_str() == replay.posture)
            else {
                continue;
            };
            let Some(context) = decision.context else {
                replay.unparsed += 1;
                continue;
            };
            let confidence = Confidence::saturating_from_basis_points(decision.confidence_bp);
            let minimum = Confidence::saturating_from_basis_points(decision.minimum_confidence_bp);
            let outcome = disposition(posture.context_level(context), confidence, minimum);
            replay.counts.record(outcome);
            if DispositionCounts::label_of(outcome) != decision.recorded_disposition {
                replay.flips_vs_recorded += 1;
            }
        }
    }
}

pub async fn run_replay(
    database: &PgPool,
    workspace_id: crowdrelay_domain::WorkspaceId,
    workspace_slug: &str,
    options: ReplayOptions,
) -> Result<()> {
    // Decision history grows with every autopilot cycle, so stream it in
    // keyset pages instead of materializing a year of JSONB payloads at once.
    // The replay is pure counting, so folding per page is equivalent; the
    // (evaluated_at, id) cursor keeps page boundaries duplicate-free.
    const PAGE_SIZE: i64 = 5_000;
    let workspace_uuid = workspace_id.into_uuid();
    let mut replays = GrowthPosture::ALL
        .iter()
        .map(|posture| PostureReplay {
            posture: posture.as_str(),
            counts: DispositionCounts::EMPTY,
            flips_vs_recorded: 0_u64,
            unparsed: 0_u64,
        })
        .collect::<Vec<_>>();
    let mut decision_count = 0_usize;
    let mut cursor_time: Option<(OffsetDateTime, Uuid)> = None;
    loop {
        let rows =
            sqlx::query_as::<_, (OffsetDateTime, Uuid, String, i32, String, serde_json::Value)>(
                r#"
            SELECT evaluated_at, id, context, confidence_basis_points, disposition, policy_snapshot
            FROM viryaos_autopilot_decisions
            WHERE workspace_id = $1
              AND evaluated_at >= now() - make_interval(days => $2::int)
              AND ($3::timestamptz IS NULL OR (evaluated_at, id) > ($3, $4))
            ORDER BY evaluated_at, id
            LIMIT $5
            "#,
            )
            .bind(workspace_uuid)
            .bind(i32::try_from(options.since_days).unwrap_or(i32::MAX))
            .bind(cursor_time.as_ref().map(|(at, _)| *at))
            .bind(cursor_time.as_ref().map(|(_, id)| *id))
            .bind(PAGE_SIZE)
            .fetch_all(database)
            .await
            .context("failed to read autopilot decision history")?;
        if rows.is_empty() {
            break;
        }
        let page_len = rows.len();
        let page_end = rows.last().map(|(at, id, ..)| (*at, *id));
        let decisions: Vec<RecordedDecision> = rows
            .into_iter()
            .map(
                |(_, _, context_raw, confidence_bp, disposition_raw, snapshot)| RecordedDecision {
                    context: AutopilotContext::from_storage(&context_raw),
                    confidence_bp: u16::try_from(confidence_bp.clamp(0, i32::from(u16::MAX)))
                        .unwrap_or(u16::MAX),
                    recorded_disposition: DispositionCounts::parse(&disposition_raw)
                        .map(DispositionCounts::label_of)
                        .unwrap_or("deny"),
                    minimum_confidence_bp: snapshot
                        .get("minimum_confidence_basis_points")
                        .and_then(serde_json::Value::as_i64)
                        .and_then(|value| u16::try_from(value).ok())
                        .unwrap_or(8_000),
                },
            )
            .collect();
        decision_count += decisions.len();
        fold_counterfactual(&mut replays, &decisions);
        cursor_time = page_end;
        if page_len < usize::try_from(PAGE_SIZE).unwrap_or(usize::MAX) {
            break;
        }
    }

    let action_rows = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT status, count(*) AS actions
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND created_at >= now() - make_interval(days => $2::int)
        GROUP BY status
        ORDER BY actions DESC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(i32::try_from(options.since_days).unwrap_or(i32::MAX))
    .fetch_all(database)
    .await
    .context("failed to read autopilot action history")?;

    let median_approval_minutes: Option<f64> = sqlx::query_scalar(
        r#"
        SELECT percentile_cont(0.5) WITHIN GROUP (
            ORDER BY EXTRACT(EPOCH FROM (approved_at - created_at)) / 60.0
        )
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND approved_at IS NOT NULL
          AND created_at >= now() - make_interval(days => $2::int)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(i32::try_from(options.since_days).unwrap_or(i32::MAX))
    .fetch_one(database)
    .await
    .context("failed to read approval latency history")?;

    if options.json {
        let document = serde_json::json!({
            "workspace": workspace_slug,
            "since_days": options.since_days,
            "decision_count": decision_count,
            "postures": replays.iter().map(|replay| serde_json::json!({
                "posture": replay.posture,
                "dispositions": DISPOSITION_LABELS
                    .iter()
                    .zip(replay.clone().as_counts())
                    .map(|(label, count)| serde_json::json!({
                        "disposition": label,
                        "count": count,
                    }))
                    .collect::<Vec<_>>(),
                "flips_vs_recorded": replay.flips_vs_recorded,
                "unparsed": replay.unparsed,
            })).collect::<Vec<_>>(),
            "actions_by_status": action_rows.iter().map(|(status, count)| serde_json::json!({
                "status": status,
                "count": count,
            })).collect::<Vec<_>>(),
            "median_approval_minutes": median_approval_minutes,
        });
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    println!(
        "autopilot policy replay — workspace `{workspace_slug}`, last {} day(s)",
        options.since_days
    );
    println!("decisions considered: {decision_count}");
    println!();
    println!(
        "{:<12} {:>7} {:>9} {:>11} {:>10} {:>6} {:>8} {:>9}",
        "posture", "deny", "observe", "recommend", "approval", "auto", "flips*", "unparsed"
    );
    for replay in &replays {
        let counts = replay.counts;
        println!(
            "{:<12} {:>7} {:>9} {:>11} {:>10} {:>6} {:>8} {:>9}",
            replay.posture,
            counts.deny,
            counts.observe_only,
            counts.recommend_only,
            counts.require_approval,
            counts.auto_execute,
            replay.flips_vs_recorded,
            replay.unparsed,
        );
    }
    println!();
    println!("* flips: recomputed disposition differs from the recorded decision");
    println!();
    if action_rows.is_empty() {
        println!("actions in window: none");
    } else {
        println!("actions in window:");
        for (status, count) in &action_rows {
            println!("  {status:<20} {count}");
        }
    }
    if let Some(minutes) = median_approval_minutes {
        println!("median approval latency: {minutes:.1} min");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(
        context: Option<AutopilotContext>,
        confidence_bp: u16,
        minimum_confidence_bp: u16,
    ) -> RecordedDecision {
        RecordedDecision {
            context,
            confidence_bp,
            recorded_disposition: "require_approval",
            minimum_confidence_bp,
        }
    }

    #[test]
    fn replay_options_default_and_parse() {
        assert_eq!(ReplayOptions::default(), parse_replay_options([]).unwrap());
        let parsed = parse_replay_options([
            "--json".to_owned(),
            "--since-days".to_owned(),
            "90".to_owned(),
        ])
        .unwrap();
        assert_eq!(parsed.since_days, 90);
        assert!(parsed.json);
        assert!(parse_replay_options(["--bogus".to_owned()]).is_err());
        assert!(parse_replay_options(["--since-days".to_owned()]).is_err());
        assert!(parse_replay_options(["--since-days".to_owned(), "0".to_owned()]).is_err());
        assert!(
            parse_replay_options(["--since-days".to_owned(), (MAX_SINCE_DAYS + 1).to_string()])
                .is_err()
        );
    }

    #[test]
    fn grounded_observes_what_full_send_would_have_executed() {
        let decisions = vec![decision(Some(AutopilotContext::Outreach), 9_500, 8_000)];
        let replays = counterfactual(&decisions);
        let by_name = |name: &str| {
            replays
                .iter()
                .find(|replay| replay.posture == name)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("grounded").counts.observe_only, 1);
        assert_eq!(by_name("working").counts.require_approval, 1);
        assert_eq!(by_name("full_send").counts.auto_execute, 1);
        assert_eq!(by_name("full_send").flips_vs_recorded, 1);
    }

    #[test]
    fn low_confidence_denies_in_every_posture() {
        let decisions = vec![decision(Some(AutopilotContext::Beacon), 5_000, 8_000)];
        for replay in counterfactual(&decisions) {
            assert_eq!(replay.counts.deny, 1, "{}", replay.posture);
        }
    }

    #[test]
    fn unparsable_context_is_counted_not_dropped() {
        let decisions = vec![decision(None, 9_000, 8_000)];
        for replay in counterfactual(&decisions) {
            assert_eq!(replay.unparsed, 1);
            assert_eq!(replay.counts, DispositionCounts::EMPTY);
        }
    }

    #[test]
    fn disposition_labels_round_trip_with_storage() {
        for label in DISPOSITION_LABELS {
            let parsed = DispositionCounts::parse(label)
                .map(DispositionCounts::label_of)
                .unwrap();
            assert_eq!(parsed, label);
        }
        assert!(DispositionCounts::parse("yolo").is_none());
    }
}
