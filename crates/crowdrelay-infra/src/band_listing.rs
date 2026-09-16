//! Storage for the profile a band publishes when it wants representation.
//!
//! The policy lives in `crowdrelay_domain::listing`; this is the table behind
//! it. Two things about the shape are deliberate and easy to undo by accident.
//!
//! **Only the band writes here.** No trigger fills a claim from fan rows and no
//! sync infers a city from geography. If a number is in a listing, a person
//! typed it — which is what makes the domain's `redact` a boundary rather than
//! a filter over data the reader could have reached another way. A label
//! reading a tenant's calendar is one tenant reading another's commercial
//! position; a label reading a listing is the band advertising, and the
//! difference is entirely that nothing arrived here on its own.
//!
//! **Saving is not publishing.** `save` never changes visibility. A band edits
//! a draft as often as it likes and the listing stays unlisted until `publish`
//! is called, which is the only path that runs the domain's review. That split
//! is why a half-finished listing cannot leak: there is no code path from
//! "typed something" to "visible".

use crowdrelay_domain::listing::{BandListing, ListedClaim, ListingVisibility};
use crowdrelay_domain::value_tier::MetricValueTier;
use sqlx::{PgPool, Row};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum BandListingError {
    #[error("band listing database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("no listing for this workspace")]
    NotFound,
    /// The domain refused it. Carries the band-facing sentence rather than a
    /// code, because the caller's job is to show it, not to translate it.
    #[error("{0}")]
    Refused(String),
}

fn tier_to_storage(tier: MetricValueTier) -> &'static str {
    match tier {
        MetricValueTier::Vanity => "vanity",
        MetricValueTier::Intermediate => "intermediate",
        MetricValueTier::Downstream => "downstream",
    }
}

/// An unknown tier reads as `Vanity` rather than failing the load.
///
/// The CHECK constraint makes an unknown value unreachable in practice. If one
/// ever appears, treating it as the weakest tier means the listing is refused
/// for having nothing above vanity — which surfaces the problem to the band
/// instead of publishing a claim nobody can classify.
fn tier_from_storage(value: &str) -> MetricValueTier {
    match value {
        "downstream" => MetricValueTier::Downstream,
        "intermediate" => MetricValueTier::Intermediate,
        _ => MetricValueTier::Vanity,
    }
}

/// Anything unrecognised is unlisted. The safe direction for a visibility
/// column is always "nobody sees it".
fn visibility_from_storage(value: &str) -> ListingVisibility {
    match value {
        "admitted_readers" => ListingVisibility::AdmittedReaders,
        _ => ListingVisibility::Unlisted,
    }
}

/// The band's own view of its listing: the content plus the publication
/// state — when it went live and the token that admits a reader. Nothing in
/// here is redacted; this is the owner looking at its own row.
pub struct ListingState {
    pub listing: BandListing,
    pub share_token: Uuid,
    pub published_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
}

#[derive(Clone)]
pub struct PostgresBandListingRepository {
    pool: PgPool,
}

impl PostgresBandListingRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Writes the band's draft. Never changes visibility.
    ///
    /// A listing that does not exist yet is created unlisted. One that is
    /// already published stays published and its content changes under the
    /// reader — which is what editing a live profile means, and why `publish`
    /// re-runs the review rather than trusting the last one.
    pub async fn save(
        &self,
        workspace_id: Uuid,
        listing: &BandListing,
    ) -> Result<(), BandListingError> {
        let mut transaction = self.pool.begin().await?;

        sqlx::query(
            r#"
            INSERT INTO viryaos_band_listings
                (workspace_id, act_name, genre_tags, cities, published_dates, seeking)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (workspace_id) DO UPDATE
            SET act_name = EXCLUDED.act_name,
                genre_tags = EXCLUDED.genre_tags,
                cities = EXCLUDED.cities,
                published_dates = EXCLUDED.published_dates,
                seeking = EXCLUDED.seeking,
                updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(listing.act_name.trim())
        .bind(&listing.genre_tags)
        .bind(&listing.cities)
        .bind(&listing.published_dates)
        .bind(&listing.seeking)
        .execute(&mut *transaction)
        .await?;

        // Claims are replaced wholesale: they are an ordered list the band
        // edits as one thing, and merging them by label would silently keep a
        // claim the band deleted.
        sqlx::query("DELETE FROM viryaos_band_listing_claims WHERE workspace_id = $1")
            .bind(workspace_id)
            .execute(&mut *transaction)
            .await?;

        for (position, claim) in listing.claims.iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO viryaos_band_listing_claims
                    (workspace_id, position, label, value, tier, basis)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
            )
            .bind(workspace_id)
            .bind(i32::try_from(position).unwrap_or(i32::MAX))
            .bind(claim.label.trim())
            .bind(claim.value)
            .bind(tier_to_storage(claim.tier))
            .bind(claim.basis.trim())
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;
        Ok(())
    }

    /// The band's own view: everything it has typed, supported or not.
    pub async fn load(&self, workspace_id: Uuid) -> Result<Option<BandListing>, BandListingError> {
        let Some(row) = sqlx::query(
            r#"
            SELECT act_name, genre_tags, cities, published_dates, seeking, visibility
            FROM viryaos_band_listings WHERE workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };

        let claims = sqlx::query(
            r#"
            SELECT label, value, tier, basis
            FROM viryaos_band_listing_claims
            WHERE workspace_id = $1
            ORDER BY position
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|claim| ListedClaim {
            label: claim.get::<String, _>("label"),
            value: claim.get::<Option<i64>, _>("value"),
            tier: tier_from_storage(&claim.get::<String, _>("tier")),
            basis: claim.get::<String, _>("basis"),
        })
        .collect();

        Ok(Some(BandListing {
            act_name: row.get("act_name"),
            genre_tags: row.get("genre_tags"),
            cities: row.get("cities"),
            claims,
            published_dates: row.get("published_dates"),
            seeking: row.get("seeking"),
            visibility: visibility_from_storage(&row.get::<String, _>("visibility")),
        }))
    }

    /// The band's own view plus publication metadata — what the editor shows.
    ///
    /// `load` returns the content alone; this adds the token the share link
    /// is built from and the timestamps that answer "is it live" without a
    /// second query.
    pub async fn load_state(
        &self,
        workspace_id: Uuid,
    ) -> Result<Option<ListingState>, BandListingError> {
        let Some(listing) = self.load(workspace_id).await? else {
            return Ok(None);
        };
        let row = sqlx::query(
            r#"
            SELECT share_token, published_at, updated_at
            FROM viryaos_band_listings WHERE workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(Some(ListingState {
            listing,
            share_token: row.get("share_token"),
            published_at: row.get("published_at"),
            updated_at: row.get("updated_at"),
        }))
    }

    /// What a link-holder receives: the listing only when the token matches
    /// and the domain's `redact` allows it.
    ///
    /// The token is the admission — a reader has the link and nothing else,
    /// so the lookup is by token alone. A correct token on an unlisted
    /// listing still returns nothing, because unlisting is the band taking
    /// the ad down, and "the link stopped working" is what taking it down
    /// means.
    pub async fn read_visible_by_token(
        &self,
        share_token: Uuid,
    ) -> Result<Option<BandListing>, BandListingError> {
        let Some(workspace_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT workspace_id FROM viryaos_band_listings WHERE share_token = $1",
        )
        .bind(share_token)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let Some(state) = self.load_state(workspace_id).await? else {
            return Ok(None);
        };
        Ok(crowdrelay_domain::listing::redact(&state.listing))
    }

    /// Mints a fresh share token, invalidating every link already sent.
    /// Returns the new token so the caller can show the new link.
    pub async fn rotate_share_token(&self, workspace_id: Uuid) -> Result<Uuid, BandListingError> {
        let token = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE viryaos_band_listings
            SET share_token = gen_random_uuid(), updated_at = now()
            WHERE workspace_id = $1
            RETURNING share_token
            "#,
        )
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(BandListingError::NotFound)?;
        Ok(token)
    }

    /// Runs the domain's review and makes the listing visible if it passes.
    ///
    /// The review runs here rather than at save time because a draft is allowed
    /// to be incomplete. Publication is the moment the claims have to hold up,
    /// and re-running it on every publish means an edit to a live listing
    /// cannot quietly degrade it below the bar it originally cleared.
    pub async fn publish(&self, workspace_id: Uuid) -> Result<(), BandListingError> {
        let listing = self
            .load(workspace_id)
            .await?
            .ok_or(BandListingError::NotFound)?;

        crowdrelay_domain::listing::review_listing(&listing)
            .map_err(|refusal| BandListingError::Refused(refusal.message()))?;

        sqlx::query(
            r#"
            UPDATE viryaos_band_listings
            SET visibility = 'admitted_readers', published_at = now(), updated_at = now()
            WHERE workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Takes the listing out of view. The content stays, so a band that
    /// unlists to fix something does not retype it.
    pub async fn unlist(&self, workspace_id: Uuid) -> Result<(), BandListingError> {
        let changed = sqlx::query(
            r#"
            UPDATE viryaos_band_listings
            SET visibility = 'unlisted', published_at = NULL, updated_at = now()
            WHERE workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(BandListingError::NotFound);
        }
        Ok(())
    }

    // There is deliberately no `is_published` here.
    //
    // It was written, and it was the wrong shape. The only caller that wants
    // the answer is the representation approach gate, and that gate has to
    // read the visibility inside its own transaction — at request time under
    // the advisory lock, and at dispatch with `FOR UPDATE` — so that a
    // `publish` or `unlist` racing the send cannot flip the answer between
    // the check and the emit. A method on this repository takes its own
    // connection from the pool and therefore cannot join either transaction,
    // which makes it a check that looks authoritative and is not.
    //
    // So the gate's two call sites in `representation.rs` and
    // `autopilot/operations/execution.rs` query the column themselves. Adding
    // the convenience method back would give a future caller a way to ask the
    // question without the lock, which is the failure this note exists to
    // prevent.
}
