// The measurement ledger — the plan's fifteen claims, each stated with the
// number that answers it or the reason this build cannot produce one.
//
// The plan judges the first 90 days on rates, not totals. A rate is stated
// only when its denominator clears `RATE_FLOOR`; below it the counts are
// shown and the rate is not, and a claim this build cannot measure says why.
// Every number is derived from the existing ledger tables — no new state, no
// new writes, no new migrations. The SQL itself lives in
// `crowdrelay_infra::measurement_queries` so the disposable-database test can
// drive the same strings.

use crowdrelay_domain::measurement::{
    Breakdown, Claim, Measure, MeasurementLedger, RATE_FLOOR, WINDOW_DAYS,
};
use crowdrelay_infra::measurement_queries;

#[derive(Debug, FromRow)]
struct UnsharedRow {
    happened: i64,
    shared: i64,
}

#[derive(Debug, FromRow)]
struct AmplificationRow {
    n: i64,
    median_minutes: Option<f64>,
}

#[derive(Debug, FromRow)]
struct SuggestionsReadRow {
    shown: i64,
    acted: i64,
}

#[derive(Debug, FromRow)]
struct OutreachChannelRow {
    channel: String,
    sent: i64,
    replied: i64,
}

#[derive(Debug, FromRow)]
struct TimeSavedRow {
    approvals: i64,
}

#[derive(Debug, FromRow)]
struct PlanFollowedRow {
    planned: i64,
    delivered: i64,
}

#[derive(Debug, FromRow)]
struct DriftCaughtRow {
    n: i64,
    median_days: Option<f64>,
    slipped_untold: i64,
}

#[derive(Debug, FromRow)]
struct RoomLeakRow {
    // `slug` is selected but unused — the breakdown label is title + date.
    title: String,
    starts_at: OffsetDateTime,
    scans: i64,
    room_size: Option<i64>,
}

#[derive(Debug, FromRow)]
struct ChannelFansRow {
    channel: String,
    fans: i64,
}

#[derive(Debug, FromRow)]
struct RecoveredRow {
    recovered: i64,
}

#[derive(Debug, FromRow)]
struct ArchiveRow {
    imported: i64,
    confirmed: i64,
}

#[derive(Debug, FromRow)]
struct SourceRoiRow {
    channel: String,
    acquired: i64,
    engaged_30d: i64,
}

fn claim(
    key: &'static str,
    claim: &'static str,
    measured_as: &'static str,
    measure: Measure,
) -> Claim {
    Claim {
        key,
        claim,
        measured_as,
        window_days: WINDOW_DAYS,
        measure,
        breakdown: Vec::new(),
    }
}

fn unmeasured(
    key: &'static str,
    claim_text: &'static str,
    measured_as: &'static str,
    reason: &'static str,
) -> Claim {
    claim(key, claim_text, measured_as, Measure::Unmeasured { reason })
}

async fn load_measurement_ledger(
    state: &AppState,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<MeasurementLedger, sqlx::Error> {
    let pool = &state.database;

    let unshared = sqlx::query_as::<_, UnsharedRow>(measurement_queries::UNSHARED_SQL)
        .bind(workspace_id)
        .bind(now)
        .fetch_one(pool)
        .await?;

    let amplification =
        sqlx::query_as::<_, AmplificationRow>(measurement_queries::AMPLIFICATION_SPEED_SQL)
            .bind(workspace_id)
            .bind(now)
            .fetch_one(pool)
            .await?;

    let suggestions =
        sqlx::query_as::<_, SuggestionsReadRow>(measurement_queries::SUGGESTIONS_READ_SQL)
            .bind(workspace_id)
            .bind(now)
            .fetch_one(pool)
            .await?;

    let outreach_channels =
        sqlx::query_as::<_, OutreachChannelRow>(measurement_queries::OUTREACH_CONVERTS_SQL)
            .bind(workspace_id)
            .bind(now)
            .fetch_all(pool)
            .await?;

    let time_saved = sqlx::query_as::<_, TimeSavedRow>(measurement_queries::TIME_SAVED_SQL)
        .bind(workspace_id)
        .bind(now)
        .fetch_one(pool)
        .await?;

    let plan = sqlx::query_as::<_, PlanFollowedRow>(measurement_queries::PLAN_FOLLOWED_SQL)
        .bind(workspace_id)
        .bind(now)
        .fetch_one(pool)
        .await?;

    let drift = sqlx::query_as::<_, DriftCaughtRow>(measurement_queries::DRIFT_CAUGHT_SQL)
        .bind(workspace_id)
        .bind(now)
        .fetch_one(pool)
        .await?;

    let shows = sqlx::query_as::<_, RoomLeakRow>(measurement_queries::ROOM_LEAK_SQL)
        .bind(workspace_id)
        .bind(now)
        .fetch_all(pool)
        .await?;

    let gathered_channels =
        sqlx::query_as::<_, ChannelFansRow>(measurement_queries::FANS_GATHERED_SQL)
            .bind(workspace_id)
            .bind(now)
            .fetch_all(pool)
            .await?;

    let recovered = sqlx::query_as::<_, RecoveredRow>(measurement_queries::RECOVERY_NOT_GROWTH_SQL)
        .bind(workspace_id)
        .bind(now)
        .fetch_one(pool)
        .await?;

    // Not windowed: the archive is a stock, not a flow, so the query takes no
    // `$2` — bind only the workspace.
    let archive = sqlx::query_as::<_, ArchiveRow>(measurement_queries::ARCHIVE_WORTH_MINING_SQL)
        .bind(workspace_id)
        .fetch_one(pool)
        .await?;

    let source_roi = sqlx::query_as::<_, SourceRoiRow>(measurement_queries::SOURCE_ROI_SQL)
        .bind(workspace_id)
        .bind(now)
        .fetch_all(pool)
        .await?;

    let mut claims: Vec<Claim> = Vec::with_capacity(15);

    // 1. "Nothing the band does goes unshared" — actions ingested ÷ happened.
    claims.push(claim(
        "unshared",
        "Nothing the band does goes unshared",
        "actions ingested ÷ happened",
        Measure::rate(unshared.shared, unshared.happened),
    ));

    // 2. "Amplification is fast" — median minutes action → first artifact.
    claims.push(claim(
        "amplification_speed",
        "Amplification is fast",
        "median minutes action → first artifact",
        if amplification.n == 0 {
            Measure::Unmeasured {
                reason: "no source has produced an artifact yet",
            }
        } else {
            Measure::Minutes {
                median: amplification.median_minutes.unwrap_or(0.0),
                n: amplification.n,
            }
        },
    ));

    // 3. "Suggestions worth reading" — acted on ÷ shown.
    claims.push(claim(
        "suggestions_read",
        "Suggestions worth reading",
        "acted on ÷ shown",
        Measure::rate(suggestions.acted, suggestions.shown),
    ));

    // 4. "Opportunities real" — accepted ÷ shortlisted.
    claims.push(unmeasured(
        "opportunities_real",
        "Opportunities real",
        "accepted ÷ shortlisted",
        "the opportunity shortlist ledger is not on this build yet — Sprint 3 wires it",
    ));

    // 5. "Outreach converts" — replies ÷ sent, per channel.
    let sent: i64 = outreach_channels.iter().map(|row| row.sent).sum();
    let replied: i64 = outreach_channels.iter().map(|row| row.replied).sum();
    let mut outreach = claim(
        "outreach_converts",
        "Outreach converts",
        "replies ÷ sent, per channel",
        Measure::rate(replied, sent),
    );
    outreach.breakdown = outreach_channels
        .iter()
        .map(|row| Breakdown {
            label: row.channel.clone(),
            measure: Measure::rate(row.replied, row.sent),
        })
        .collect();
    claims.push(outreach);

    // 6. "Time saved" — approvals/week × minutes by hand. Only the measured
    //    factor is reported; the multiplier is the chief's model and stays
    //    there.
    claims.push(claim(
        "time_saved",
        "Time saved",
        "approvals/week × minutes by hand",
        Measure::Count {
            value: time_saved.approvals,
            unit: "approvals in 90 days",
        },
    ));

    // 7. "Plan followed" — arc beats delivered ÷ planned, delivered capped per
    //    arc so over-delivery on one cannot cover a slip on another.
    claims.push(claim(
        "plan_followed",
        "Plan followed",
        "arc beats delivered ÷ planned",
        Measure::rate(plan.delivered, plan.planned),
    ));

    // 8. "Drift caught early" — days between slip and told. `slipped_untold`
    //    is always reported: a drift nobody was reminded about is the worst
    //    case of the claim, not a reason to stay silent.
    let mut drift_caught = claim(
        "drift_caught",
        "Drift caught early",
        "days between slip and told",
        if drift.n > 0 {
            Measure::Days {
                median: drift.median_days.unwrap_or(0.0),
                n: drift.n,
            }
        } else {
            Measure::Unmeasured {
                reason: "no assignment has slipped and been reminded yet",
            }
        },
    );
    drift_caught.breakdown = vec![Breakdown {
        label: "slipped, not yet told".to_owned(),
        measure: Measure::Count {
            value: drift.slipped_untold,
            unit: "open assignments past due",
        },
    }];
    claims.push(drift_caught);

    // 9. "The room stops leaking" — scans ÷ room size, per show. The master
    //    variable. One show is one observation, so the per-show floor is 1,
    //    not 20; a show with no admission capacity on record is unmeasured,
    //    and the top line only sums the shows that could be judged.
    let room_leak_measure = if shows.is_empty() {
        Measure::Unmeasured {
            reason: "no completed show in the window",
        }
    } else {
        let measured: Vec<&RoomLeakRow> = shows
            .iter()
            .filter(|row| row.room_size.is_some_and(|room| room > 0))
            .collect();
        if measured.is_empty() {
            Measure::Unmeasured {
                reason: "completed shows in the window have no admission capacity on record",
            }
        } else {
            Measure::rate(
                measured.iter().map(|row| row.scans).sum(),
                measured.iter().filter_map(|row| row.room_size).sum(),
            )
        }
    };
    let mut room_leak = claim(
        "room_leak",
        "The room stops leaking",
        "scans ÷ room size, per show",
        room_leak_measure,
    );
    room_leak.breakdown = shows
        .iter()
        .map(|row| Breakdown {
            label: format!("{} · {}", row.title, row.starts_at.date()),
            measure: match row.room_size {
                Some(room) if room > 0 => Measure::rate_with_floor(row.scans, room, 1),
                _ => Measure::Unmeasured {
                    reason: "no admission capacity on record for this show",
                },
            },
        })
        .collect();
    claims.push(room_leak);

    // 10. "Fans gathered, not just retained" — new consented reachable fans
    //     per channel per month; the control plane divides by 3 and says so.
    let mut fans_gathered = claim(
        "fans_gathered",
        "Fans gathered, not just retained",
        "new consented reachable fans per channel per month",
        Measure::Count {
            value: gathered_channels.iter().map(|row| row.fans).sum(),
            unit: "new consented fans in 90 days",
        },
    );
    fans_gathered.breakdown = gathered_channels
        .iter()
        .map(|row| Breakdown {
            label: row.channel.clone(),
            measure: Measure::Count {
                value: row.fans,
                unit: "fans",
            },
        })
        .collect();
    claims.push(fans_gathered);

    // 11. "Recovery is not growth" — archive confirmations on their own line.
    claims.push(claim(
        "recovery_not_growth",
        "Recovery is not growth",
        "archive confirmations on their own line",
        Measure::Count {
            value: recovered.recovered,
            unit: "archive confirmations in 90 days",
        },
    ));

    // 12. "The archive was worth mining" — confirmations ÷ contacts imported.
    //     Not windowed, so `window_days` is 0 rather than a 90 the query never
    //     applied.
    claims.push(Claim {
        key: "archive_worth_mining",
        claim: "The archive was worth mining",
        measured_as: "confirmations ÷ contacts imported",
        window_days: 0,
        measure: Measure::rate(archive.confirmed, archive.imported),
        breakdown: Vec::new(),
    });

    // 13. "Source ROI honest" — fans still engaged at 30d ÷ acquired, per
    //     channel; only fans old enough to have had 30 days are counted.
    let acquired: i64 = source_roi.iter().map(|row| row.acquired).sum();
    let engaged: i64 = source_roi.iter().map(|row| row.engaged_30d).sum();
    let mut roi = claim(
        "source_roi",
        "Source ROI honest",
        "fans still engaged at 30d ÷ acquired, per channel",
        Measure::rate(engaged, acquired),
    );
    roi.breakdown = source_roi
        .iter()
        .map(|row| Breakdown {
            label: row.channel.clone(),
            measure: Measure::rate(row.engaged_30d, row.acquired),
        })
        .collect();
    claims.push(roi);

    // 14. "Discovery rate (festival)" — became fans of unknown act ÷ attendees.
    claims.push(unmeasured(
        "discovery_rate",
        "Discovery rate (festival)",
        "became fans of unknown act ÷ attendees",
        "needs a festival tenant and a populated event_acts bill; neither exists yet",
    ));

    // 15. "Counterparty pull" — non-tenant artifacts delivered → follow-up
    //     conversations.
    claims.push(unmeasured(
        "counterparty_pull",
        "Counterparty pull",
        "non-tenant artifacts delivered → follow-up conversations",
        "T+7 counterparty reports leave through email outside the reach ledger; no reply join exists yet",
    ));

    Ok(MeasurementLedger {
        observed_at: now,
        window_days: WINDOW_DAYS,
        rate_floor: RATE_FLOOR,
        claims,
    })
}

pub async fn measurement_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match load_measurement_ledger(
        &state,
        state.ops.workspace_id().into_uuid(),
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(ledger) => private_json(StatusCode::OK, ledger),
        Err(error) => {
            tracing::warn!(%error, "could not load measurement ledger");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}
