//! What the band knows about a person before it writes to them — the store.
//!
//! The rule and the checks live in `crowdrelay_domain::contact_research`; this
//! is the table behind them (`contact_research`, migration 0396). One reader and
//! one writer, both keyed by the address, so the two roles a person can wear
//! (`latarnik`) share one piece of research instead of paying for it twice.

use crowdrelay_domain::contact_research::{HOOK_MAX_AGE_DAYS, HookRefusal, PersonalHook};
use sqlx::PgPool;
use time::Date;
use uuid::Uuid;

/// Why a hook was not recorded.
#[derive(Debug, thiserror::Error)]
pub enum ResearchError {
    #[error("contact research database operation failed")]
    Database(#[from] sqlx::Error),
    /// No such beacon, or it has no address to key the research on.
    #[error("no such contact")]
    NotFound,
    /// The candidate broke a rule; carries the operator-facing sentence.
    #[error("{0}")]
    Refused(String),
}

impl From<HookRefusal> for ResearchError {
    fn from(refusal: HookRefusal) -> Self {
        Self::Refused(refusal.message())
    }
}

/// The newest recent fact on file about `email`, or `None`.
///
/// "Recent" is decided here in SQL against `today`, with the window the domain
/// owns, so a stale fact is invisible to every caller rather than something
/// each one has to remember to check.
///
/// # Errors
///
/// Propagates the database error.
pub async fn latest_hook(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    today: Date,
) -> Result<Option<PersonalHook>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    latest_hook_on(&mut conn, workspace_id, email, today).await
}

/// [`latest_hook`] on a connection, so a letter can be composed in the same
/// transaction that records it.
///
/// # Errors
///
/// Propagates the database error.
pub async fn latest_hook_on(
    conn: &mut sqlx::PgConnection,
    workspace_id: Uuid,
    email: &str,
    today: Date,
) -> Result<Option<PersonalHook>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, Option<String>, String, Date)>(
        r#"
        SELECT fact, praise, source_url, observed_on
        FROM contact_research
        WHERE workspace_id = $1
          AND normalized_email = $2
          AND observed_on <= $3
          AND observed_on >= $3 - $4::int
        ORDER BY observed_on DESC, researched_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(email.trim().to_ascii_lowercase())
    .bind(today)
    .bind(i32::try_from(HOOK_MAX_AGE_DAYS).unwrap_or(120))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(
        row.map(|(fact, praise, source_url, observed_on)| PersonalHook {
            fact,
            praise,
            source_url,
            observed_on,
        }),
    )
}

/// Records an already-validated hook against an address on `conn`.
///
/// Takes a connection rather than a pool so the worker can write it inside the
/// transaction that records the agent outcome: the fact and the outcome that
/// produced it commit together or not at all. `hook` can only have come from
/// [`PersonalHook::new`], so the register and recency rules have already held.
///
/// Keyed by the address, not by a beacon or an outreach target: one person can
/// be both, and the research must not be done, paid for or lost twice.
///
/// # Errors
///
/// [`ResearchError::Refused`] for an unusable language or address, or the
/// database error.
pub async fn record_hook_on(
    conn: &mut sqlx::PgConnection,
    workspace_id: Uuid,
    email: &str,
    hook: &PersonalHook,
    language: &str,
    researched_by: &str,
) -> Result<(), ResearchError> {
    if language.len() != 2 || !language.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(ResearchError::Refused(
            "the language is a two-letter code such as pl or en".to_owned(),
        ));
    }
    let email = email.trim().to_ascii_lowercase();
    if !email.contains('@') {
        return Err(ResearchError::NotFound);
    }
    sqlx::query(
        r#"
        INSERT INTO contact_research
            (workspace_id, normalized_email, fact, praise, source_url, observed_on,
             language, researched_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (workspace_id, normalized_email, source_url) DO UPDATE
            SET fact = EXCLUDED.fact,
                praise = EXCLUDED.praise,
                observed_on = EXCLUDED.observed_on,
                language = EXCLUDED.language,
                researched_by = EXCLUDED.researched_by,
                researched_at = now()
        "#,
    )
    .bind(workspace_id)
    .bind(&email)
    .bind(&hook.fact)
    .bind(&hook.praise)
    .bind(&hook.source_url)
    .bind(hook.observed_on)
    .bind(language)
    .bind(researched_by.trim())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The address a beacon is reached at, lowercased, or `None`.
async fn beacon_email(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT lower(btrim(contact_email)) FROM beacons WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .filter(|email| !email.is_empty()))
}

/// Validates a candidate and records it against the beacon's address.
///
/// The same checks serve a person's note and, through the worker, the research
/// agent's result; `researched_by` says which (`operator` or
/// `agent:<template>`). The same source again replaces the earlier row, so
/// re-running research is idempotent.
///
/// # Errors
///
/// [`ResearchError::Refused`] with the rule that was broken, or the beacon is
/// unknown, or the database error.
#[allow(clippy::too_many_arguments)]
pub async fn record_hook_for_beacon(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    fact: &str,
    praise: Option<&str>,
    source_url: &str,
    observed_on: Date,
    language: &str,
    researched_by: &str,
    today: Date,
) -> Result<PersonalHook, ResearchError> {
    let hook = PersonalHook::new(fact, praise, source_url, observed_on, today)?;
    let email = beacon_email(pool, workspace_id, beacon_id)
        .await?
        .ok_or(ResearchError::NotFound)?;
    let mut conn = pool.acquire().await?;
    record_hook_on(
        &mut conn,
        workspace_id,
        &email,
        &hook,
        language,
        researched_by,
    )
    .await?;
    Ok(hook)
}

/// The template that reads a person's recent work. Must match the agent
/// service's catalog id.
pub const RESEARCH_TEMPLATE: &str = "contact-researcher";

/// A person is researched again no sooner than this after the last attempt that
/// did not fail. Research costs a premium agent run; a person the agent could
/// find nothing recent about does not become findable by asking again tomorrow.
pub const RESEARCH_RETRY_DAYS: i32 = 7;

/// Asks the research agent to read one beacon's recent work.
///
/// Researching contacts nobody, so this needs no approval; it is still bounded:
/// only people who would otherwise be askable are researched (the relationship
/// tests have all passed and the only thing missing is the research), and one
/// person is researched at most once per [`RESEARCH_RETRY_DAYS`] unless the
/// earlier attempt failed.
///
/// The person is pinned in the task's metadata by address
/// (`subject_contact_email`). The worker accepts a result only for that
/// address, and the model is never given a way to name anybody else: the
/// address is not in the prompt and not in the tool's output.
///
/// # Errors
///
/// [`ResearchError::Refused`] with the sentence the operator reads,
/// [`ResearchError::NotFound`], or the database error.
pub async fn queue_contact_research(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    now: time::OffsetDateTime,
) -> Result<Uuid, ResearchError> {
    let contact = crate::latarnik::dual_role_contact(pool, workspace_id, beacon_id, now, true)
        .await?
        .ok_or(ResearchError::NotFound)?;
    let needs_research = crowdrelay_domain::latarnik_invite::InviteHold::NeedsResearch.message();
    if contact.has_research {
        return Err(ResearchError::Refused(
            "this person was read recently — their fact is on file".to_owned(),
        ));
    }
    if contact.hold_reason.as_deref() != Some(needs_research.as_str()) {
        // Held for some other reason first: cold, written to last week, asked
        // before, opted out. Researching them would be spending on someone who
        // cannot be asked.
        return Err(ResearchError::Refused(format!(
            "not worth researching yet: {}",
            contact
                .hold_reason
                .unwrap_or_else(|| "they are already askable".to_owned())
        )));
    }
    let email = beacon_email(pool, workspace_id, beacon_id)
        .await?
        .ok_or(ResearchError::NotFound)?;
    enqueue_research(
        pool,
        workspace_id,
        &email,
        &contact.display_name,
        &contact.role,
        contact.city.as_deref(),
    )
    .await
}

/// Asks the research agent to read an outreach target's recent work.
///
/// The outreach engine pitches targets that are not beacons (press, radio,
/// playlists), and the rule is the same for them: nobody is pitched unread.
/// A target is researched only if it is still open to contact — active,
/// accepting outreach, not marked do-not-contact, and not a person who said no
/// — and has no recent fact on file.
///
/// # Errors
///
/// [`ResearchError::Refused`] with the sentence the operator reads,
/// [`ResearchError::NotFound`], or the database error.
pub async fn queue_target_research(
    pool: &PgPool,
    workspace_id: Uuid,
    target_id: Uuid,
    now: time::OffsetDateTime,
) -> Result<Uuid, ResearchError> {
    let target = sqlx::query_as::<_, (String, String, String, bool, bool, bool, String)>(
        r#"
        SELECT lower(btrim(contact_email)), display_name, target_kind, active,
               accepts_outreach, do_not_contact, last_reply_disposition
        FROM outreach_targets
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(target_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ResearchError::NotFound)?;
    let (email, name, kind, active, accepts, do_not_contact, reply) = target;
    if !active
        || !accepts
        || do_not_contact
        || matches!(reply.as_str(), "declined" | "do_not_contact")
    {
        return Err(ResearchError::Refused(
            "not worth researching: they are not open to contact".to_owned(),
        ));
    }
    if latest_hook(pool, workspace_id, &email, now.date())
        .await?
        .is_some()
    {
        return Err(ResearchError::Refused(
            "this person was read recently — their fact is on file".to_owned(),
        ));
    }
    enqueue_research(pool, workspace_id, &email, &name, &kind, None).await
}

/// Puts one `contact-researcher` task on the agents queue, once a week at most.
async fn enqueue_research(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    display_name: &str,
    role: &str,
    city: Option<&str>,
) -> Result<Uuid, ResearchError> {
    let table_exists =
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('agent_service_tasks') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !table_exists {
        return Err(ResearchError::Refused(
            "the agent service is not deployed here, so nothing can read them".to_owned(),
        ));
    }
    let recent = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM agent_service_tasks
            WHERE workspace_id = $1
              AND template_id = $2
              AND metadata->>'subject_contact_email' = $3
              AND status <> 'failed'
              AND created_at > now() - make_interval(days => $4::int)
        )
        "#,
    )
    .bind(workspace_id)
    .bind(RESEARCH_TEMPLATE)
    .bind(email)
    .bind(RESEARCH_RETRY_DAYS)
    .fetch_one(pool)
    .await?;
    if recent {
        return Err(ResearchError::Refused(format!(
            "they were sent for research in the last {RESEARCH_RETRY_DAYS} days — the result is \
             on its way or the agent found nothing recent"
        )));
    }
    let prompt = format!(
        "Research this person for the band, so a letter to them can open with something true \
         about their recent work.\n\nname: {display_name}\nrole: {role}\ncity: {}\n\n\
         Return the one most recent, specific thing they did, cited exactly as instructed, or \
         nothing if the research data does not support one.",
        city.unwrap_or("unknown"),
    );
    let task_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks
            (id, workspace_id, template_id, model_id, prompt, status, tier, metadata)
        VALUES ($1, $2, $3, 'auto', $4, 'queued', 'premium', $5)
        "#,
    )
    .bind(task_id)
    .bind(workspace_id)
    .bind(RESEARCH_TEMPLATE)
    .bind(prompt)
    .bind(serde_json::json!({
        "source": "operator",
        "subject_contact_email": email,
    }))
    .execute(pool)
    .await?;
    Ok(task_id)
}
