//! Issuing, storing, serving and verifying audience attestations.
//!
//! The policy is in `crowdrelay_domain::attestation`: what may be attested,
//! the privacy floor, the freshness bound, and the canonical digest. This file
//! is the three things a domain cannot do — measure, sign, and persist.
//!
//! # The digest and the signature answer different questions
//!
//! The digest is canonical and public: anybody holding the document can
//! recompute it, and a mismatch means a field was edited after issue. It proves
//! nothing about origin, because anybody can compute a digest over anything.
//!
//! The signature is an HMAC over the digest under a server-held key. It is the
//! only part of the document a stranger has a reason to believe, because it is
//! the only part they cannot produce themselves. A verify path that checked the
//! digest alone would accept a document somebody wrote from scratch, which is
//! the failure this whole feature exists to prevent — so [`VerifiedAttestation`]
//! reports both answers separately and never collapses them into one boolean.
//!
//! # A stored attestation is never edited
//!
//! An attestation is a snapshot somebody was shown and may have acted on.
//! Re-measuring produces a new document with a new digest; the old one stays
//! exactly as issued until it expires or is revoked. Migration 0297 enforces
//! this with a trigger rather than trusting this file to remember.

use crowdrelay_domain::attestation::{
    Attestation, AttestationRefusal, AttestedFigure, AttestedMetric, FigureScope, MeasuredFigure,
    issue,
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::{PgPool, Row};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

/// Domain separation for the attestation signing key.
///
/// Shares the pattern with `sensitive_response`: the configured secret is
/// hashed with a purpose string so the same secret used for two purposes yields
/// two unrelated keys. Without it, a signature forged in one subsystem would be
/// valid in another.
const KEY_DERIVATION_DOMAIN: &[u8] = b"crowdrelay.attestation.signing.v1\n";

/// The server-held key an attestation is signed under.
///
/// Debug is redacted because a key that reaches a log is a key that has to be
/// rotated, and the rotation invalidates every signature already in the wild.
#[derive(Clone)]
pub struct AttestationSigningKey([u8; 32]);

impl std::fmt::Debug for AttestationSigningKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AttestationSigningKey([REDACTED])")
    }
}

/// Written by hand rather than derived, and the difference is not cosmetic.
///
/// `Config` derives `PartialEq`, which is what requires this at all. A derived
/// comparison on key material short-circuits at the first differing byte, and
/// anything that can time that comparison learns how much of a guess was
/// right. Config equality is not a path an attacker reaches today — but the
/// type is reachable, and a type that is safe in one place and unsafe in
/// another is a type somebody eventually moves.
impl PartialEq for AttestationSigningKey {
    fn eq(&self, other: &Self) -> bool {
        let mut difference = 0u8;
        for (left, right) in self.0.iter().zip(other.0.iter()) {
            difference |= left ^ right;
        }
        difference == 0
    }
}

impl Eq for AttestationSigningKey {}

impl AttestationSigningKey {
    #[must_use]
    pub fn derive_from_secret(secret: &[u8]) -> Self {
        let mut digest = <Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut digest, KEY_DERIVATION_DOMAIN);
        sha2::Digest::update(&mut digest, secret);
        Self(sha2::Digest::finalize(digest).into())
    }

    /// Signs a digest. Returns lowercase hex, matching the column's CHECK.
    fn sign(&self, digest: &str) -> String {
        // HMAC accepts any key length, and this one is fixed at 32 bytes.
        let mut mac = match <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0) {
            Ok(mac) => mac,
            Err(_) => return String::new(),
        };
        mac.update(digest.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Whether `signature` is ours, compared in constant time.
    ///
    /// A byte-by-byte early return leaks how many leading characters were
    /// correct, and a caller who can ask repeatedly recovers the signature one
    /// character at a time. The verify endpoint is public and unauthenticated,
    /// so that caller definitely exists.
    fn verify(&self, digest: &str, signature: &str) -> bool {
        let expected = self.sign(digest);
        // An empty expectation never verifies.
        //
        // `sign` returns an empty string when HMAC initialisation fails, which
        // it cannot with a fixed 32-byte key — but the crate denies
        // `expect_used`, so the impossible branch has to return *something*,
        // and "" compared against "" is `true`. That would make every
        // signature valid at exactly the moment signing stopped working. The
        // column's CHECK keeps an empty signature out of the table, so this
        // guards the presented side rather than the stored one.
        if expected.is_empty() || signature.is_empty() {
            return false;
        }
        if expected.len() != signature.len() {
            return false;
        }
        let mut difference = 0u8;
        for (left, right) in expected.bytes().zip(signature.bytes()) {
            difference |= left ^ right;
        }
        difference == 0
    }
}

#[derive(Debug, Error)]
pub enum AttestationError {
    #[error("attestation database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("no attestation matches")]
    NotFound,
    /// The domain refused to issue. Carries the operator-facing sentence.
    #[error("{0}")]
    Refused(String),
    /// A stored row could not be read back into the domain's shape. Treated as
    /// corruption rather than as an empty result: silently returning "no
    /// figures" would publish a document that says the act has nothing.
    #[error("a stored attestation could not be read back")]
    Corrupt,
}

/// What a reader learns when they present a document.
///
/// Four independent answers, never collapsed. A document can be authentic and
/// expired, or unedited and forged — and a single `valid: bool` would make both
/// of those read as "no" with no way to tell which.
#[derive(Clone, Debug, serde::Serialize)]
pub struct VerifiedAttestation {
    pub attestation: Attestation,
    /// The digest recomputed from the figures matches the stored digest.
    pub unedited: bool,
    /// The signature over that digest is ours.
    pub issued_by_us: bool,
    /// Inside its validity window at the moment of asking.
    pub current: bool,
    pub revoked: bool,
}

impl VerifiedAttestation {
    /// The one-line answer, for a reader who wants a verdict rather than four
    /// flags. Ordered by how badly each fails: a forgery is worse news than an
    /// expiry, so it is reported first even when both are true.
    #[must_use]
    pub fn verdict(&self) -> &'static str {
        if !self.issued_by_us {
            "not issued by CrowdRelay"
        } else if !self.unedited {
            "issued by CrowdRelay, but edited since"
        } else if self.revoked {
            "issued by CrowdRelay, then withdrawn by the act"
        } else if !self.current {
            "issued by CrowdRelay, unedited, but out of date"
        } else {
            "issued by CrowdRelay, unedited, current"
        }
    }
}

#[derive(Clone)]
pub struct PostgresAttestationRepository {
    pool: PgPool,
    key: AttestationSigningKey,
}

impl PostgresAttestationRepository {
    #[must_use]
    pub fn new(pool: PgPool, key: AttestationSigningKey) -> Self {
        Self { pool, key }
    }

    /// The signature this repository would put on a digest.
    ///
    /// Public because the document has to show it: a reader who wants to check
    /// the attestation against our published key needs the signature in front
    /// of them, and an API layer rendering the document has no other way to get
    /// it. Signing an arbitrary digest reveals nothing — that is what an HMAC
    /// is for.
    #[must_use]
    pub fn sign_digest(&self, digest: &str) -> String {
        self.key.sign(digest)
    }

    /// Measures, issues, signs and stores in one step.
    ///
    /// Measurement and issue are not separable by a caller on purpose: a
    /// caller that could supply its own figures could supply any number, and
    /// the product claim is that no number here was typed by a person.
    pub async fn issue_for_workspace(
        &self,
        workspace_id: Uuid,
        act_name: &str,
        cities: &[String],
        now: OffsetDateTime,
    ) -> Result<Attestation, AttestationError> {
        let measured = self.measure(workspace_id, cities, now).await?;
        let attestation = issue(act_name, &measured, now)
            .map_err(|refusal: AttestationRefusal| AttestationError::Refused(refusal.message()))?;
        let signature = self.key.sign(&attestation.digest);

        sqlx::query(
            r#"
            INSERT INTO viryaos_attestations
                (workspace_id, act_name, figures, issued_at, valid_until, digest, signature)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            -- A digest collides only when the same workspace issues the same
            -- figures inside the same second, because the act name and both
            -- timestamps are inside it. That is the same document, so the
            -- content columns are left alone — the immutability trigger would
            -- reject changing them anyway.
            --
            -- But it is NOT a no-op. A band that revokes a document sent to the
            -- wrong person and immediately re-issues used to get `DO NOTHING`
            -- and their old token back, still pointing at the revoked row: they
            -- believed they had a fresh link, the recipient saw "withdrawn by
            -- the act", and nothing said otherwise. Re-issuing is an explicit
            -- act, so it clears the revocation and mints a new token — the old
            -- link stays dead, which is the safe direction.
            ON CONFLICT (digest) DO UPDATE
            SET revoked_at = NULL,
                share_token = gen_random_uuid()
            WHERE viryaos_attestations.workspace_id = EXCLUDED.workspace_id
            "#,
        )
        .bind(workspace_id)
        .bind(&attestation.act_name)
        .bind(serde_json::to_value(&attestation.figures).map_err(|_| AttestationError::Corrupt)?)
        .bind(attestation.issued_at)
        .bind(attestation.valid_until)
        .bind(&attestation.digest)
        .bind(&signature)
        .execute(&self.pool)
        .await?;
        // `DO NOTHING` rather than an error: two issues over identical facts at
        // the same instant produce the same digest, and the second one is not a
        // conflict, it is the same document. The caller gets it either way.
        Ok(attestation)
    }

    /// What a link-holder receives.
    ///
    /// The token is the whole admission — a reader has the link and nothing
    /// else — so the lookup is by token alone and returns the verification
    /// alongside the document. A revoked attestation still resolves and reports
    /// `revoked`, because "we cannot find this" reads to a sceptical reader
    /// exactly like "this was forged".
    pub async fn read_by_token(
        &self,
        share_token: Uuid,
        now: OffsetDateTime,
    ) -> Result<VerifiedAttestation, AttestationError> {
        let row = sqlx::query(
            r#"
            SELECT workspace_id, act_name, figures, issued_at, valid_until, digest, signature, revoked_at
            FROM viryaos_attestations WHERE share_token = $1
            "#,
        )
        .bind(share_token)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AttestationError::NotFound)?;
        self.verify_row(&row, now)
    }

    /// What a stranger gets when they type a digest off a document they were
    /// handed. No token, no account: the digest is the lookup key, and the
    /// answer is the same one the link would have given.
    pub async fn verify_digest(
        &self,
        digest: &str,
        now: OffsetDateTime,
    ) -> Result<VerifiedAttestation, AttestationError> {
        let row = sqlx::query(
            r#"
            SELECT workspace_id, act_name, figures, issued_at, valid_until, digest, signature, revoked_at
            FROM viryaos_attestations WHERE digest = $1
            "#,
        )
        .bind(digest)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AttestationError::NotFound)?;
        self.verify_row(&row, now)
    }

    fn verify_row(
        &self,
        row: &sqlx::postgres::PgRow,
        now: OffsetDateTime,
    ) -> Result<VerifiedAttestation, AttestationError> {
        let figures: Vec<AttestedFigure> =
            serde_json::from_value(row.get("figures")).map_err(|_| AttestationError::Corrupt)?;
        let stored_digest: String = row.get("digest");
        let signature: String = row.get("signature");
        let revoked_at: Option<OffsetDateTime> = row.get("revoked_at");
        let attestation = Attestation {
            act_name: row.get("act_name"),
            figures,
            issued_at: row.get("issued_at"),
            valid_until: row.get("valid_until"),
            digest: stored_digest.clone(),
        };
        Ok(VerifiedAttestation {
            // Recomputed from the figures rather than trusted from the column:
            // a row whose digest disagrees with its figures is corrupt, and the
            // reader is told so instead of being handed the column's claim.
            unedited: attestation.digest_matches(),
            issued_by_us: self.key.verify(&stored_digest, &signature),
            current: attestation.is_current(now),
            revoked: revoked_at.is_some(),
            attestation,
        })
    }

    /// The share token for a document this workspace issued.
    ///
    /// Separate from `issue_for_workspace` because the token is generated by
    /// the column default, so the issuing path has no way to return it without
    /// a second read. Scoped to the workspace: a digest is public, and without
    /// the scope any holder of one could ask for the link to somebody else's
    /// document.
    pub async fn share_token_for(
        &self,
        workspace_id: Uuid,
        digest: &str,
    ) -> Result<Uuid, AttestationError> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT share_token FROM viryaos_attestations
             WHERE workspace_id = $1 AND digest = $2",
        )
        .bind(workspace_id)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AttestationError::NotFound)
    }

    /// Withdraws a document. The row stays; the link stops working.
    pub async fn revoke(
        &self,
        workspace_id: Uuid,
        digest: &str,
        now: OffsetDateTime,
    ) -> Result<(), AttestationError> {
        let changed = sqlx::query(
            "UPDATE viryaos_attestations SET revoked_at = $3
             WHERE workspace_id = $1 AND digest = $2 AND revoked_at IS NULL",
        )
        .bind(workspace_id)
        .bind(digest)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(AttestationError::NotFound);
        }
        Ok(())
    }

    /// Mints a fresh token, killing every link already sent.
    pub async fn rotate_share_token(
        &self,
        workspace_id: Uuid,
        digest: &str,
    ) -> Result<Uuid, AttestationError> {
        sqlx::query_scalar::<_, Uuid>(
            "UPDATE viryaos_attestations SET share_token = gen_random_uuid()
             WHERE workspace_id = $1 AND digest = $2 RETURNING share_token",
        )
        .bind(workspace_id)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AttestationError::NotFound)
    }

    // ── Measurement (4A.2) ──────────────────────────────────────────────────
    //
    // One query per metric. A metric that cannot be measured is ABSENT from the
    // result, never zero: an attested `0` is a claim that the act has none of
    // something, and a failed query is not that claim. This is the read-model
    // null rule, and it matters more here than anywhere else in the product,
    // because the document travels to somebody making a decision.

    async fn measure(
        &self,
        workspace_id: Uuid,
        cities: &[String],
        now: OffsetDateTime,
    ) -> Result<Vec<MeasuredFigure>, AttestationError> {
        let mut figures = Vec::new();

        // Tickets sold, lifetime. `window_days: 0` says lifetime rather than
        // pretending to a window the query does not apply.
        if let Some(value) = self.tickets_sold(workspace_id).await? {
            figures.push(MeasuredFigure {
                metric: AttestedMetric::TicketsSold,
                scope: FigureScope::Everywhere,
                value,
                window_days: 0,
                observed_at: now,
            });
        }

        if let Some(value) = self.observed_attendance(workspace_id).await? {
            figures.push(MeasuredFigure {
                metric: AttestedMetric::ObservedAttendance,
                scope: FigureScope::Everywhere,
                value,
                window_days: 0,
                observed_at: now,
            });
        }

        if let Some(value) = self.repeat_attenders(workspace_id).await? {
            figures.push(MeasuredFigure {
                metric: AttestedMetric::RepeatAttenders,
                scope: FigureScope::Everywhere,
                value,
                window_days: 0,
                observed_at: now,
            });
        }

        for city in cities {
            if let Some(value) = self.reachable_fans_in_city(workspace_id, city).await? {
                figures.push(MeasuredFigure {
                    metric: AttestedMetric::ReachableFans,
                    scope: FigureScope::City(city.clone()),
                    value,
                    window_days: 0,
                    observed_at: now,
                });
            }
        }

        Ok(figures)
    }

    async fn tickets_sold(&self, workspace_id: Uuid) -> Result<Option<u32>, AttestationError> {
        // Paid and partially-refunded, matching `city_venues`' own definition of
        // a sold ticket. A fully refunded order is not a sale.
        let value = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(*)::bigint
            FROM ticket_orders
            WHERE workspace_id = $1 AND status IN ('paid', 'partially_refunded')
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(u32::try_from(value).ok())
    }

    async fn observed_attendance(
        &self,
        workspace_id: Uuid,
    ) -> Result<Option<u32>, AttestationError> {
        // Distinct people, not distinct scans. One person scanned twice at the
        // door is one attender, and counting rows would inflate the figure
        // exactly where a buyer is least able to check it. A check-in with no
        // `fan_id` is an anonymous scan — counted once as itself, because it is
        // a real body in the room that the identity spine could not resolve.
        let value = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT (
                (SELECT count(DISTINCT fan_id)::bigint
                 FROM concert_checkins
                 WHERE workspace_id = $1 AND fan_id IS NOT NULL)
                +
                (SELECT count(*)::bigint
                 FROM concert_checkins
                 WHERE workspace_id = $1 AND fan_id IS NULL)
            )
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(u32::try_from(value).ok())
    }

    async fn repeat_attenders(&self, workspace_id: Uuid) -> Result<Option<u32>, AttestationError> {
        // The same definition `city_venues` uses for a room's repeat attenders,
        // applied to the act instead: a buyer email that paid for two or more
        // distinct events. Two definitions of "came back" is the shape of every
        // number that disagrees with itself across two screens.
        let value = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(*)::bigint FROM (
                SELECT lower(btrim(ticket_order.buyer_email)) AS buyer
                FROM ticket_orders AS ticket_order
                JOIN ticket_sales AS sale
                  ON sale.workspace_id = ticket_order.workspace_id
                 AND sale.id = ticket_order.ticket_sale_id
                WHERE ticket_order.workspace_id = $1
                  AND ticket_order.status IN ('paid', 'partially_refunded')
                  AND btrim(COALESCE(ticket_order.buyer_email, '')) <> ''
                GROUP BY lower(btrim(ticket_order.buyer_email))
                HAVING count(DISTINCT sale.event_id) >= 2
            ) AS returning_buyers
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(u32::try_from(value).ok())
    }

    /// Reachable fans in one city.
    ///
    /// The gate is `city_funnel`'s, restated: an active fan, whose latest
    /// marketing consent on the append-only `fan_consents` is granted, who
    /// enabled nearby gigs, and whose own city is inside the radius they chose.
    /// **It must stay identical to the console's**, because a band showing a
    /// label 300 and its own dashboard showing 260 is worse than showing
    /// neither — the document's whole value is that its numbers hold up.
    async fn reachable_fans_in_city(
        &self,
        workspace_id: Uuid,
        city_slug: &str,
    ) -> Result<Option<u32>, AttestationError> {
        let value = sqlx::query_scalar::<_, Option<i64>>(
            r#"
            SELECT count(DISTINCT fan.id)::bigint
            FROM cities AS target
            JOIN fan_location_preferences AS preferences
              ON preferences.workspace_id = $1
             AND preferences.nearby_gigs_enabled
            JOIN cities AS fan_city
              ON fan_city.id = preferences.city_id
             AND fan_city.latitude IS NOT NULL
             AND fan_city.longitude IS NOT NULL
            JOIN fans AS fan
              ON fan.workspace_id = preferences.workspace_id
             AND fan.id = preferences.fan_id
             AND fan.status = 'active'
            WHERE target.slug = $2
              AND target.latitude IS NOT NULL
              AND target.longitude IS NOT NULL
              AND EXISTS (
                  SELECT 1
                  FROM fan_consents AS consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.granted
                    AND consent.id = (
                        SELECT newest.id
                        FROM fan_consents AS newest
                        WHERE newest.workspace_id = consent.workspace_id
                          AND newest.fan_id = consent.fan_id
                          AND newest.purpose = consent.purpose
                        ORDER BY newest.recorded_at DESC, newest.id DESC
                        LIMIT 1
                    )
              )
              AND abs(fan_city.latitude - target.latitude)
                  <= (preferences.radius_km + 1)::double precision / 111.0
              AND ROUND(6371 * 2 * ASIN(LEAST(1.0, SQRT(
                    POWER(SIN(RADIANS(fan_city.latitude - target.latitude) / 2), 2)
                    + COS(RADIANS(target.latitude)) * COS(RADIANS(fan_city.latitude))
                    * POWER(SIN(RADIANS(fan_city.longitude - target.longitude) / 2), 2)
                  ))))::integer <= preferences.radius_km
            "#,
        )
        .bind(workspace_id)
        .bind(city_slug)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        // A city we do not hold produces no row, and no row is not zero
        // reachable fans — it is "we cannot measure this city". Absent.
        Ok(value.and_then(|count| u32::try_from(count).ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AttestationSigningKey {
        AttestationSigningKey::derive_from_secret(b"an-attestation-signing-secret")
    }

    /// The failure mode a lint fix introduced: `sign` cannot return an error,
    /// so its unreachable branch returns `String::new()`, and an empty
    /// expectation compared against an empty presentation is equal. Every
    /// signature would verify at exactly the moment signing broke.
    #[test]
    fn an_empty_signature_never_verifies() {
        assert!(!key().verify(&"a".repeat(64), ""));
    }

    #[test]
    fn a_signature_verifies_only_under_its_own_key() {
        let mine = key();
        let theirs = AttestationSigningKey::derive_from_secret(b"somebody-elses-secret");
        let digest = "a".repeat(64);

        let signature = mine.sign(&digest);
        assert!(mine.verify(&digest, &signature));
        assert!(
            !theirs.verify(&digest, &signature),
            "a signature verified under a key that did not produce it"
        );
    }

    #[test]
    fn a_signature_is_bound_to_its_digest() {
        let signing = key();
        let signature = signing.sign(&"a".repeat(64));
        assert!(
            !signing.verify(&"b".repeat(64), &signature),
            "a signature moved to a different document and still verified"
        );
    }

    /// The column's CHECK is `^[0-9a-f]{64}$`, so a signature that is not
    /// lowercase hex of exactly 64 characters would fail the insert at runtime
    /// rather than here.
    #[test]
    fn a_signature_is_lowercase_hex_of_the_length_the_column_accepts() {
        let signature = key().sign(&"a".repeat(64));
        assert_eq!(signature.len(), 64);
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!signature.chars().any(|c| c.is_ascii_uppercase()));
    }

    /// Domain separation: the same secret used for another purpose must not
    /// produce a key that validates attestations.
    #[test]
    fn the_signing_key_is_domain_separated_from_the_raw_secret() {
        let secret = b"shared-secret-material";
        let derived = AttestationSigningKey::derive_from_secret(secret);
        let mut undomained = <Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut undomained, secret);
        let raw: [u8; 32] = sha2::Digest::finalize(undomained).into();
        assert_ne!(derived.0, raw);
    }

    /// A key is the one thing that must never reach a log.
    #[test]
    fn the_key_never_prints_itself() {
        let rendered = format!("{:?}", key());
        assert_eq!(rendered, "AttestationSigningKey([REDACTED])");
    }

    /// Four independent answers, never collapsed. A forged document and an
    /// expired one are both "no", and a reader needs to know which.
    #[test]
    fn the_verdict_names_the_worst_thing_that_is_true() {
        let base = |unedited, issued_by_us, current, revoked| VerifiedAttestation {
            attestation: Attestation {
                act_name: "Virya".to_owned(),
                figures: Vec::new(),
                issued_at: OffsetDateTime::UNIX_EPOCH,
                valid_until: OffsetDateTime::UNIX_EPOCH,
                digest: String::new(),
            },
            unedited,
            issued_by_us,
            current,
            revoked,
        };
        assert_eq!(
            base(true, true, true, false).verdict(),
            "issued by CrowdRelay, unedited, current"
        );
        assert_eq!(
            base(true, true, false, false).verdict(),
            "issued by CrowdRelay, unedited, but out of date"
        );
        assert_eq!(
            base(true, true, true, true).verdict(),
            "issued by CrowdRelay, then withdrawn by the act"
        );
        assert_eq!(
            base(false, true, true, false).verdict(),
            "issued by CrowdRelay, but edited since"
        );
        // Forgery outranks everything: a document that is expired AND not ours
        // reports the forgery, because that is the fact that matters.
        assert_eq!(
            base(false, false, false, true).verdict(),
            "not issued by CrowdRelay"
        );
    }
}
