// Predicted show cost against settled show cost.
//
// Two writes in a fixed order. The HTTP layer only checks transport shape: what
// the model predicts is the domain's, what is stored and how the variance is
// derived is the repository's, and nothing here computes money.

/// Money a single show can plausibly involve, in minor units. Ten million
/// złoty is not a gig, it is a typo or a unit mistake, and a settlement that
/// large would drag the model's calibration off on its own.
const MAX_SHOW_MONEY_MINOR: i64 = 1_000_000_000;

/// A band does not drive to the other side of the planet for a show.
const MAX_SHOW_DISTANCE_KM: u32 = 20_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowCostPredictionRequest {
    distance_km: Option<u32>,
    nights_away: Option<u8>,
    offered_fee_minor: i64,
    #[serde(default)]
    application_fee_minor: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowCostSettlementRequest {
    transport_minor: i64,
    accommodation_minor: i64,
    per_diem_minor: i64,
    overhead_minor: i64,
    #[serde(default)]
    other_minor: i64,
    fee_received_minor: i64,
    settled_by: String,
}

const fn plausible_money(value: i64) -> bool {
    value >= 0 && value <= MAX_SHOW_MONEY_MINOR
}

/// Freezes what the model says this show will cost, with the rates it used.
///
/// Called while the show is still ahead. Recomputing the estimate at settlement
/// time would score today's model against itself, which always passes, so the
/// prediction is written once and never revised.
pub async fn freeze_show_cost_prediction(
    State(state): State<AppState>,
    Path(event_id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<ShowCostPredictionRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !plausible_money(request.offered_fee_minor)
        || !plausible_money(request.application_fee_minor)
        || request
            .distance_km
            .is_some_and(|km| km > MAX_SHOW_DISTANCE_KM)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .freeze_show_cost_prediction(
            state.ops.workspace_id(),
            FreezeShowCostPrediction {
                event_id: EventId::from_uuid(event_id),
                distance_km: request.distance_km,
                nights_away: request.nights_away,
                offered_fee_minor: request.offered_fee_minor,
                application_fee_minor: request.application_fee_minor,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Records what the show actually cost and scores the model against it.
///
/// Returns 404 when no prediction was frozen: there is no honest way to score a
/// model against a show it was never asked about, and inventing a retrospective
/// estimate is the one thing this whole loop exists to avoid.
pub async fn settle_show_cost(
    State(state): State<AppState>,
    Path(event_id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<ShowCostSettlementRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let settled_by = request.settled_by.trim().to_owned();
    let lines = [
        request.transport_minor,
        request.accommodation_minor,
        request.per_diem_minor,
        request.overhead_minor,
        request.other_minor,
        request.fee_received_minor,
    ];
    if settled_by.is_empty()
        || settled_by.chars().count() > 120
        || !lines.iter().copied().all(plausible_money)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .settle_show_cost(
            state.ops.workspace_id(),
            SettleShowCost {
                event_id: EventId::from_uuid(event_id),
                settled: SettledShowCost {
                    transport_minor: request.transport_minor,
                    accommodation_minor: request.accommodation_minor,
                    per_diem_minor: request.per_diem_minor,
                    overhead_minor: request.overhead_minor,
                    other_minor: request.other_minor,
                    fee_received_minor: request.fee_received_minor,
                },
                settled_by,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Every show the model was asked about, and how wrong it turned out to be.
///
/// A show with no settlement carries no verdict rather than a neutral one, and
/// a drifting show names the line that moved the most money together with what
/// an operator changes about it.
pub async fn show_economics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_show_cost_ledger(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(shows) => private_json(StatusCode::OK, ShowEconomicsResponse { shows }),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

#[derive(Debug, Serialize)]
struct ShowEconomicsResponse {
    shows: Vec<ShowCostLedgerEntry>,
}
