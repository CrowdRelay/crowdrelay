// Show growth ladder read (P.4) — one show's approve-once ladder: whether the
// operator has said yes to the whole T-21 → T+7 sequence, and where every rung
// of it stands.
//
// Read model, not a pipeline: the evaluator writes the rungs as
// `show_growth`-context actions on the event, `approve_show_ladder` writes the
// approval row, and this only reports the two. The "approve once, rungs keep
// their own evidence gates" semantics live in the domain — this surface shows
// them, it does not recompute them.

/// The ladder over one show: the approval row's state plus every rung the
/// ladder would release, in the order the night meets them.
#[derive(Debug, Serialize)]
pub struct ShowLadderView {
    pub event_id: Uuid,
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    /// The event's own status — a ladder on a `cancelled` night is a different
    /// read than one on a `published` one.
    pub event_status: String,
    /// `approved` while a live approval row exists, `revoked` when the newest
    /// row has been closed, `none` when the operator was never asked. The
    /// state machine is `revoked_at IS NULL` — this read names it, nothing
    /// more.
    pub ladder_state: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub approved_at: Option<OffsetDateTime>,
    pub approved_by: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Rungs in the order they become due — `available_at` is the lever's
    /// scheduled reach time, so the list reads T-21 down to T+7.
    pub rungs: Vec<ShowLadderRung>,
}

#[derive(Debug, Serialize)]
pub struct ShowLadderRung {
    pub action_id: Uuid,
    /// The lever vocabulary (`canonical_link_setup`, `post_show_recap`, …) —
    /// the stable key the panel renders, never a translation.
    pub lever: String,
    pub action_kind: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub available_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub approval_expires_at: Option<OffsetDateTime>,
    /// Who released the rung: `operator:show_ladder` for the approve-once
    /// path, an individual approval, or `null` while it still waits.
    pub approved_by: Option<String>,
    /// The same briefing the approval screen shows for this rung. `None` when
    /// the stored payload is a shape this build cannot parse — an invented
    /// briefing would be worse than none.
    pub briefing: Option<crowdrelay_application::autopilot::ActionBriefing>,
}

#[derive(FromRow)]
struct ShowLadderRungRow {
    action_id: Uuid,
    action_kind: String,
    status: String,
    available_at: OffsetDateTime,
    approval_expires_at: Option<OffsetDateTime>,
    approved_by: Option<String>,
    payload: sqlx::types::Json<serde_json::Value>,
    lever: Option<String>,
}

pub async fn show_ladder(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(event_id) = Uuid::parse_str(&event_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let workspace_id = state.ops.workspace_id().into_uuid();
    let pool = &state.database;

    // The event and its newest approval row: a revoked ladder keeps its row
    // for the ledger, so the live state is whichever row is newest — approved
    // while `revoked_at` is NULL, revoked otherwise, none when no row exists.
    let event = sqlx::query_as::<
        _,
        (
            String,
            OffsetDateTime,
            String,
            Option<OffsetDateTime>,
            Option<String>,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT event.title,
               event.starts_at,
               event.status,
               ladder.approved_at,
               ladder.approved_by,
               ladder.revoked_at
        FROM events AS event
        LEFT JOIN LATERAL (
            SELECT approval.approved_at, approval.approved_by, approval.revoked_at
            FROM viryaos_show_ladder_approvals AS approval
            WHERE approval.workspace_id = event.workspace_id
              AND approval.event_id = event.id
            ORDER BY approval.approved_at DESC, approval.id DESC
            LIMIT 1
        ) AS ladder ON true
        WHERE event.workspace_id = $1
          AND event.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_optional(pool)
    .await;

    let Some((title, starts_at, event_status, approved_at, approved_by, revoked_at)) = (match event
    {
        Ok(row) => row,
        Err(_) => {
            tracing::warn!("could not load show ladder event");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    }) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };

    // The rungs are the show_growth actions already written on the event —
    // every state, not just the live ones: a ladder that hides its finished
    // rungs reads as if nothing happened before the pending one.
    let rows = sqlx::query_as::<_, ShowLadderRungRow>(
        r#"
        SELECT action.id AS action_id,
               action.action_kind,
               action.status,
               action.available_at,
               action.approval_expires_at,
               action.approved_by,
               action.payload,
               action.payload ->> 'lever' AS lever
        FROM viryaos_autopilot_actions AS action
        WHERE action.workspace_id = $1
          AND action.context = 'show_growth'
          AND action.subject_kind = 'event'
          AND action.subject_id = $2
        ORDER BY action.available_at ASC, action.created_at ASC, action.id
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(pool)
    .await;

    let rows = match rows {
        Ok(rows) => rows,
        Err(_) => {
            tracing::warn!("could not load show ladder rungs");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    };

    // Same language resolution the approval panel uses — a briefing renders
    // in the crew's locale or stays in its source language, never fails.
    let crew_locale = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'crew_locale'",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map_or(
        crowdrelay_application::autopilot::BriefingLocale::default(),
        |tag| crowdrelay_application::autopilot::BriefingLocale::from_tag(&tag),
    );

    let rungs = rows
        .into_iter()
        .map(|row| {
            let briefing =
                serde_json::from_value::<crowdrelay_application::autopilot::AutopilotActionPayload>(
                    row.payload.0.clone(),
                )
                .ok()
                .map(|payload| payload.briefing().localized(crew_locale));
            ShowLadderRung {
                action_id: row.action_id,
                lever: row.lever.unwrap_or_default(),
                action_kind: row.action_kind,
                status: row.status,
                available_at: row.available_at,
                approval_expires_at: row.approval_expires_at,
                approved_by: row.approved_by,
                briefing,
            }
        })
        .collect();

    let ladder_state = match (approved_at, revoked_at) {
        (None, _) => "none",
        (Some(_), None) => "approved",
        (Some(_), Some(_)) => "revoked",
    };

    private_json(
        StatusCode::OK,
        ShowLadderView {
            event_id,
            title,
            starts_at,
            event_status,
            ladder_state: ladder_state.to_owned(),
            approved_at,
            approved_by,
            revoked_at,
            rungs,
        },
    )
}
