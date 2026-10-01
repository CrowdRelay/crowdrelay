// The beacon human-send lane — the fallback for a capability nobody
// advertises.
//
// `beacon.outreach` and `beacon.invite_batch` sit in PENDING_ROUTE: no n8n
// route exists, so even when the capability gate fails open the emitted
// event dies at the bridge, and when a registry is live the asks park as
// `awaiting_executor` forever. Either way the verified partner the decision
// was about never hears anything.
//
// The lane hands the same action to the operator instead. `prepare` runs
// the identical verification, contact-window reservation, campaign touch
// and link minting the executor arm performs — the partner's earned guards
// do not loosen because a person carries the words — then claims the action
// for `operator-console`. `sent` files the ordinary terminal receipt, so a
// hand-sent ask carries the same evidence an executor-completed one does.
// The letter goes out from the operator's own client: first contact with a
// partner is never the system's to make unattended, and a lane pretending
// otherwise is exactly what parked it.

/// The `executor_id` the human lane claims and reports under. Claims, reports
/// and the executor circuit all key on it, so `operator-console` shows up in
/// the same evidence columns an n8n executor would — distinguishable by name.
pub const OPERATOR_EXECUTOR_ID: &str = "operator-console";

/// The action kinds this lane serves. Discovery is deliberately absent: a
/// discovery ask emits a research event, not a letter a person can send by
/// hand, so there is nothing here to hand over.
const BEACON_ASK_KINDS: [&str; 2] = [
    "beacon.outreach.request",
    "beacon.invite_batch.request",
];

/// Where one ask stands in the human lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeaconAskLaneState {
    /// Approved, dispatched, and parked on the missing capability — nothing
    /// was ever prepared or sent.
    Parked,
    /// `prepare` ran: links minted, claim held by the operator, letter
    /// written. Awaiting the operator's send + `sent` mark.
    Prepared,
    /// An executor-less workspace did emit the event and the action was
    /// marked `succeeded` at dispatch — but every delivery died `dead`, so
    /// the ask never reached the partner. The succeeded status is premature;
    /// the operator's send plus the receipt it files is the correction. The
    /// same state covers actions the receipt-gap sweep already moved to
    /// `unknown` for exactly the same reason.
    Unreached,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BeaconAskQueueItem {
    pub action_id: Uuid,
    /// `outreach` or `invite_batch` — the payload's own phrasing minus the
    /// request suffix.
    pub kind: String,
    pub beacon_id: Uuid,
    pub beacon_name: String,
    pub beacon_kind: Option<String>,
    pub contact_email: String,
    pub event_id: Uuid,
    pub event_title: String,
    pub event_slug: String,
    #[serde(with = "time::serde::rfc3339")]
    pub event_starts_at: OffsetDateTime,
    /// The campaign phase for outreach asks; `None` on invite batches.
    pub phase: Option<String>,
    /// Codes on offer for invite batches; `None` on outreach asks.
    pub requested_count: Option<i64>,
    pub lane_state: BeaconAskLaneState,
    /// When the operator claimed the ask (`prepared`), if they did.
    #[serde(with = "time::serde::rfc3339::option")]
    pub operator_claimed_at: Option<OffsetDateTime>,
}

/// The letter `prepare` minted. The operator reads `to`, copies `subject` +
/// `body` into their own client, sends, then marks `sent`. Links inside the
/// body are already live — `prepare` minted them before showing them.
#[derive(Clone, Debug, serde::Serialize)]
pub struct BeaconAskPrepared {
    pub action_id: Uuid,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub beacon_name: String,
    pub event_title: String,
}

/// The claim token `sent` needs to file the receipt against the operator's
/// claim. Read from the claim row inside the handler's transaction so a
/// retry always reports against the current attempt.
pub struct BeaconAskSendClaim {
    pub claim_token: Uuid,
    /// The claim already closed `succeeded` — the receipt dedupe answers
    /// replayed and nothing else moves.
    pub already_reported: bool,
}

#[derive(FromRow)]
struct QueueActionRow {
    id: Uuid,
    action_kind: String,
    status: String,
    last_error_kind: Option<String>,
    payload: Value,
    operator_claim_status: Option<String>,
    operator_claimed_at: Option<OffsetDateTime>,
    operator_sent_at: Option<OffsetDateTime>,
}

/// Whether the action's real emission ever reached a live delivery. `true`
/// also when no delivery row exists at all — the bridge being unregistered
/// and the bridge refusing are the same outcome for the partner.
async fn emission_never_delivered(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: Uuid,
) -> Result<bool, RepositoryError> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM autopilot_action_emissions emission
            WHERE emission.workspace_id = $1
              AND emission.action_id = $2
              AND emission.outbox_event_id IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM webhook_deliveries delivery
                  WHERE delivery.workspace_id = emission.workspace_id
                    AND delivery.outbox_event_id = emission.outbox_event_id
                    AND delivery.status IN ('pending','processing','delivered')
              )
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)
}

/// The emission marker the human lane writes. The receipt path requires one
/// `autopilot_action_emissions` row per reportable action; the lane emits no
/// outbox event, so the marker deliberately carries `outbox_event_id = NULL` —
/// `claim_execution` refuses it (`IS NOT NULL`), which is correct: no
/// executor may take over work the operator holds.
async fn ensure_operator_emission_marker(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: Uuid,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        INSERT INTO autopilot_action_emissions (
            workspace_id, action_id, emission_key, outbox_event_id
        ) VALUES ($1, $2, $3, NULL)
        ON CONFLICT (workspace_id, emission_key) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("autopilot-action:{action_id}:operator-send"))
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// `GET` the queue: every beacon ask whose send needs a person — parked on
/// the unadvertised capability, claimed-but-not-yet-sent by the operator, or
/// emitted into a route that delivered nothing.
pub async fn list_beacon_ask_queue(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<BeaconAskQueueItem>, RepositoryError> {
    let rows = sqlx::query_as::<_, QueueActionRow>(
        r#"
        SELECT action.id, action.action_kind, action.status, action.last_error_kind,
               action.payload,
               claim.status AS operator_claim_status,
               claim.claimed_at AS operator_claimed_at,
               sent.occurred_at AS operator_sent_at
        FROM autopilot_actions action
        LEFT JOIN autopilot_execution_claims claim
          ON claim.workspace_id = action.workspace_id
         AND claim.action_id = action.id
         AND claim.executor_id = $2
        LEFT JOIN autopilot_execution_reports sent
          ON sent.workspace_id = action.workspace_id
         AND sent.action_id = action.id
         AND sent.executor_id = $2
         AND sent.status = 'succeeded'
        WHERE action.workspace_id = $1
          AND action.action_kind = ANY($3)
          -- `unknown` is what the receipt-gap sweep calls an emitted ask
          -- whose deliveries all died: the same `unreached` row to the
          -- operator, one ledger word later.
          AND action.status IN ('queued','processing','succeeded','unknown')
        ORDER BY action.id
        LIMIT 200
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(OPERATOR_EXECUTOR_ID)
    .bind(&BEACON_ASK_KINDS[..])
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Delivery check for the emitted classes, batched: an action counts as
    // unreached when a real emission exists and no live delivery does.
    let emitted_ids: Vec<Uuid> = rows
        .iter()
        .filter(|row| matches!(row.status.as_str(), "succeeded" | "unknown"))
        .map(|row| row.id)
        .collect();
    let unreached: std::collections::HashSet<Uuid> = if emitted_ids.is_empty() {
        std::collections::HashSet::new()
    } else {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT DISTINCT emission.action_id
            FROM autopilot_action_emissions emission
            WHERE emission.workspace_id = $1
              AND emission.action_id = ANY($2)
              AND emission.outbox_event_id IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM webhook_deliveries delivery
                  WHERE delivery.workspace_id = emission.workspace_id
                    AND delivery.outbox_event_id = emission.outbox_event_id
                    AND delivery.status IN ('pending','processing','delivered')
              )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&emitted_ids)
        .fetch_all(pool)
        .await
        .map_err(map_sqlx)?
        .into_iter()
        .collect()
    };

    // Parse payloads first so the beacon/event facts load in one batch each.
    struct Parsed {
        row_index: usize,
        beacon_id: Uuid,
        event_id: Uuid,
        phase: Option<String>,
        requested_count: Option<i64>,
    }
    let mut parsed: Vec<Parsed> = Vec::with_capacity(rows.len());
    let mut beacon_ids: Vec<Uuid> = Vec::new();
    let mut event_ids: Vec<Uuid> = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let Ok(payload) =
            serde_json::from_value::<AutopilotActionPayload>(row.payload.clone())
        else {
            continue;
        };
        let fields = match payload {
            AutopilotActionPayload::RequestBeaconOutreach {
                beacon_id,
                event_id,
                phase,
                ..
            } => (
                beacon_id.into_uuid(),
                event_id.into_uuid(),
                Some(
                    match phase {
                        crowdrelay_domain::beacons::BeaconOutreachPhase::Initial => "initial",
                        crowdrelay_domain::beacons::BeaconOutreachPhase::CollaborationFollowUp => {
                            "collaboration_follow_up"
                        }
                        crowdrelay_domain::beacons::BeaconOutreachPhase::LocalPush => "local_push",
                        crowdrelay_domain::beacons::BeaconOutreachPhase::PostShowThanks => {
                            "post_show_thanks"
                        }
                    }
                    .to_owned(),
                ),
                None,
            ),
            AutopilotActionPayload::RequestBeaconInviteBatch {
                beacon_id,
                event_id,
                requested_count,
                ..
            } => (
                beacon_id.into_uuid(),
                event_id.into_uuid(),
                None,
                Some(i64::from(requested_count)),
            ),
            _ => continue,
        };
        beacon_ids.push(fields.0);
        event_ids.push(fields.1);
        parsed.push(Parsed {
            row_index,
            beacon_id: fields.0,
            event_id: fields.1,
            phase: fields.2,
            requested_count: fields.3,
        });
    }

    let beacons: std::collections::HashMap<Uuid, (String, Option<String>, String)> =
        sqlx::query_as::<_, (Uuid, String, Option<String>, String)>(
            r#"
            SELECT id, display_name, beacon_kind, contact_email
            FROM beacons
            WHERE workspace_id = $1 AND id = ANY($2)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&beacon_ids)
        .fetch_all(pool)
        .await
        .map_err(map_sqlx)?
        .into_iter()
        .map(|(id, name, kind, email)| (id, (name, kind, email)))
        .collect();
    let events: std::collections::HashMap<Uuid, (String, String, OffsetDateTime)> =
        sqlx::query_as::<_, (Uuid, String, String, OffsetDateTime)>(
            r#"
            SELECT id, title, slug, starts_at
            FROM events
            WHERE workspace_id = $1 AND id = ANY($2)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&event_ids)
        .fetch_all(pool)
        .await
        .map_err(map_sqlx)?
        .into_iter()
        .map(|(id, title, slug, starts_at)| (id, (title, slug, starts_at)))
        .collect();

    let mut items = Vec::with_capacity(parsed.len());
    for entry in parsed {
        let Some(row) = rows.get(entry.row_index) else {
            continue;
        };
        // A recorded operator send takes the row out of the queue entirely.
        if row.operator_sent_at.is_some() {
            continue;
        }
        let lane_state = if row.operator_claim_status.as_deref() == Some("claimed") {
            BeaconAskLaneState::Prepared
        } else if row.status == "queued"
            && row.last_error_kind.as_deref() == Some("awaiting_executor")
        {
            BeaconAskLaneState::Parked
        } else if matches!(row.status.as_str(), "succeeded" | "unknown")
            && unreached.contains(&row.id)
        {
            BeaconAskLaneState::Unreached
        } else {
            continue;
        };
        // No beacon row means the partner was deleted between decision and
        // now — nothing to send, and nothing for a queue to hold open.
        let Some((beacon_name, beacon_kind, contact_email)) = beacons.get(&entry.beacon_id)
        else {
            continue;
        };
        let Some((event_title, event_slug, event_starts_at)) = events.get(&entry.event_id) else {
            continue;
        };
        let kind = match row.action_kind.as_str() {
            "beacon.outreach.request" => "outreach",
            _ => "invite_batch",
        };
        items.push(BeaconAskQueueItem {
            action_id: row.id,
            kind: kind.to_owned(),
            beacon_id: entry.beacon_id,
            beacon_name: beacon_name.clone(),
            beacon_kind: beacon_kind.clone(),
            contact_email: contact_email.clone(),
            event_id: entry.event_id,
            event_title: event_title.clone(),
            event_slug: event_slug.clone(),
            event_starts_at: *event_starts_at,
            phase: entry.phase,
            requested_count: entry.requested_count,
            lane_state,
            operator_claimed_at: row.operator_claimed_at,
        });
    }
    Ok(items)
}

/// The letter the operator sends. Deterministic on purpose — the same ask in
/// the same phase reads the same way, and the operator edits from a floor,
/// not a blank page. Plain register, one link, no hype.
fn beacon_ask_subject(band: &str, event_title: &str) -> String {
    format!("{band} — {event_title}")
}

/// The facts the outreach letter renders from — one struct because a
/// positional list this long is a typo away from swapping venue and city.
struct OutreachLetterFacts<'a> {
    band: &'a str,
    beacon_name: &'a str,
    phase_key: &'a str,
    event_title: &'a str,
    venue: Option<&'a str>,
    city: Option<&'a str>,
    starts_at: OffsetDateTime,
    show_url: Option<&'a str>,
    ticket_url: Option<&'a str>,
    epk_url: Option<&'a str>,
    now: OffsetDateTime,
}

fn beacon_ask_outreach_body(facts: &OutreachLetterFacts<'_>) -> String {
    let where_when = match (facts.venue, facts.city) {
        (Some(venue), Some(city)) => format!("{venue}, {city}"),
        (Some(venue), None) => venue.to_owned(),
        (None, Some(city)) => city.to_owned(),
        (None, None) => "the venue".to_owned(),
    };
    let date = facts
        .starts_at
        .format(&time::macros::format_description!(
            "[day] [month repr:short] [year]"
        ))
        .unwrap_or_else(|_| facts.starts_at.to_string());
    let link_lines = [
        facts.show_url.map(|url| format!("The night: {url}")),
        facts.ticket_url.map(|url| format!("Tickets: {url}")),
        facts.epk_url.map(|url| format!("More about the band: {url}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");
    let band = facts.band;
    let event_title = facts.event_title;
    let opener = match facts.phase_key {
        "collaboration_follow_up" => format!(
            "Following up on my note about {event_title} — {band} at {where_when} on {date}."
        ),
        "local_push" => {
            let days = (facts.starts_at - facts.now).whole_days();
            format!(
                "{band} plays {event_title} at {where_when} on {date} — {days} days out now."
            )
        }
        "post_show_thanks" => format!(
            "{band} played {event_title} at {where_when} on {date}."
        ),
        _ => format!("{band} plays {event_title} at {where_when} on {date}."),
    };
    let closer = match facts.phase_key {
        "post_show_thanks" => {
            "If a recap or photos fit what you run, they're yours.".to_owned()
        }
        "collaboration_follow_up" | "local_push" => {
            "If a mention still fits, glad to set it up.".to_owned()
        }
        _ => "If it fits what you cover, happy to set something up.".to_owned(),
    };
    format!(
        "Hi {},\n\n{opener}\n\n{link_lines}\n\n{closer}\n\n{band}\n",
        facts.beacon_name
    )
}

fn beacon_ask_invite_body(
    band: &str,
    beacon_name: &str,
    requested_count: u16,
    event_title: &str,
    starts_at: OffsetDateTime,
    show_url: Option<&str>,
) -> String {
    let date = starts_at
        .format(&time::macros::format_description!(
            "[day] [month repr:short] [year]"
        ))
        .unwrap_or_else(|_| starts_at.to_string());
    let link_line = show_url
        .map(|url| format!("The night: {url}\n\n"))
        .unwrap_or_default();
    format!(
        "Hi {beacon_name},\n\n{band} plays {event_title} on {date}.\n\n{link_line}\
         If your people would come, say yes and we issue {requested_count} invite \
         codes on our side — every signup they bring gets counted rather than lost.\n\n\
         {band}\n"
    )
}

/// `POST …/prepare`: turn a parked (or never-delivered) beacon ask into a
/// letter the operator can send. Runs the same verification, contact-window
/// reservation, campaign touch and link minting the executor arm performs —
/// the partner's earned guards do not loosen because a person carries the
/// words. Claims the action for `operator-console` and moves `queued` to
/// `processing`, the same transition the worker's claim performs.
///
/// Refuses (`Conflict`) when the action is not a beacon ask, when the
/// partner re-check fails (declined, deferred, gone cold — the stale-yes
/// guard the executor applies verbatim), or when the operator's claim
/// already closed `succeeded`.
pub async fn prepare_beacon_ask(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
) -> Result<BeaconAskPrepared, RepositoryError> {
    let mut transaction = pool.begin().await.map_err(map_sqlx)?;
    let now = OffsetDateTime::now_utc();
    let row = sqlx::query_as::<_, (String, String, Option<String>, Value)>(
        r#"
        SELECT status, action_kind, last_error_kind, payload
        FROM autopilot_actions
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::NotFound)?;
    if !BEACON_ASK_KINDS.contains(&row.1.as_str()) {
        return Err(RepositoryError::NotFound);
    }
    let (status, _kind, last_error_kind, payload) = row;
    let operator_claim = sqlx::query_scalar::<_, String>(
        r#"
        SELECT status FROM autopilot_execution_claims
        WHERE workspace_id = $1 AND action_id = $2 AND executor_id = $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(OPERATOR_EXECUTOR_ID)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(map_sqlx)?;
    let eligible = match status.as_str() {
        // The parked state, plus a queued row the operator already claimed —
        // a lease sweep can bounce `processing` back and the claim is the
        // evidence that the letter is still theirs to send.
        "queued" => {
            last_error_kind.as_deref() == Some("awaiting_executor")
                || operator_claim.as_deref() == Some("claimed")
        }
        // Re-entry after a partial prepare, or the claim waiting on `sent`.
        "processing" => operator_claim.as_deref() == Some("claimed"),
        // Emitted at dispatch into a route that delivered nothing: the ask
        // never reached the partner, so preparing it for a human send is the
        // correction — never a second send. `unknown` is the same action a
        // receipt-gap sweep older; the receipt resolves it through the
        // legal Unknown→Succeeded edge when the operator marks sent.
        "succeeded" | "unknown" => {
            operator_claim.is_none()
                && emission_never_delivered(&mut transaction, workspace_id, action_id).await?
        }
        _ => false,
    };
    if !eligible {
        return Err(RepositoryError::Conflict);
    }

    let typed_id = AutopilotActionId::from_uuid(action_id);
    let payload = serde_json::from_value::<AutopilotActionPayload>(payload)
        .map_err(|_| RepositoryError::Unexpected)?;
    let prepared = match payload {
        AutopilotActionPayload::RequestBeaconOutreach {
            beacon_id,
            event_id,
            beacon_version,
            phase,
            template_key,
        } => {
            // The campaign touch + contact-window reservation run exactly
            // once per ask: at the first prepare of a parked action, or at
            // dispatch in the executor path. A re-prepare (operator claimed
            // already) and an emitted-but-unreached action both rebuild the
            // letter from existing state — re-touching would inflate
            // `beacon_campaigns.followup_count`, which feeds the decision
            // keys that decide whether the next proposal fires at all.
            let record_touch = status == "queued" && operator_claim.is_none();
            let dispatch = prepare_beacon_outreach(
                &mut transaction,
                workspace_id,
                typed_id,
                beacon_id,
                event_id,
                beacon_version,
                &phase,
                &template_key,
                record_touch,
                now,
            )
            .await?;
            let band = sqlx::query_scalar::<_, String>(
                "SELECT name FROM workspaces WHERE id = $1",
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let subject = beacon_ask_subject(&band, &dispatch.event_title);
            let body = beacon_ask_outreach_body(&OutreachLetterFacts {
                band: &band,
                beacon_name: &dispatch.beacon_name,
                phase_key: dispatch.phase_key,
                event_title: &dispatch.event_title,
                venue: dispatch.event_venue.as_deref(),
                city: dispatch.event_city.as_deref(),
                starts_at: dispatch.event_starts_at,
                show_url: dispatch.show_url.as_deref(),
                ticket_url: dispatch.ticket_url.as_deref(),
                epk_url: dispatch.epk_url.as_deref(),
                now,
            });
            BeaconAskPrepared {
                action_id,
                to: dispatch.contact_email,
                subject,
                body,
                beacon_name: dispatch.beacon_name,
                event_title: dispatch.event_title,
            }
        }
        AutopilotActionPayload::RequestBeaconInviteBatch {
            beacon_id,
            beacon_version,
            event_id,
            requested_count,
        } => {
            let dispatch = prepare_beacon_invite_batch(
                &mut transaction,
                workspace_id,
                typed_id,
                beacon_id,
                beacon_version,
                event_id,
                requested_count,
                now,
            )
            .await?;
            // The batch executor carries no links — the partner distributes
            // codes, not URLs — but the lane's letter still needs one the
            // click spine counts, so the show link mints here and here only.
            let (site_root, first_tenant) =
                beacon_letter_site(&mut transaction, workspace_id).await?;
            let show_url = if first_tenant {
                let direct = site_root
                    .as_deref()
                    .map(|root| format!("{root}/pl/live/{}/", dispatch.event_slug));
                ensure_beacon_action_link(
                    &mut transaction,
                    workspace_id,
                    typed_id,
                    beacon_id,
                    site_root.as_deref(),
                    "show",
                    direct.as_deref(),
                    "invite_batch",
                )
                .await?
                .or(direct)
            } else {
                None
            };
            let band = sqlx::query_scalar::<_, String>(
                "SELECT name FROM workspaces WHERE id = $1",
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let subject = format!(
                "Invite codes for {} — {}",
                dispatch.beacon_name, dispatch.event_title
            );
            let body = beacon_ask_invite_body(
                &band,
                &dispatch.beacon_name,
                dispatch.requested_count,
                &dispatch.event_title,
                dispatch.event_starts_at,
                show_url.as_deref(),
            );
            BeaconAskPrepared {
                action_id,
                to: dispatch.contact_email,
                subject,
                body,
                beacon_name: dispatch.beacon_name,
                event_title: dispatch.event_title,
            }
        }
        // The kind column said beacon but the stored payload did not parse
        // into either ask shape — drift, so fail closed rather than guess.
        _ => return Err(RepositoryError::Conflict),
    };

    ensure_operator_emission_marker(&mut transaction, workspace_id, action_id).await?;
    if status == "queued" {
        sqlx::query(
            r#"
            UPDATE autopilot_actions
            SET status = 'processing',
                started_at = COALESCE(started_at, $3),
                last_error_kind = NULL,
                updated_at = $3
            WHERE workspace_id = $1 AND id = $2 AND status = 'queued'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
    }
    // The operator's claim. Re-prepare rotates the token, which is correct:
    // only the newest letter the operator holds may file the receipt.
    let claim_status = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO autopilot_execution_claims (
            workspace_id, action_id, executor_id, claim_token, status,
            attempt_number, claimed_at
        ) VALUES ($1,$2,$3,$4,'claimed',1,$5)
        ON CONFLICT (workspace_id, action_id, executor_id) DO UPDATE
            SET claim_token = EXCLUDED.claim_token,
                status = 'claimed',
                attempt_number = autopilot_execution_claims.attempt_number + 1,
                provider_reference = NULL,
                error_kind = NULL,
                claimed_at = EXCLUDED.claimed_at,
                completed_at = NULL
            WHERE autopilot_execution_claims.status <> 'succeeded'
        RETURNING status
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(OPERATOR_EXECUTOR_ID)
    .bind(Uuid::now_v7())
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(map_sqlx)?;
    // A claim the update refused to touch is already `succeeded` — the ask
    // was marked sent and is not the lane's to reopen.
    if claim_status.as_deref() != Some("claimed") {
        return Err(RepositoryError::Conflict);
    }
    transaction.commit().await.map_err(map_sqlx)?;
    Ok(prepared)
}

/// `POST …/sent`, first half: verify the operator holds a live claim on a
/// beacon ask and hand back its token. The handler then files the terminal
/// receipt through `record_execution_report` — the identical closing move an
/// executor makes — so the action's evidence chain does not learn a second
/// shape for "a person sent it".
pub async fn beacon_ask_send_claim(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
) -> Result<BeaconAskSendClaim, RepositoryError> {
    let mut transaction = pool.begin().await.map_err(map_sqlx)?;
    let row = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT status, action_kind
        FROM autopilot_actions
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::NotFound)?;
    if !BEACON_ASK_KINDS.contains(&row.1.as_str()) {
        return Err(RepositoryError::NotFound);
    }
    let (status, _kind) = row;
    let claim = sqlx::query_as::<_, (String, Uuid)>(
        r#"
        SELECT status, claim_token FROM autopilot_execution_claims
        WHERE workspace_id = $1 AND action_id = $2 AND executor_id = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(OPERATOR_EXECUTOR_ID)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(map_sqlx)?;
    let Some((claim_status, claim_token)) = claim else {
        return Err(RepositoryError::Conflict);
    };
    // The receipt already landed — a retried `sent` must answer replayed
    // whatever sweeps did to the action row since.
    if claim_status == "succeeded" {
        transaction.commit().await.map_err(map_sqlx)?;
        return Ok(BeaconAskSendClaim {
            claim_token,
            already_reported: true,
        });
    }
    match (status.as_str(), claim_status.as_str()) {
        // A sweep bounced the action back to queued while the operator held
        // the claim — re-assert processing so the receipt's Running→Succeeded
        // transition has a legal edge.
        ("queued", "claimed") => {
            ensure_operator_emission_marker(&mut transaction, workspace_id, action_id).await?;
            sqlx::query(
                r#"
                UPDATE autopilot_actions
                SET status = 'processing',
                    started_at = COALESCE(started_at, $3),
                    updated_at = $3
                WHERE workspace_id = $1 AND id = $2 AND status = 'queued'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id)
            .bind(OffsetDateTime::now_utc())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
        }
        // The parked path and the emitted-but-unreached path both arrive
        // holding a live claim — anything else was never prepared. `unknown`
        // stays `unknown` until the receipt lands: Unknown+Executed resolves
        // to Succeeded on its own edge, no interim write needed.
        ("processing" | "succeeded" | "unknown", "claimed") => {}
        _ => return Err(RepositoryError::Conflict),
    }
    transaction.commit().await.map_err(map_sqlx)?;
    Ok(BeaconAskSendClaim {
        claim_token,
        already_reported: false,
    })
}
