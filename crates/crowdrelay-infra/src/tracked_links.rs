//! The one write that makes a link a letter may print.
//!
//! A URL put in front of a person — a pitch's listen link, the signature
//! site line, an invitation's member-area link — must resolve through
//! `/l/{slug}` on the tenant's member site. The web host proxies that path to
//! the API's click-recording redirect, so a click leaves a `click_events`
//! row and an attribution cookie instead of teaching the ledger nothing.
//!
//! [`ensure_smart_link`] owns the row that makes a slug real; the domain's
//! [`TrackedLink`] owns the value a composer can print. Both stay together:
//! a link whose row was never written 404s, and a row nobody can print is a
//! link nobody clicks.

use crowdrelay_domain::{DestinationUrl, SmartLinkSlug, TrackedLink};
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

/// Writes or repairs the `smart_links` row `slug` → `destination`, then
/// returns the link in printable member-site form, `{site}/l/{slug}`.
///
/// `Ok(None)` — the caller then shortens the letter or refuses rather than
/// printing a URL the ledger cannot see — when:
///
/// * `destination` is not an absolute http(s) URL (a redirect to nowhere is
///   worse than no link), or
/// * the tenant has no member site — the `/l/` redirect path lives on it.
///
/// The row is still written when only the print form is missing: the API's
/// own `/v1/go/{slug}` resolves it, and a member site configured later picks
/// the same link up. `channel_*` name the placement when the row belongs to
/// one (`email`, a platform name); `None` leaves an existing identity alone
/// — a letter reprinting a shared link must not strip the attribution it
/// was minted with. An existing `campaign_id` survives for the same reason.
pub(crate) async fn ensure_smart_link(
    connection: &mut PgConnection,
    workspace_id: Uuid,
    slug: &str,
    destination: &str,
    site_root: Option<&str>,
    channel_source: Option<&str>,
    channel_creative: Option<&str>,
) -> Result<Option<TrackedLink>, sqlx::Error> {
    let Ok(destination) = DestinationUrl::parse(destination) else {
        return Ok(None);
    };
    // A bad slug is a caller bug — only literals and already-parsed values
    // reach here — and it must surface as one rather than disappear into
    // `None`, which would silently print a letter without its link.
    let slug = SmartLinkSlug::parse(slug).map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    sqlx::query(
        r#"
        INSERT INTO smart_links (workspace_id, slug, destination_url, active,
                                 channel_source, channel_creative)
        VALUES ($1, $2, $3, true, $4, $5)
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            active = true,
            channel_source = COALESCE(EXCLUDED.channel_source, smart_links.channel_source),
            channel_creative = COALESCE(EXCLUDED.channel_creative, smart_links.channel_creative)
        "#,
    )
    .bind(workspace_id)
    .bind(slug.as_str())
    .bind(destination.as_str())
    .bind(channel_source)
    .bind(channel_creative)
    .execute(&mut *connection)
    .await?;
    Ok(site_root.map(|root| TrackedLink::for_site(root, &slug)))
}

/// `ensure_smart_link` for callers holding a pool rather than a connection —
/// the latarnik invite and the pool-scoped sender identity mint outside a
/// transaction.
pub(crate) async fn ensure_smart_link_on_pool(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    slug: &str,
    destination: &str,
    site_root: Option<&str>,
    channel_source: Option<&str>,
    channel_creative: Option<&str>,
) -> Result<Option<TrackedLink>, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    ensure_smart_link(
        &mut connection,
        workspace_id,
        slug,
        destination,
        site_root,
        channel_source,
        channel_creative,
    )
    .await
}

/// `ensure_smart_link` inside a decision-persistence transaction — the row
/// rolls back with the action it was minted for.
pub(crate) async fn ensure_smart_link_in_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    slug: &str,
    destination: &str,
    site_root: Option<&str>,
    channel_source: Option<&str>,
    channel_creative: Option<&str>,
) -> Result<Option<TrackedLink>, sqlx::Error> {
    ensure_smart_link(
        transaction,
        workspace_id,
        slug,
        destination,
        site_root,
        channel_source,
        channel_creative,
    )
    .await
}
