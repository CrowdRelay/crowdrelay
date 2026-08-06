async fn issue_challenge(
    state: &crate::AppState,
    player_id: Uuid,
    headers: &HeaderMap,
    payload: Result<Json<ChallengeRequest>, JsonRejection>,
) -> Response {
    if !valid_idempotency_key(headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "A valid Idempotency-Key is required.",
        );
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid request.",
            );
        }
    };
    if !valid_drop_id(&payload.drop_id) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid drop.");
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let active = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_drops
            WHERE workspace_id = $1
              AND id = $2
              AND active
              AND exact_lat IS NOT NULL
              AND exact_lng IS NOT NULL
              AND starts_at <= now()
              AND ends_at >= now()
              AND (
                  SELECT count(*)
                  FROM area_claims
                  WHERE area_claims.workspace_id = area_drops.workspace_id
                    AND area_claims.drop_id = area_drops.id
              ) < max_claims
        )
        "#,
    )
    .bind(workspace_id)
    .bind(&payload.drop_id)
    .fetch_one(state.ticketing.pool())
    .await;
    match active {
        Ok(true) => {}
        Ok(false) => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_INACTIVE",
                "This drop is not active.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA challenge drop lookup failed");
            return temporary();
        }
    }

    let recent = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM area_challenges
        WHERE workspace_id = $1
          AND player_id = $2
          AND issued_at > now() - ($3::bigint * interval '1 second')
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(CHALLENGE_WINDOW_SECONDS)
    .fetch_one(state.ticketing.pool())
    .await;
    match recent {
        Ok(count) if count >= MAX_CHALLENGES_PER_WINDOW => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "Too many attempts. Try again later.",
            );
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, "AREA challenge rate lookup failed");
            return temporary();
        }
    }

    let token = challenge_token();
    let issued_at = OffsetDateTime::now_utc();
    let expires_at = issued_at + time::Duration::seconds(CHALLENGE_LIFETIME_SECONDS);
    let inserted = sqlx::query(
        r#"
        INSERT INTO area_challenges (
            workspace_id, player_id, drop_id, token_hash, issued_at, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(&payload.drop_id)
    .bind(token_hash(&token))
    .bind(issued_at)
    .bind(expires_at)
    .execute(state.ticketing.pool())
    .await;
    if let Err(error) = inserted {
        tracing::error!(%error, "AREA challenge insert failed");
        return temporary();
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ChallengeResponse {
            ok: true,
            challenge: token,
            issued_at: epoch_millis(issued_at),
            expires_at: epoch_millis(expires_at),
            min_samples: u32::try_from(CHALLENGE_MIN_SAMPLES).unwrap_or(3),
            max_samples: u32::try_from(CHALLENGE_MAX_SAMPLES).unwrap_or(8),
            min_duration_ms: u32::try_from(CHALLENGE_MIN_DURATION_MS).unwrap_or(6_000),
        }),
    )
        .into_response()
}

pub async fn me_challenge(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ChallengeRequest>, JsonRejection>,
) -> Response {
    match mobile_player(&state, &headers).await {
        Ok(Some(player_id)) => issue_challenge(&state, player_id, &headers, payload).await,
        Ok(None) => error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_REQUIRED",
            "Sign in required.",
        ),
        Err(error) => {
            tracing::warn!(%error, "AREA fan session lookup failed");
            temporary()
        }
    }
}

pub async fn internal_challenge(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ChallengeRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    match player_exists(&state, player_id).await {
        Ok(true) => issue_challenge(&state, player_id, &headers, payload).await,
        Ok(false) => error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA player lookup failed");
            temporary()
        }
    }
}

fn valid_samples(samples: &[PositionSample]) -> bool {
    samples.len() >= CHALLENGE_MIN_SAMPLES
        && samples.len() <= CHALLENGE_MAX_SAMPLES
        && samples.iter().all(|sample| {
            sample.lat.is_finite()
                && (-90.0..=90.0).contains(&sample.lat)
                && sample.lng.is_finite()
                && (-180.0..=180.0).contains(&sample.lng)
                && sample.accuracy.is_finite()
                && (0.0..=10_000.0).contains(&sample.accuracy)
                && i64::try_from(sample.captured_at).is_ok()
        })
}

fn to_radians(value: f64) -> f64 {
    value * std::f64::consts::PI / 180.0
}

fn distance_meters(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let earth_radius = 6_371_000.0;
    let delta_lat = to_radians(lat2 - lat1);
    let delta_lng = to_radians(lng2 - lng1);
    let a = (delta_lat / 2.0).sin().powi(2)
        + to_radians(lat1).cos() * to_radians(lat2).cos() * (delta_lng / 2.0).sin().powi(2);
    earth_radius * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        let left = values.get(middle.checked_sub(1)?)?;
        let right = values.get(middle)?;
        Some((left + right) / 2.0)
    } else {
        values.get(middle).copied()
    }
}

fn challenge_time_valid(challenge: &LockedChallenge, samples: &[PositionSample]) -> bool {
    let issued_ms = i64::try_from(epoch_millis(challenge.issued_at)).unwrap_or(i64::MAX);
    let expires_ms = i64::try_from(epoch_millis(challenge.expires_at)).unwrap_or(i64::MIN);
    let now_ms = i64::try_from(epoch_millis(OffsetDateTime::now_utc())).unwrap_or(i64::MAX);
    let mut times = samples
        .iter()
        .filter_map(|sample| i64::try_from(sample.captured_at).ok())
        .collect::<Vec<_>>();
    times.sort_unstable();
    let Some(first) = times.first().copied() else {
        return false;
    };
    let Some(last) = times.last().copied() else {
        return false;
    };
    let upper_bound = now_ms
        .saturating_add(SAMPLE_CLOCK_TOLERANCE_MS)
        .min(expires_ms.saturating_add(SAMPLE_CLOCK_TOLERANCE_MS));
    times.iter().all(|captured| {
        *captured >= issued_ms - SAMPLE_CLOCK_TOLERANCE_MS && *captured <= upper_bound
    }) && last - first >= CHALLENGE_MIN_DURATION_MS
}

fn location_evaluation(drop: &DropRow, samples: &[PositionSample]) -> Result<i32, &'static str> {
    let (Some(exact_lat), Some(exact_lng)) = (drop.exact_lat, drop.exact_lng) else {
        return Err("DROP_INACTIVE");
    };
    let accurate = samples
        .iter()
        .filter(|sample| sample.accuracy <= MAX_ACCURACY_METERS)
        .map(|sample| {
            let distance = distance_meters(sample.lat, sample.lng, exact_lat, exact_lng);
            let tolerance = (sample.accuracy * 0.35).min(15.0);
            (
                distance,
                f64::from(drop.radius_meters) + tolerance,
                sample.accuracy,
            )
        })
        .collect::<Vec<_>>();
    if accurate.len() < CHALLENGE_MIN_SAMPLES {
        return Err("LOW_ACCURACY");
    }
    let inside = accurate
        .iter()
        .filter(|(distance, allowed, _)| distance <= allowed)
        .count();
    let median_distance =
        median(accurate.iter().map(|item| item.0).collect()).ok_or("NOT_ENOUGH_SAMPLES")?;
    let median_accuracy =
        median(accurate.iter().map(|item| item.2).collect()).ok_or("NOT_ENOUGH_SAMPLES")?;
    let allowed = f64::from(drop.radius_meters) + (median_accuracy * 0.35).min(15.0);
    if inside < CHALLENGE_MIN_SAMPLES || median_distance > allowed {
        return Err("OUTSIDE_ZONE");
    }
    let bounded = median_distance.round().clamp(0.0, f64::from(i32::MAX));
    Ok(bounded as i32)
}
