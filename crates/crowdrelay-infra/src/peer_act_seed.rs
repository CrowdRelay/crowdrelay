//! Writing a researched band sheet into the shared peer-act registry.
//!
//! `domain::peer_act_seed` splits a sheet row into the act everyone sees and
//! the contact one tenant holds; this module is where that split lands on
//! disk. The act upserts into `place_peer_acts` on its `name_key` — a pure
//! identity row, plus the `home_city_id` pointer when the sheet's city
//! resolves against the catalogue — and every claim about it becomes a
//! `place_peer_act_facts` row with `provenance = 'researched'` and the
//! sheet's own source as `source_ref`, the same contract
//! `venue_seed::import_sheet` keeps for rooms.
//!
//! # Which facts are global
//!
//! `workspace_id NULL` is the platform's knowledge of the act: its home
//! city, its public pages. `workspace_id` set is the contributing tenant's
//! lead: the contact address and the researcher's notes. Contact is never
//! global — the address in one tenant's sheet is that tenant's relationship
//! to build, not a shared directory.
//!
//! # What a re-scan does and does not do
//!
//! Facts upsert on `(peer_act_id, attribute, provenance, source_ref)` within
//! a scope — a re-researched row refreshes the claim rather than stacking a
//! twin, and a row that vanishes from the next sheet retracts nothing. The
//! same holds for `home_city_id`: a re-scan that resolves a city writes the
//! pointer, but a re-scan that does not carry a city never erases one a
//! previous sheet knew — `COALESCE(EXCLUDED, existing)` is the difference
//! between "not stated this time" and "moved".

use crowdrelay_domain::peer_act_seed::{PeerActSeedReport, SeededPeer};
use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// What one act's import did. `ImportedWithoutCity` is not a refusal — the
/// act lands with `home_city_id` NULL — but the outcome names the gap so the
/// sheet can be corrected rather than silently under-citied.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PeerSeedOutcome {
    Imported,
    /// The City cell named no catalogue city; the act imported without one.
    ImportedWithoutCity,
}

/// What one sheet's import did, in counts the worker can report.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct PeerActSeedSummary {
    pub imported: u64,
    /// Imported rows whose City cell named no catalogue city.
    pub unresolved_city: u64,
    /// Rows whose own transaction failed — a bad row is counted and logged,
    /// never allowed to take the sheet's other bands down with it.
    pub failed: u64,
}

/// One attributed claim about an act, as `write_fact` takes it — a struct
/// rather than eight positional arguments, the same shape
/// `VenueFactWrite` gives venue facts.
pub struct PeerActFactWrite<'a> {
    pub peer_act_id: Uuid,
    pub attribute: &'a str,
    pub value: &'a str,
    pub provenance: &'a str,
    pub source_ref: &'a str,
    pub observed_at: Option<OffsetDateTime>,
    pub expires_at: Option<OffsetDateTime>,
    pub workspace_id: Option<Uuid>,
}

#[derive(Clone)]
pub struct PostgresPeerActSeedRepository {
    pool: PgPool,
}

impl PostgresPeerActSeedRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One attributed claim about an act — the single upsert every fact
    /// writer goes through, so the sheet importer and any later sweep share
    /// one dedupe rule rather than two copies of it.
    ///
    /// `workspace_id` `None` writes the global row; `Some` writes the
    /// contributor-private one. `provenance` names the fact's trust class
    /// (the CHECK constraint on the column is the arbiter — an unknown class
    /// fails the write rather than being filed silently). `expires_at` is a
    /// deletion deadline, not a staleness hint.
    ///
    /// Value and source_ref are capped at the CHECK bounds (2000) — a
    /// source cell longer than that is truncated rather than failing the
    /// act's whole write over prose.
    ///
    /// # Errors
    ///
    /// Propagates the database error — including the CHECK violations a
    /// caller earns for an unknown provenance or an over-long attribute.
    pub async fn write_fact<'e, E>(
        executor: E,
        fact: PeerActFactWrite<'_>,
    ) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let value: String = fact.value.chars().take(2000).collect();
        let source_ref: String = fact.source_ref.chars().take(2000).collect();
        // Two partial unique indexes back the dedupe (migration 0325): a
        // global fact conflicts on the four-column key, a private one on the
        // key plus its workspace. One statement per scope — the arbiter's
        // WHERE must match the index predicate for inference to find it.
        let sql = if fact.workspace_id.is_some() {
            r#"
            INSERT INTO place_peer_act_facts
                (peer_act_id, attribute, value, provenance, source_ref,
                 observed_at, expires_at, workspace_id)
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()), $7, $8)
            ON CONFLICT (peer_act_id, attribute, provenance, source_ref, workspace_id)
            WHERE workspace_id IS NOT NULL
            DO UPDATE SET value = EXCLUDED.value,
                          observed_at = EXCLUDED.observed_at,
                          expires_at = EXCLUDED.expires_at
            "#
        } else {
            r#"
            INSERT INTO place_peer_act_facts
                (peer_act_id, attribute, value, provenance, source_ref,
                 observed_at, expires_at, workspace_id)
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()), $7, NULL)
            ON CONFLICT (peer_act_id, attribute, provenance, source_ref)
            WHERE workspace_id IS NULL
            DO UPDATE SET value = EXCLUDED.value,
                          observed_at = EXCLUDED.observed_at,
                          expires_at = EXCLUDED.expires_at
            "#
        };
        let mut query = sqlx::query(sql)
            .bind(fact.peer_act_id)
            .bind(fact.attribute)
            .bind(&value)
            .bind(fact.provenance)
            .bind(&source_ref)
            .bind(fact.observed_at)
            .bind(fact.expires_at);
        if let Some(workspace_id) = fact.workspace_id {
            query = query.bind(workspace_id);
        }
        query.execute(executor).await?;
        Ok(())
    }

    /// Imports every parsed act of one sheet. Each act is its own
    /// transaction — one bad row must not take the sheet's other bands with
    /// it, and a half-written act is worse than a refused one.
    pub async fn import_sheet(
        &self,
        workspace_id: Uuid,
        report: &PeerActSeedReport,
    ) -> Result<PeerActSeedSummary, sqlx::Error> {
        let mut summary = PeerActSeedSummary::default();
        for act in &report.acts {
            match self.import_act(workspace_id, act).await {
                Ok(PeerSeedOutcome::Imported) => summary.imported += 1,
                Ok(PeerSeedOutcome::ImportedWithoutCity) => {
                    summary.imported += 1;
                    summary.unresolved_city += 1;
                }
                Err(error) => {
                    summary.failed += 1;
                    tracing::warn!(
                        act = %act.act.name,
                        error = %error,
                        "peer act row failed to import; the rest of the sheet continues"
                    );
                }
            }
        }
        Ok(summary)
    }

    /// One act: resolve the city when the sheet stated one, upsert the
    /// identity row, write its genres and facts.
    ///
    /// The city resolves the way `import_venue` resolves it — a name match
    /// only when exactly one city answers, because a name two cities share
    /// picks nothing rather than picking wrong. `cities.slug` is unique per
    /// country, not globally, so the id resolves in the same query the
    /// country constraint ran in: a "Neustadt" inside a Czechia row must
    /// not resolve to the German catalogue entry, and a slug shared across
    /// two countries must not pick one by accident.
    async fn import_act(
        &self,
        workspace_id: Uuid,
        peer: &SeededPeer,
    ) -> Result<PeerSeedOutcome, sqlx::Error> {
        let act = &peer.act;
        let country_code = act.country.as_deref().and_then(resolve_country_code);
        let city_id = match &act.city {
            Some(city) => {
                let ids = sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM cities \
                     WHERE (slug = lower(btrim($1)) \
                        OR lower(btrim(name)) = lower(btrim($1))) \
                       AND ($2::text IS NULL OR country_code = $2)",
                )
                .bind(city)
                .bind(country_code)
                .fetch_all(&self.pool)
                .await?;
                match ids.as_slice() {
                    [only] => Some(*only),
                    _ => None,
                }
            }
            None => None,
        };
        let city_unresolved = act.city.is_some() && city_id.is_none();

        // The fact's clock is the sheet's own Research_Date at midnight UTC;
        // a missing or unparsable one means "seen now".
        let date_format = time::macros::format_description!("[year]-[month]-[day]");
        let observed_at: Option<OffsetDateTime> = act
            .researched_on
            .as_deref()
            .map(str::trim)
            .and_then(|raw| Date::parse(raw, &date_format).ok())
            .map(|date| date.midnight().assume_utc());

        // Where a claim can be checked: the sheet's own source, falling back
        // to the band's public page — the parser has already refused a row
        // carrying none of the three.
        let source_ref = act
            .source_url
            .as_deref()
            .or(act.social.as_deref())
            .or(act.website.as_deref())
            .unwrap_or("sheet");

        let mut tx = self.pool.begin().await?;
        // name_key's CHECK caps it at 500 chars; a longer key would fail the
        // row outright. Truncating alone could merge two distinct acts onto
        // one identity, so an over-length key keeps a bounded prefix plus a
        // digest of the whole key — still deterministic, still one act.
        let peer_act_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            WITH key AS (SELECT place_venue_key($1) AS k)
            INSERT INTO place_peer_acts (name_key, display_name, home_city_id)
            SELECT CASE WHEN k IS NULL OR char_length(k) <= 500 THEN k
                        ELSE left(k, 475) || '-' || left(md5(k), 24) END,
                   left(btrim($2), 500), $3
            FROM key
            ON CONFLICT (name_key)
            DO UPDATE SET display_name = EXCLUDED.display_name,
                          home_city_id = COALESCE(EXCLUDED.home_city_id,
                                                  place_peer_acts.home_city_id)
            RETURNING id
            "#,
        )
        .bind(&act.name)
        .bind(&act.name)
        .bind(city_id)
        .fetch_one(&mut *tx)
        .await?;

        // Global half — the act as everyone sees it (workspace_id NULL).
        // The fact stores the sheet's own spelling of the town — the claim
        // — while `home_city_id` is the resolution of that claim. Storing
        // the resolved id here would hide a wrong guess; storing the text
        // lets an audit compare what the sheet said against what we filed.
        if let Some(city) = &act.city {
            researched_fact(
                &mut tx,
                peer_act_id,
                "home_city",
                city,
                source_ref,
                observed_at,
                None,
            )
            .await?;
        }
        if let Some(country) = &act.country {
            researched_fact(
                &mut tx,
                peer_act_id,
                "home_country",
                country,
                source_ref,
                observed_at,
                None,
            )
            .await?;
        }
        // The researcher's "still a working band" finding, verbatim — the
        // evidence the 2026-activity rule ran on. Without it a live import
        // is indistinguishable from a stale one.
        if let Some(activity) = &act.activity {
            researched_fact(
                &mut tx,
                peer_act_id,
                "activity",
                activity,
                source_ref,
                observed_at,
                None,
            )
            .await?;
        }
        if let Some(social) = &act.social {
            researched_fact(
                &mut tx,
                peer_act_id,
                "link:social",
                social,
                source_ref,
                observed_at,
                None,
            )
            .await?;
        }
        if let Some(website) = &act.website {
            researched_fact(
                &mut tx,
                peer_act_id,
                "link:website",
                website,
                source_ref,
                observed_at,
                None,
            )
            .await?;
        }

        // Genres land in the typed table, not facts — `propose_peers` and the
        // comparable-acts join read `place_peer_act_genres` through
        // `place_genre_aliases`, and a fact row would be invisible to both.
        for tag in &act.genre_tags {
            sqlx::query(
                r#"
                INSERT INTO place_peer_act_genres
                    (peer_act_id, genre_tag, provenance, source_ref, observed_at)
                VALUES ($1, left(btrim($2), 100), 'researched', left($3, 2000),
                        COALESCE($4, now()))
                ON CONFLICT (peer_act_id, genre_tag, provenance, source_ref)
                DO UPDATE SET observed_at = EXCLUDED.observed_at
                "#,
            )
            .bind(peer_act_id)
            .bind(tag)
            .bind(source_ref)
            .bind(observed_at)
            .execute(&mut *tx)
            .await?;
        }

        // Private half — one tenant's lead on the act. The contact address
        // and everything about how it was found and judged (type, source,
        // readiness, confidence) are the importing tenant's research, so
        // all of them land scoped: another tenant proposing this act sees
        // the public page and the city, never this lead's trail.
        for (attribute, value) in [
            ("contact_email", peer.view.email.as_deref()),
            ("contact_type", peer.view.contact_type.as_deref()),
            ("contact_source", peer.view.contact_source.as_deref()),
            (
                "outreach_readiness",
                peer.view.outreach_readiness.as_deref(),
            ),
            ("confidence", peer.view.confidence.as_deref()),
            ("notes", peer.view.notes.as_deref()),
        ] {
            if let Some(value) = value {
                researched_fact(
                    &mut tx,
                    peer_act_id,
                    attribute,
                    value,
                    source_ref,
                    observed_at,
                    Some(workspace_id),
                )
                .await?;
            }
        }

        tx.commit().await?;
        Ok(if city_unresolved {
            PeerSeedOutcome::ImportedWithoutCity
        } else {
            PeerSeedOutcome::Imported
        })
    }
}

/// The sheet's own claims: `researched` provenance, no deletion clock. The
/// shorthand exists so the import above reads as the list of attributes it
/// writes rather than a wall of repeated constants — the upsert itself is
/// `PostgresPeerActSeedRepository::write_fact`, which every fact writer
/// shares.
async fn researched_fact(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    peer_act_id: Uuid,
    attribute: &str,
    value: &str,
    source_ref: &str,
    observed_at: Option<OffsetDateTime>,
    workspace_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    PostgresPeerActSeedRepository::write_fact(
        &mut **tx,
        PeerActFactWrite {
            peer_act_id,
            attribute,
            value,
            provenance: "researched",
            source_ref,
            observed_at,
            expires_at: None,
            workspace_id,
        },
    )
    .await
}

/// A sheet's country spelling → the ISO code `cities.country_code` carries.
///
/// Covers the spellings the feeds actually use — English names and the
/// local ones a Polish researcher writes — for the countries the catalogue
/// can plausibly hold. `None` means "don't constrain": the city match falls
/// back to the global unique-name rule rather than guessing a country.
fn resolve_country_code(country: &str) -> Option<&'static str> {
    let code = match country.trim().to_lowercase().as_str() {
        "germany" | "deutschland" | "niemcy" | "de" => "DE",
        "poland" | "polska" | "pl" => "PL",
        "czechia" | "czech republic" | "czechy" | "czeska" | "cz" => "CZ",
        "slovakia" | "slovak republic" | "słowacja" | "slowacja" | "sk" => "SK",
        "austria" | "österreich" | "at" => "AT",
        "ukraine" | "ukraina" | "ua" => "UA",
        "united kingdom" | "uk" | "great britain" | "gb" => "GB",
        "france" | "francja" | "fr" => "FR",
        "netherlands" | "holland" | "holandia" | "nl" => "NL",
        _ => return None,
    };
    Some(code)
}
