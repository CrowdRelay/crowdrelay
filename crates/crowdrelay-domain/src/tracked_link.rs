//! A link that resolves through the tenant's own redirect surface.
//!
//! Every URL a letter, pitch or ask carries must be one of these. The type
//! is the gate: a composer cannot receive a bare `&str` destination, so a
//! raw YouTube link or a plain site URL cannot be drafted into human-facing
//! copy by mistake. A click on a plain destination leaves no row — the
//! channel that sent it teaches the brain nothing, which is how a hundred
//! outreach emails once produced zero attributable fans.
//!
//! "Tracked" here means exactly two shapes:
//!
//! * `https://{site}/l/{slug}` — the member-site path the tenant's web host
//!   proxies to the API, and
//! * `https://{api}/v1/go/{slug}` — the API's own redirect.
//!
//! Both land on the same handler: it writes the `click_events` row, marks
//! the browser with the attribution cookie, and redirects. Anything else —
//! a foreign host, a deep link into the site, a bare origin — is refused at
//! construction, so a letter can only ever carry a link the ledger sees.

use std::fmt;

use url::Url;

use crate::SmartLinkSlug;

/// A URL that provably passes through the tenant's click-recording redirect.
///
/// Constructed only by [`TrackedLink::parse`] or [`TrackedLink::for_site`].
/// The value inside is the full URL string, ready to print in a letter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedLink(String);

/// Why a URL is not a tracked link — each variant is a real shape the caller
/// must fix rather than a formatting hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackedLinkRefusal {
    /// Not an absolute `https://` URL — a relative path or a scheme-less
    /// string cannot be checked against the trusted hosts.
    NotAbsoluteHttps,
    /// The host is not one of the tenant's own origins. A click on it never
    /// reaches the redirect surface, so it leaves no evidence.
    ForeignHost,
    /// The path is not `/l/{slug}` or `/v1/go/{slug}` — a deep link into the
    /// tenant's own site still bypasses the click recorder, which is the
    /// whole point of carrying the link.
    NotRedirectPath,
    /// The trailing segment is not a valid smart-link slug.
    BadSlug,
}

impl TrackedLinkRefusal {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotAbsoluteHttps => "link must be an absolute https:// URL",
            Self::ForeignHost => {
                "link host is not the tenant's site or API — a click there leaves no row"
            }
            Self::NotRedirectPath => {
                "link must pass through /l/{slug} or /v1/go/{slug} — a direct URL is invisible to the ledger"
            }
            Self::BadSlug => "the redirect's last segment is not a valid smart-link slug",
        }
    }
}

impl fmt::Display for TrackedLinkRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for TrackedLinkRefusal {}

impl TrackedLink {
    /// Validates that `url` is a redirect through one of `trusted_hosts` —
    /// the member-site origin's host and the API's host, e.g.
    /// `&["virya.music", "signal-api.virya.music"]`.
    ///
    /// # Errors
    ///
    /// Each refusal names the concrete shape problem; callers fail closed —
    /// a letter that cannot carry a tracked link carries no link line, and
    /// a send path that cannot mint one must not send.
    pub fn parse(url: &str, trusted_hosts: &[&str]) -> Result<Self, TrackedLinkRefusal> {
        let trimmed = url.trim();
        let parsed = Url::parse(trimmed).map_err(|_| TrackedLinkRefusal::NotAbsoluteHttps)?;
        if parsed.scheme() != "https" {
            return Err(TrackedLinkRefusal::NotAbsoluteHttps);
        }
        let host = parsed.host_str().ok_or(TrackedLinkRefusal::ForeignHost)?;
        if !trusted_hosts
            .iter()
            .any(|trusted| host.eq_ignore_ascii_case(trusted.trim()))
        {
            return Err(TrackedLinkRefusal::ForeignHost);
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(TrackedLinkRefusal::NotRedirectPath);
        }
        let segments: Vec<&str> = parsed
            .path_segments()
            .map(|parts| parts.filter(|part| !part.is_empty()).collect())
            .unwrap_or_default();
        let slug = match segments.as_slice() {
            // `{site}/l/{slug}` — the public-facing short form the tenant's
            // redirects proxy to the API.
            ["l", slug] => *slug,
            // `{api}/v1/go/{slug}` — the API's own redirect path.
            ["v1", "go", slug] => *slug,
            _ => return Err(TrackedLinkRefusal::NotRedirectPath),
        };
        SmartLinkSlug::parse(slug).map_err(|_| TrackedLinkRefusal::BadSlug)?;
        Ok(Self(trimmed.to_owned()))
    }

    /// Builds `{site_origin}/l/{slug}` directly — the canonical print form a
    /// caller mints after ensuring the `smart_links` row exists. The site
    /// origin is trusted by construction (the caller just wrote the row it
    /// resolves), so no host check runs here.
    #[must_use]
    pub fn for_site(site_origin: &str, slug: &SmartLinkSlug) -> Self {
        Self(format!(
            "{}/l/{}",
            site_origin.trim().trim_end_matches('/'),
            slug.as_str()
        ))
    }

    /// The full URL, ready to print in a letter.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TrackedLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The absolute URLs in `text` that do not resolve through
/// `{site_root}/l/{slug}` — the only shape a letter may print.
///
/// The send-path backstop for a gate that otherwise lives in the type system:
/// composers can only receive a `TrackedLink`, so an untracked URL in a body
/// means a bypass — a draft written before the gate existed, a body a person
/// edited after approval, a path that never met a composer. `site_root` is
/// the tenant's member-site origin; `None` is the honest "cannot verify" —
/// every URL reads untracked and the caller must refuse, not wave through.
///
/// Deliberately conservative about what even counts: only absolute `http(s)`
/// URLs. A scheme-less `virya.music` is not a click a browser follows, so it
/// is not the regression this scan exists to catch.
#[must_use]
pub fn untracked_links_in(text: &str, site_root: Option<&str>) -> Vec<String> {
    letter_links(text)
        .filter(|token| tracked_site_slug(token, site_root).is_none())
        .map(str::to_owned)
        .collect()
}

/// The valid member-site slugs in a letter, or `None` when any absolute
/// URL bypasses the redirect. Dispatch checks these against the live ledger;
/// a correct URL shape alone does not prove that the redirect exists.
#[must_use]
pub fn tracked_site_link_slugs_in(text: &str, site_root: Option<&str>) -> Option<Vec<String>> {
    letter_links(text)
        .map(|token| tracked_site_slug(token, site_root))
        .collect()
}

fn letter_links(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']')
    })
    .filter(|token| {
        Url::parse(token)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
    })
}

fn tracked_site_slug(token: &str, site_root: Option<&str>) -> Option<String> {
    let root = site_root?.trim().trim_end_matches('/');
    let origin = Url::parse(root).ok()?;
    if origin.scheme() != "https"
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return None;
    }
    let prefix = format!("{root}/l/");
    let slug = token.strip_prefix(&prefix)?;
    SmartLinkSlug::parse(slug)
        .ok()
        .map(|slug| slug.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTS: &[&str] = &["virya.music", "signal-api.virya.music"];

    #[test]
    fn accepts_site_short_link() -> Result<(), Box<dyn std::error::Error>> {
        let link = TrackedLink::parse("https://virya.music/l/summer-drop", HOSTS)?;
        assert_eq!(link.as_str(), "https://virya.music/l/summer-drop");
        Ok(())
    }

    #[test]
    fn accepts_api_go_link() -> Result<(), Box<dyn std::error::Error>> {
        TrackedLink::parse("https://signal-api.virya.music/v1/go/summer-drop", HOSTS)?;
        Ok(())
    }

    #[test]
    fn rejects_the_shapes_that_leave_no_evidence() {
        // The raw-YouTube and plain-site shapes that reached send queues
        // before the gate existed.
        assert_eq!(
            TrackedLink::parse("https://youtube.com/watch?v=abc", HOSTS),
            Err(TrackedLinkRefusal::ForeignHost)
        );
        assert_eq!(
            TrackedLink::parse("https://virya.music/signal", HOSTS),
            Err(TrackedLinkRefusal::NotRedirectPath)
        );
        assert_eq!(
            TrackedLink::parse("https://virya.music", HOSTS),
            Err(TrackedLinkRefusal::NotRedirectPath)
        );
        // A foreign host wearing the redirect's clothes still resolves to
        // somebody else's redirect — the click never reaches our recorder.
        assert_eq!(
            TrackedLink::parse("https://evil.example/l/fake", HOSTS),
            Err(TrackedLinkRefusal::ForeignHost)
        );
        assert_eq!(
            TrackedLink::parse("http://virya.music/l/x", HOSTS),
            Err(TrackedLinkRefusal::NotAbsoluteHttps)
        );
        assert_eq!(
            TrackedLink::parse("virya.music/l/x", HOSTS),
            Err(TrackedLinkRefusal::NotAbsoluteHttps)
        );
        assert_eq!(
            TrackedLink::parse("https://virya.music/l/", HOSTS),
            Err(TrackedLinkRefusal::NotRedirectPath)
        );
        assert_eq!(
            TrackedLink::parse("https://virya.music/l/x?utm_source=ig", HOSTS),
            Err(TrackedLinkRefusal::NotRedirectPath)
        );
        // Slug characters follow SmartLinkSlug's rule — an invalid segment
        // would 404 at the redirect anyway.
        assert_eq!(
            TrackedLink::parse("https://virya.music/l/bad.slug", HOSTS),
            Err(TrackedLinkRefusal::BadSlug)
        );
    }

    #[test]
    fn for_site_builds_the_print_form() {
        let slug = SmartLinkSlug::parse("signal-install").unwrap();
        let link = TrackedLink::for_site("https://virya.music/", &slug);
        assert_eq!(link.as_str(), "https://virya.music/l/signal-install");
    }

    #[test]
    fn untracked_links_flags_what_a_letter_cannot_carry() {
        let site = Some("https://virya.music");
        // The two historical violations: a raw YouTube link and the plain
        // site URL — both clicked, both invisible.
        let body = "Listen: https://www.youtube.com/watch?v=abc\n\
                    Music: https://virya.music";
        let flagged = untracked_links_in(body, site);
        assert_eq!(flagged.len(), 2);
        // The tracked forms pass — the printed site link and a release pitch.
        let body = "Listen: https://virya.music/l/release-rytual\n\
                    Music: https://virya.music/l/site";
        assert!(untracked_links_in(body, site).is_empty());
        // A foreign host wearing the redirect's clothes is not tracked — the
        // prefix is the tenant's own origin, not the path's shape.
        assert_eq!(
            untracked_links_in("https://evil.example/l/x", site).len(),
            1
        );
        // No site configured: nothing can verify, so every URL is untracked.
        assert_eq!(
            untracked_links_in("https://virya.music/l/site", None).len(),
            1
        );
        // Scheme-less text is not a click — not flagged.
        assert!(untracked_links_in("find us at virya.music", site).is_empty());
    }

    #[test]
    fn a_redirect_prefix_without_a_valid_slug_is_not_tracked() {
        let site = Some("https://virya.music");
        for link in [
            "https://virya.music/l/",
            "https://virya.music/l/bad.slug",
            "https://virya.music/l/site/extra",
            "https://virya.music/l/site?next=elsewhere",
            "https://virya.music/l/site#fragment",
        ] {
            assert_eq!(untracked_links_in(link, site), vec![link.to_owned()]);
            assert_eq!(tracked_site_link_slugs_in(link, site), None);
        }
        assert_eq!(
            tracked_site_link_slugs_in(
                "Listen: https://virya.music/l/release-x\nSite: https://virya.music/l/site",
                site,
            ),
            Some(vec!["release-x".to_owned(), "site".to_owned()])
        );
        assert_eq!(
            tracked_site_link_slugs_in("No URLs in this letter", None),
            Some(vec![])
        );
        assert_eq!(
            tracked_site_link_slugs_in("http://virya.music/l/site", Some("http://virya.music")),
            None
        );
    }
}
