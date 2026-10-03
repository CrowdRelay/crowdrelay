//! Persistence for the Latarnik role: the evidence read from first-party rows,
//! and the person-keyed role it can create.
//!
//! The fan is found by what the system owns about them (`fans`, consents,
//! orders, check-ins, sessions); the role hangs off `persons`, reached through
//! the fan's email identity, so the same human is one `persons` row whether
//! they arrived as a commenter, a fan or a contact. Every statement names
//! `workspace_id` on every table it reads (the tenant-isolation ratchet counts
//! the ones that do not).
//!
//! Recording a candidate asks nobody anything. The ask is a separate, gated
//! step that moves the role to `invited`.

use crowdrelay_domain::latarnik::{FanEvidence, RoleStatus};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum LatarnikError {
    #[error("latarnik database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("no such role")]
    NotFound,
    #[error("the role cannot move from {from} to {to}")]
    IllegalMove {
        from: &'static str,
        to: &'static str,
    },
    #[error("an ended role needs a reason")]
    ReasonRequired,
}

/// One fan and what the system has observed about them. The address stays in
/// infra: it is how the fan is joined to a person, never a field the evaluator
/// or a response can see.
#[derive(Debug)]
pub struct ObservedFan {
    pub fan_id: Uuid,
    pub email: String,
    pub evidence: FanEvidence,
}

#[derive(Debug, FromRow)]
struct EvidenceRow {
    fan_id: Uuid,
    normalized_email: String,
    tenure_days: i64,
    consented: bool,
    active_now: bool,
    active_before: bool,
    distinct_actions_90d: i32,
    attended_show_90d: bool,
    has_purchased: bool,
    qualified_referrals: i64,
    suppressed_elsewhere: bool,
    already_asked: bool,
}

/// Active, non-merged fans and the evidence behind each, oldest first, bounded.
///
/// "Active" here is the account status only; whether the fan is *currently
/// active* in the activation sense is `active_now`, computed with the canonical
/// `fan_has_meaningful_action_between` so this definition cannot drift from the
/// one the counter uses.
///
/// # Errors
///
/// Propagates the database error.
pub async fn load_fan_evidence(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    limit: i64,
) -> Result<Vec<ObservedFan>, LatarnikError> {
    let rows = sqlx::query_as::<_, EvidenceRow>(
        r#"
        WITH RECURSIVE family(root_id, member_id) AS (
            SELECT fan.id, fan.id
            FROM fans fan
            WHERE fan.workspace_id = $1 AND fan.merged_into_fan_id IS NULL
            UNION ALL
            SELECT family.root_id, child.id
            FROM family
            JOIN fans child
              ON child.workspace_id = $1
             AND child.merged_into_fan_id = family.member_id
        )
        SELECT f.id AS fan_id,
               f.normalized_email,
               FLOOR(EXTRACT(EPOCH FROM ($2 - f.created_at)) / 86400.0)::bigint AS tenure_days,
               COALESCE((SELECT consent.granted FROM fan_consents consent
                          WHERE consent.workspace_id = f.workspace_id
                            AND consent.fan_id = f.id
                            AND consent.purpose = 'marketing'
                          ORDER BY consent.recorded_at DESC, consent.id DESC
                          LIMIT 1), false) AS consented,
               fan_has_meaningful_action_between(
                   f.workspace_id, f.id, f.normalized_email,
                   $2 - interval '30 days', $2 + interval '1 second') AS active_now,
               fan_has_meaningful_action_between(
                   f.workspace_id, f.id, f.normalized_email,
                   $2 - interval '60 days', $2 - interval '30 days') AS active_before,
               (
                 (EXISTS (SELECT 1 FROM ticket_orders o
                           WHERE o.workspace_id = f.workspace_id AND o.buyer_email = f.normalized_email
                             AND o.status IN ('paid', 'partially_refunded')
                             AND o.paid_at >= $2 - interval '90 days'))::int
               + (EXISTS (SELECT 1 FROM merch_order_facts m
                           WHERE m.workspace_id = f.workspace_id AND m.fan_id = f.id
                             AND m.confirmed_at >= $2 - interval '90 days'))::int
               + (EXISTS (
                     SELECT 1
                     FROM referral_attributions r
                     JOIN family referrer
                       ON referrer.root_id = f.id
                      AND referrer.member_id = r.referrer_fan_id
                     JOIN family referred
                       ON referred.member_id = r.referred_fan_id
                     WHERE r.workspace_id = f.workspace_id
                       AND r.status = 'qualified'
                       AND r.qualified_at >= $2 - interval '90 days'
                       AND referred.root_id <> referrer.root_id
                       AND canonical_qualified_referral_owner_id(
                             f.workspace_id,referred.root_id
                           ) = referrer.root_id
                 ))::int
               + (EXISTS (SELECT 1 FROM concert_checkins c
                           WHERE c.workspace_id = f.workspace_id AND c.fan_id = f.id
                             AND c.checked_in_at >= $2 - interval '90 days')
                  OR EXISTS (SELECT 1 FROM admission_passes p
                              WHERE p.workspace_id = f.workspace_id AND p.fan_id = f.id
                                AND p.status = 'redeemed'
                                AND p.redeemed_at >= $2 - interval '90 days'))::int
               + (EXISTS (SELECT 1 FROM event_interests i
                           WHERE i.workspace_id = f.workspace_id AND i.fan_id = f.id
                             AND i.created_at >= $2 - interval '90 days'))::int
               + (EXISTS (SELECT 1 FROM fan_sessions s
                           WHERE s.workspace_id = f.workspace_id AND s.fan_id = f.id
                             AND s.revoked_at IS NULL
                             AND s.last_seen_at >= $2 - interval '90 days'))::int
               ) AS distinct_actions_90d,
               (EXISTS (SELECT 1 FROM concert_checkins c
                         WHERE c.workspace_id = f.workspace_id AND c.fan_id = f.id
                           AND c.checked_in_at >= $2 - interval '90 days')
                OR EXISTS (SELECT 1 FROM admission_passes p
                            WHERE p.workspace_id = f.workspace_id AND p.fan_id = f.id
                              AND p.status = 'redeemed'
                              AND p.redeemed_at >= $2 - interval '90 days')) AS attended_show_90d,
               (EXISTS (SELECT 1 FROM ticket_orders o
                         WHERE o.workspace_id = f.workspace_id AND o.buyer_email = f.normalized_email
                           AND o.status IN ('paid', 'partially_refunded'))
                OR EXISTS (SELECT 1 FROM merch_order_facts m
                            WHERE m.workspace_id = f.workspace_id AND m.fan_id = f.id)) AS has_purchased,
               (
                 SELECT count(DISTINCT referred.root_id)
                 FROM referral_attributions r
                 JOIN family referrer
                   ON referrer.root_id = f.id
                  AND referrer.member_id = r.referrer_fan_id
                 JOIN family referred
                   ON referred.member_id = r.referred_fan_id
                 WHERE r.workspace_id = f.workspace_id
                   AND r.status = 'qualified'
                   AND referred.root_id <> referrer.root_id
                   AND canonical_qualified_referral_owner_id(
                         f.workspace_id,referred.root_id
                       ) = referrer.root_id
               ) AS qualified_referrals,
               EXISTS (SELECT 1 FROM outreach_targets t
                        WHERE t.workspace_id = f.workspace_id
                          AND lower(btrim(t.contact_email)) = f.normalized_email
                          AND t.do_not_contact) AS suppressed_elsewhere,
               (
                 EXISTS (SELECT 1
                           FROM person_identities pi
                           JOIN latarnik_roles lr
                             ON lr.workspace_id = pi.workspace_id AND lr.person_id = pi.person_id
                          WHERE pi.workspace_id = f.workspace_id
                            AND pi.kind = 'email' AND pi.value = f.normalized_email)
                 OR EXISTS (
                     SELECT 1
                     FROM person_identities pi
                     JOIN fan_advocacy_opportunities opportunity
                       ON opportunity.workspace_id = pi.workspace_id
                      AND opportunity.person_id = pi.person_id
                     WHERE pi.workspace_id = f.workspace_id
                       AND pi.kind = 'email'
                       AND pi.platform IS NULL
                       AND pi.value = f.normalized_email
                 )
               ) AS already_asked
        FROM fans f
        WHERE f.workspace_id = $1
          AND f.status = 'active'
          AND f.deleted_at IS NULL
          AND f.merged_into_fan_id IS NULL
        ORDER BY f.created_at, f.id
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ObservedFan {
            fan_id: row.fan_id,
            email: row.normalized_email,
            evidence: FanEvidence {
                account_open: true,
                consented: row.consented,
                tenure_days: row.tenure_days,
                active_now: row.active_now,
                active_before: row.active_before,
                distinct_actions_90d: u8::try_from(row.distinct_actions_90d.clamp(0, 255))
                    .unwrap_or(u8::MAX),
                attended_show_90d: row.attended_show_90d,
                has_purchased: row.has_purchased,
                qualified_referrals: u32::try_from(row.qualified_referrals).unwrap_or(u32::MAX),
                suppressed_in_any_role: row.suppressed_elsewhere,
                already_asked: row.already_asked,
            },
        })
        .collect())
}

async fn resolve_person_for_email(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    email: &str,
) -> Result<Uuid, LatarnikError> {
    if let Some(person_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT person_id FROM person_identities
         WHERE workspace_id = $1 AND kind = 'email' AND value = $2 AND platform IS NULL",
    )
    .bind(workspace_id)
    .bind(email)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(person_id);
    }

    let person_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO persons (workspace_id) VALUES ($1) RETURNING id",
    )
    .bind(workspace_id)
    .fetch_one(&mut **tx)
    .await?;
    let claimed = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO person_identities (workspace_id, person_id, kind, platform, value, source)
         VALUES ($1, $2, 'email', NULL, $3, 'fan_evidence_sweep')
         ON CONFLICT (workspace_id, kind, COALESCE(platform, ''), value) DO NOTHING
         RETURNING person_id",
    )
    .bind(workspace_id)
    .bind(person_id)
    .bind(email)
    .fetch_optional(&mut **tx)
    .await?;
    if claimed.is_some() {
        return Ok(person_id);
    }

    // Lost a race for the address: discard the empty person and use the
    // identity winner. Nothing else can point at this just-created row yet.
    sqlx::query("DELETE FROM persons WHERE workspace_id = $1 AND id = $2")
        .bind(workspace_id)
        .bind(person_id)
        .execute(&mut **tx)
        .await?;
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT person_id FROM person_identities
         WHERE workspace_id = $1 AND kind = 'email' AND value = $2 AND platform IS NULL",
    )
    .bind(workspace_id)
    .bind(email)
    .fetch_one(&mut **tx)
    .await?)
}

/// Records the lightest possible advocacy mission: ask one ready fan to carry
/// one tracked referral to one relevant person.
///
/// This is deliberately not a Latarnik role and it sends nothing. The existing
/// fan lifecycle owns the later message, cooldown, approval and delivery.
/// Person-keying means fan merges cannot duplicate or erase the one-ask budget.
///
/// Returns the opportunity id only when this call created it.
///
/// # Errors
///
/// Propagates the database error; the transaction rolls back whole.
pub async fn record_referral_opportunity(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    evidence: &FanEvidence,
    now: OffsetDateTime,
) -> Result<Option<Uuid>, LatarnikError> {
    let evidence_json = serde_json::to_value(evidence)
        .map_err(|_| LatarnikError::Database(sqlx::Error::Protocol("evidence".into())))?;
    let mut tx = pool.begin().await?;
    let person_id = resolve_person_for_email(&mut tx, workspace_id, email).await?;
    let created = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fan_advocacy_opportunities
             (workspace_id, person_id, kind, source, evidence, ready_at)
         VALUES ($1, $2, 'personal_referral', 'fan_evidence_sweep', $3, $4)
         ON CONFLICT (workspace_id, person_id, kind) DO NOTHING
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(person_id)
    .bind(evidence_json)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// Records a fan as a Latarnik candidate. Finds or creates the person through
/// the fan's email identity; one row per person, so a fan detected twice, or one
/// whose role was already ended, changes nothing. Returns the role id only when
/// this call created it.
///
/// # Errors
///
/// Propagates the database error; the transaction rolls back whole.
pub async fn record_candidate(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    evidence: &FanEvidence,
    now: OffsetDateTime,
) -> Result<Option<Uuid>, LatarnikError> {
    let evidence_json = serde_json::to_value(evidence)
        .map_err(|_| LatarnikError::Database(sqlx::Error::Protocol("evidence".into())))?;
    let mut tx = pool.begin().await?;
    let person_id = resolve_person_for_email(&mut tx, workspace_id, email).await?;
    let created = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO latarnik_roles (workspace_id, person_id, source, evidence, candidate_at)
         VALUES ($1, $2, 'fan_evidence_sweep', $3, $4)
         ON CONFLICT (workspace_id, person_id) DO NOTHING
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(person_id)
    .bind(evidence_json)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// Moves a role along the status machine, stamping the fact that put it there.
/// An ended role needs a reason; an illegal move is refused, not coerced.
///
/// # Errors
///
/// [`LatarnikError::NotFound`], [`LatarnikError::IllegalMove`],
/// [`LatarnikError::ReasonRequired`], or the database error.
pub async fn transition_role(
    pool: &PgPool,
    workspace_id: Uuid,
    role_id: Uuid,
    next: RoleStatus,
    reason: Option<&str>,
    now: OffsetDateTime,
) -> Result<(), LatarnikError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query_scalar::<_, String>(
        "SELECT status FROM latarnik_roles WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(role_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(LatarnikError::NotFound)?;
    let current = RoleStatus::parse(&current).ok_or(LatarnikError::NotFound)?;
    if !current.may_become(next) {
        return Err(LatarnikError::IllegalMove {
            from: current.as_str(),
            to: next.as_str(),
        });
    }
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    if next == RoleStatus::Revoked && reason.is_none() {
        return Err(LatarnikError::ReasonRequired);
    }
    sqlx::query(
        "UPDATE latarnik_roles
         SET status = $3,
             status_reason = COALESCE($4, status_reason),
             invited_at   = CASE WHEN $3 = 'invited' THEN COALESCE(invited_at, $5) ELSE invited_at END,
             activated_at = CASE WHEN $3 = 'active'  THEN COALESCE(activated_at, $5) ELSE activated_at END,
             revoked_at   = CASE WHEN $3 = 'revoked' THEN $5 ELSE revoked_at END,
             updated_at = $5
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(role_id)
    .bind(next.as_str())
    .bind(reason)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, FromRow, serde::Serialize)]
pub struct RoleView {
    pub id: Uuid,
    pub status: String,
    pub source: String,
    pub evidence: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub candidate_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub invited_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub activated_at: Option<OffsetDateTime>,
    /// True when the same person is also a first-party fan — the plan's
    /// "one person visibly holds fan and Latarnik roles at once".
    pub is_also_a_fan: bool,
}

/// The roles, newest candidate first. No address and no name: the evidence is
/// counts and booleans, and `is_also_a_fan` says what the operator needs to
/// know about overlap without exposing who.
///
/// # Errors
///
/// Propagates the database error.
pub async fn list_roles(
    pool: &PgPool,
    workspace_id: Uuid,
    limit: i64,
) -> Result<Vec<RoleView>, LatarnikError> {
    Ok(sqlx::query_as::<_, RoleView>(
        "SELECT lr.id, lr.status, lr.source, lr.evidence,
                lr.candidate_at, lr.invited_at, lr.activated_at,
                EXISTS (SELECT 1
                          FROM person_identities pi
                          JOIN fans f
                            ON f.workspace_id = pi.workspace_id
                           AND f.normalized_email = pi.value
                         WHERE pi.workspace_id = lr.workspace_id
                           AND pi.person_id = lr.person_id
                           AND pi.kind = 'email'
                           AND f.status = 'active') AS is_also_a_fan
         FROM latarnik_roles lr
         WHERE lr.workspace_id = $1
         ORDER BY lr.candidate_at DESC, lr.id
         LIMIT $2",
    )
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// What a signed-in fan sees of their own Latarnik role. Counts and states
/// only: the evidence the system used to detect them is not shown to the person
/// it describes, and nothing here names anyone else.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MyRoleState {
    /// No role row, or one the band has only noted internally (`candidate`):
    /// nothing has been asked, so there is nothing to show.
    None,
    /// The band has asked; the person has not answered.
    Invited,
    Active,
    Paused,
    /// Ended, by either side. Terminal; shown so the Signal surface never
    /// offers the role again.
    Ended,
}

/// What the person may answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MyAnswer {
    Accept,
    Decline,
    Pause,
    Resume,
    Leave,
}

/// The fan's own role, resolved from their session through their email
/// identity. `Ok(None)` is an unauthenticated session; a fan with no role is
/// `Some(MyRoleState::None)`.
///
/// # Errors
///
/// Propagates the database error.
pub async fn my_role(
    pool: &PgPool,
    workspace_id: Uuid,
    session_token: &str,
) -> Result<Option<(MyRoleState, Option<Uuid>)>, LatarnikError> {
    let row = sqlx::query_as::<_, (Option<Uuid>, Option<String>)>(
        "SELECT lr.id, lr.status
         FROM fan_sessions session
         JOIN fans fan
           ON fan.workspace_id = session.workspace_id AND fan.id = session.fan_id
         LEFT JOIN person_identities pi
           ON pi.workspace_id = fan.workspace_id
          AND pi.kind = 'email' AND pi.platform IS NULL
          AND pi.value = fan.normalized_email
         LEFT JOIN latarnik_roles lr
           ON lr.workspace_id = pi.workspace_id AND lr.person_id = pi.person_id
         WHERE session.workspace_id = $1
           AND session.session_token_hash = digest($2, 'sha256')
           AND session.revoked_at IS NULL
           AND session.expires_at > now()
           AND fan.status = 'active'
           AND fan.deleted_at IS NULL
         LIMIT 1",
    )
    .bind(workspace_id)
    .bind(session_token)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(role_id, status)| {
        let state = match status.as_deref().and_then(RoleStatus::parse) {
            None | Some(RoleStatus::Candidate) => MyRoleState::None,
            Some(RoleStatus::Invited) => MyRoleState::Invited,
            Some(RoleStatus::Active) => MyRoleState::Active,
            Some(RoleStatus::Paused) => MyRoleState::Paused,
            Some(RoleStatus::Revoked) => MyRoleState::Ended,
        };
        (state, role_id.filter(|_| state != MyRoleState::None))
    }))
}

/// The person's answer to the role. Runs through the same status machine as
/// the operator's decision, so a person cannot skip the invitation any more
/// than the band can: accepting needs `invited`, declining needs `invited`,
/// pause/resume/leave need the matching live state. A decline is an ended role
/// with a recorded reason — terminal, so one ask is one ask.
///
/// # Errors
///
/// [`LatarnikError::NotFound`] for an unauthenticated session or a fan with no
/// asked role; [`LatarnikError::IllegalMove`] when the answer does not fit the
/// role's state.
pub async fn answer_my_role(
    pool: &PgPool,
    workspace_id: Uuid,
    session_token: &str,
    answer: MyAnswer,
    now: OffsetDateTime,
) -> Result<RoleStatus, LatarnikError> {
    let Some((state, Some(role_id))) = my_role(pool, workspace_id, session_token).await? else {
        return Err(LatarnikError::NotFound);
    };
    let (next, reason, valid) = match answer {
        MyAnswer::Accept => (RoleStatus::Active, None, state == MyRoleState::Invited),
        MyAnswer::Decline => (
            RoleStatus::Revoked,
            Some("declined_by_person"),
            state == MyRoleState::Invited,
        ),
        MyAnswer::Pause => (RoleStatus::Paused, None, state == MyRoleState::Active),
        MyAnswer::Resume => (RoleStatus::Active, None, state == MyRoleState::Paused),
        MyAnswer::Leave => (
            RoleStatus::Revoked,
            Some("left_by_person"),
            matches!(state, MyRoleState::Active | MyRoleState::Paused),
        ),
    };
    if !valid {
        return Err(LatarnikError::IllegalMove {
            from: match state {
                MyRoleState::None => "none",
                MyRoleState::Invited => "invited",
                MyRoleState::Active => "active",
                MyRoleState::Paused => "paused",
                MyRoleState::Ended => "revoked",
            },
            to: next.as_str(),
        });
    }
    transition_role(pool, workspace_id, role_id, next, reason, now).await?;
    if answer == MyAnswer::Accept {
        // The one thing an active Latarnik may do today is carry a personal
        // referral link; missions arrive with slice 3.
        sqlx::query(
            "UPDATE latarnik_roles SET capabilities = '[\"referral_link\"]'::jsonb
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(role_id)
        .execute(pool)
        .await?;
        // They carry their *own* link, so it must exist. The same code the fan
        // gets everywhere else (`load_or_create_fan_referral_code`): one active
        // code per fan, so this is a no-op for a fan who already has one.
        sqlx::query(
            "INSERT INTO referral_codes (workspace_id, fan_id, code)
             SELECT fan.workspace_id, fan.id, encode(gen_random_bytes(18), 'hex')
             FROM fan_sessions session
             JOIN fans fan
               ON fan.workspace_id = session.workspace_id AND fan.id = session.fan_id
             WHERE session.workspace_id = $1
               AND session.session_token_hash = digest($2, 'sha256')
               AND NOT EXISTS (
                   SELECT 1 FROM referral_codes rc
                   WHERE rc.workspace_id = fan.workspace_id AND rc.fan_id = fan.id AND rc.active)
             ON CONFLICT DO NOTHING",
        )
        .bind(workspace_id)
        .bind(session_token)
        .execute(pool)
        .await?;
    }
    Ok(next)
}
