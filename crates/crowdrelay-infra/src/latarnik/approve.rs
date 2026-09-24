// Approving one invitation.
//
// The shape is `gig_outreach::approve_gig_proposal`, narrowed: one person, one
// letter, and a rule that is mostly refusals. Everything is recomputed from the
// rows at the moment of the click — the standing, the reason, the words — so an
// invitation that stopped being appropriate between the screen and the button
// refuses instead of sending. A photographer who unsubscribed this morning, a
// promoter written to yesterday about a date, a beacon somebody marked
// do-not-contact: each is a refusal here, in the sentence the console shows.

use crowdrelay_domain::latarnik_invite::{InviteReason, compose};

/// What `approve_latarnik_invite` did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InviteOutcome {
    Queued {
        action_id: Uuid,
        recipient: String,
        subject: String,
    },
    /// This idempotency key already produced an invitation. Carries the stored
    /// action and its real status, which may already be `succeeded`.
    Replayed { action_id: Uuid, status: String },
}

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error("latarnik invite database operation failed")]
    Database(#[from] sqlx::Error),
    /// No such contact in this workspace.
    #[error("no such contact")]
    NotFound,
    /// The invitation may not go. Carries the operator-facing sentence, because
    /// the caller's job is to show it rather than translate it.
    #[error("{0}")]
    Refused(String),
}

/// Asks one person, once, whether they also want the dates.
///
/// # Errors
///
/// Refuses with the rule's own sentence, or propagates the database error.
pub async fn approve_latarnik_invite(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
) -> Result<InviteOutcome, InviteError> {
    // A replay answers from the ledger before any evidence is read: the second
    // click of a button that already worked must not depend on the person still
    // being invitable.
    if let Some((action_id, status)) =
        existing_invite_action(pool, workspace_id, idempotency_key).await?
    {
        return Ok(InviteOutcome::Replayed { action_id, status });
    }

    // An invitation to this person that has not finished yet is the same
    // invitation. The action ledger's in-flight subject index refuses the second
    // write anyway; asked here, the operator gets a sentence instead of a
    // unique-violation surfacing as a 500. The once-ever rule cannot catch this
    // on its own: it reads the contact governor, and the governor row is only
    // written when the letter actually leaves.
    if let Some(status) = inflight_invite(pool, workspace_id, beacon_id).await? {
        return Err(InviteError::Refused(format!(
            "an invitation to this contact is already {status} — one ask is the whole budget, and a second would be nagging"
        )));
    }

    let language = contact_language(pool, workspace_id, beacon_id).await?;
    let reason = invite_reason(pool, workspace_id, beacon_id, language, now).await?;
    // The standing is re-read by id, not searched in the capped review page —
    // an id-addressed click must not 404 because the person ranked below it.
    let contact = crate::latarnik::dual_role_contact(
        pool,
        workspace_id,
        beacon_id,
        now,
        reason.is_some(),
    )
    .await?
    .ok_or(InviteError::NotFound)?;

    // The review's own verdict, recomputed here rather than trusted from the
    // screen. `dual_role_review` already ran the domain rule; re-reading its
    // answer keeps one definition of "may we ask" instead of two.
    if !contact.invitable {
        return Err(InviteError::Refused(
            contact
                .hold_reason
                .unwrap_or_else(|| "this invitation cannot go out".to_owned()),
        ));
    }
    let Some(reason) = reason else {
        return Err(InviteError::Refused(
            "nothing concrete to tell them right now — no date in their city, no shared night, \
             no new record"
                .to_owned(),
        ));
    };

    let standing = crowdrelay_domain::latarnik_invite::ContactStanding {
        display_name: contact.display_name.clone(),
        role: contact.role.clone(),
        city: contact.city.clone(),
        relationship_score: contact.relationship_score,
        has_replied: contact.has_replied,
        do_not_contact: contact.do_not_contact,
        accepts_outreach: contact.accepts_outreach,
        days_since_last_contact: contact.days_since_last_contact,
        already_invited: contact.already_invited,
        already_a_fan: contact.hears_the_dates,
        previously_opted_out: contact.previously_opted_out,
        opt_in_pending: contact.opt_in_pending,
    };
    // Belt and braces: the review said yes, and the rule is asked again against
    // the standing the letter is actually composed from. Two answers that
    // disagree would mean the read and the send had drifted apart.
    if let InviteDecision::Hold(hold) = decide(&standing, Some(&reason)) {
        return Err(InviteError::Refused(hold.message()));
    }

    let (beacon_version, contact_email) =
        beacon_pin(pool, workspace_id, beacon_id).await?.ok_or(InviteError::NotFound)?;
    let sender = crate::gig_outreach::sender_identity(pool, workspace_id).await?;
    // A letter with nowhere to point is worse than no letter — refuse rather
    // than embed a dead link.
    let member_area = member_area_url(pool, workspace_id).await?.ok_or_else(|| {
        InviteError::Refused(
            "the member-site address is not configured for this workspace — the letter has nowhere to point"
                .to_owned(),
        )
    })?;
    let letter = compose(&sender, &standing, &reason, &member_area, language);

    let action_id = queue_invite(
        pool,
        workspace_id,
        beacon_id,
        beacon_version,
        &contact_email,
        &contact.display_name,
        &reason_sentence(&reason, language),
        &letter,
        idempotency_key,
        now,
    )
    .await?;

    Ok(InviteOutcome::Queued {
        action_id,
        recipient: contact_email,
        subject: letter.subject,
    })
}

/// The reason to write to this person now, or `None`.
///
/// Read in the order that is most useful to them: a date in their city beats a
/// night you already shared, which beats a record. Nothing is invented — each
/// arm is a row, and no row means no letter.
async fn invite_reason(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    language: crowdrelay_domain::gig_letter::LetterLanguage,
    now: OffsetDateTime,
) -> Result<Option<InviteReason>, sqlx::Error> {
    // A published show in their city, inside the window where telling somebody
    // early is a courtesy rather than a notification.
    let upcoming = sqlx::query_as::<_, (String, OffsetDateTime)>(
        r#"
        SELECT city.name, event.starts_at
        FROM beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id
         AND event.city_id = beacon.city_id
         AND event.status = 'published'
         AND event.starts_at > $3
         AND event.starts_at < $3 + INTERVAL '120 days'
        JOIN cities AS city ON city.id = event.city_id
        WHERE beacon.workspace_id = $1 AND beacon.id = $2
        ORDER BY event.starts_at
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    if let Some((city, starts_at)) = upcoming {
        return Ok(Some(InviteReason::UpcomingShowInTheirCity {
            city,
            when: crowdrelay_domain::gig_letter::letter_date(starts_at, language),
        }));
    }

    // A night this person was part of: any beacon campaign of theirs on a show
    // that happened.
    let shared = sqlx::query_as::<_, (Option<String>, OffsetDateTime)>(
        r#"
        SELECT event.venue, event.starts_at
        FROM beacon_campaigns AS campaign
        JOIN events AS event
          ON event.workspace_id = campaign.workspace_id
         AND event.id = campaign.event_id
        WHERE campaign.workspace_id = $1
          AND campaign.beacon_id = $2
          AND event.starts_at < $3
        ORDER BY event.starts_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    if let Some((venue, starts_at)) = shared {
        let fallback = match language {
            crowdrelay_domain::gig_letter::LetterLanguage::Polish => "tamtym klubie",
            crowdrelay_domain::gig_letter::LetterLanguage::English => "that night",
        };
        return Ok(Some(InviteReason::SharedPastShow {
            venue: venue.unwrap_or_else(|| fallback.to_owned()),
            when: crowdrelay_domain::gig_letter::letter_date(starts_at, language),
        }));
    }

    // A record out in the last three months. Older than that is not news to
    // somebody who covers records for a living.
    let release = sqlx::query_scalar::<_, String>(
        r#"
        SELECT title
        FROM release_plans
        WHERE workspace_id = $1
          AND active
          AND release_at < $2
          AND release_at > $2 - INTERVAL '90 days'
        ORDER BY release_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(release.map(|title| InviteReason::RecentRelease { title }))
}

/// The reason as one sentence for the ledger and the briefing, in the same
/// language the letter it describes was written in.
fn reason_sentence(
    reason: &InviteReason,
    language: crowdrelay_domain::gig_letter::LetterLanguage,
) -> String {
    use crowdrelay_domain::gig_letter::LetterLanguage;
    match (language, reason) {
        (LetterLanguage::Polish, InviteReason::UpcomingShowInTheirCity { city, when }) => {
            format!("koncert w {city} — {when}")
        }
        (LetterLanguage::English, InviteReason::UpcomingShowInTheirCity { city, when }) => {
            format!("show in {city} — {when}")
        }
        (LetterLanguage::Polish, InviteReason::SharedPastShow { venue, when }) => {
            format!("wspólny koncert w {venue} ({when})")
        }
        (LetterLanguage::English, InviteReason::SharedPastShow { venue, when }) => {
            format!("shared night at {venue} ({when})")
        }
        (LetterLanguage::Polish, InviteReason::RecentRelease { title }) => {
            format!("nowe wydawnictwo: {title}")
        }
        (LetterLanguage::English, InviteReason::RecentRelease { title }) => {
            format!("new release: {title}")
        }
    }
}

/// The version and address to pin the send against.
async fn beacon_pin(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String)>(
        "SELECT version, contact_email FROM beacons
         WHERE workspace_id = $1 AND id = $2 AND contact_email IS NOT NULL",
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(pool)
    .await
}

/// The language this person reads, from the city they work in.
///
/// Same rule as the gig letter: the room's country decides, because the letter
/// is for the person receiving it.
async fn contact_language(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
) -> Result<crowdrelay_domain::gig_letter::LetterLanguage, sqlx::Error> {
    let country = sqlx::query_scalar::<_, Option<String>>(
        r#"
        SELECT city.country_code
        FROM beacons AS beacon
        LEFT JOIN cities AS city ON city.id = beacon.city_id
        WHERE beacon.workspace_id = $1 AND beacon.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or_default();
    Ok(crowdrelay_domain::gig_letter::LetterLanguage::for_country(
        &country,
    ))
}

/// Where the invitation points. The stored override only: a shipped default
/// would send an industry contact to somebody else's website, and a missing
/// row must produce no letter rather than a dead link.
async fn member_area_url(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    let Some(base) = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings
         WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?
    .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let path = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'member_area_path'",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or_else(|| crate::tenant_settings::DEFAULT_MEMBER_AREA_PATH.to_owned());
    Ok(Some(format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_matches('/')
    )))
}

/// Has this key already produced an invitation?
async fn existing_invite_action(
    pool: &PgPool,
    workspace_id: Uuid,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, status FROM autopilot_actions
         WHERE workspace_id = $1 AND idempotency_key = $2",
    )
    .bind(workspace_id)
    .bind(idempotency_key.as_str())
    .fetch_optional(pool)
    .await
}

/// Writes the decision and the queued action in one transaction.
///
/// The decision exists for the same reason every other approval writes one: a
/// send nobody can explain is a send nobody should have made, and
/// `ops/trace/{trace_id}` joins decision, action, outbox and delivery. The
/// invitation's own evidence — who they are, why now — is the input snapshot.
#[allow(clippy::too_many_arguments)]
async fn queue_invite(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    beacon_version: i64,
    contact_email: &str,
    contact_name: &str,
    reason: &str,
    letter: &crowdrelay_domain::latarnik_invite::Invite,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
) -> Result<Uuid, InviteError> {
    let payload = AutopilotActionPayload::RequestLatarnikInvite {
        beacon_id: crowdrelay_domain::BeaconId::from_uuid(beacon_id),
        beacon_version,
        recipient_email: contact_email.to_owned(),
        recipient_name: contact_name.to_owned(),
        reason: reason.to_owned(),
        draft: letter.clone(),
    };
    let payload_json = serde_json::to_value(&payload)
        .map_err(|_| InviteError::Refused("the invitation could not be encoded".to_owned()))?;
    let action_kind = payload.action_kind();
    let action_class_value = payload.action_class();
    let action_class = action_class_value.as_str();
    let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
    let decision_key = format!("latarnik.invite:{}", idempotency_key.as_str());

    let mut tx = pool.begin().await?;
    let action_id = Uuid::now_v7();
    let decision_id = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'fan_lifecycle','beacon',$4,
                  'latarnik.invite.approved',10000,'require_approval',
                  'Operator-approved invitation to hear the dates first',
                  $5,$6,$7,$8,$9)
        ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(&decision_key)
    .bind(beacon_id)
    .bind(json!({
        "contact": contact_name,
        "reason": reason,
        "subject": letter.subject,
    }))
    .bind(json!({ "once_ever": true, "relationship_required": true }))
    .bind(&payload_json)
    .bind(now)
    .bind(trace.trace_id().into_uuid())
    .fetch_optional(&mut *tx)
    .await?
    {
        Some(id) => id,
        None => sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM autopilot_decisions
             WHERE workspace_id = $1 AND decision_key = $2",
        )
        .bind(workspace_id)
        .bind(&decision_key)
        .fetch_one(&mut *tx)
        .await?,
    };

    let action_trace = TraceContext::for_action(
        WorkspaceId::from_uuid(workspace_id),
        trace.trace_id(),
        action_id,
        Some(decision_id),
    );
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind,
            subject_kind, subject_id, idempotency_key, payload, status,
            action_class, approved_at, approved_by, available_at,
            trace_id, causation_id
        ) VALUES ($1,$2,$3,'fan_lifecycle',$4,'beacon',$5,$6,$7,
                  'queued',$8,$9,'operator:latarnik_invite_approval',
                  $9 + make_interval(secs => $12::double precision),$10,$11)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(action_kind)
    .bind(beacon_id)
    .bind(idempotency_key.as_str())
    .bind(&payload_json)
    .bind(action_class)
    .bind(now)
    .bind(action_trace.trace_id().into_uuid())
    .bind(action_trace.causation_id().map(|id| id.into_uuid()))
    // O.2: outward sends wait out the hold window, so the operator has
    // something to click when they notice a mistake.
    .bind(f64::from(
        i32::try_from(action_class_value.hold_seconds()).unwrap_or(120),
    ))
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        // Two clicks racing past the in-flight pre-read meet the constraint
        // instead — the idempotency key's or the in-flight subject index's.
        // Both mean the same thing to the operator: the ask already exists.
        if error.as_database_error().is_some_and(|e| e.is_unique_violation()) {
            InviteError::Refused(
                "an invitation to this contact is already queued — one ask is the whole budget"
                    .to_owned(),
            )
        } else {
            InviteError::Database(error)
        }
    })?;
    tx.commit().await?;
    Ok(action_id)
}

/// An invitation to this contact that has not finished yet.
///
/// The same shape the gig letter uses for a city: the ledger's in-flight index
/// is the durable guarantee, and this read is what turns it into a sentence.
async fn inflight_invite(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT status FROM autopilot_actions
        WHERE workspace_id = $1
          AND context = 'fan_lifecycle'
          AND action_kind = 'latarnik.invite.request'
          AND subject_id = $2
          AND status IN ('awaiting_approval', 'queued', 'processing')
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(pool)
    .await
}
