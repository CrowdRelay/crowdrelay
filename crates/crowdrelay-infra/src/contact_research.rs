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
    .fetch_optional(pool)
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

/// Validates a candidate and records it against the beacon's address.
///
/// The same call serves the research agent's result and a person's note:
/// `researched_by` says which (`agent:<template>` or `operator`). The same
/// source again replaces the earlier row, so re-running research is idempotent.
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
    if language.len() != 2 || !language.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(ResearchError::Refused(
            "the language is a two-letter code such as pl or en".to_owned(),
        ));
    }
    let email = sqlx::query_scalar::<_, Option<String>>(
        "SELECT lower(btrim(contact_email)) FROM beacons WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .filter(|email| !email.is_empty())
    .ok_or(ResearchError::NotFound)?;
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
    .execute(pool)
    .await?;
    Ok(hook)
}
