//! Where an OAuth provider sends the operator back to.
//!
//! The callback has to land on the API that started the flow: it verifies a
//! state cookie that API set on its own host, and it stores the credential
//! under that API's workspace. A redirect aimed at any other host fails the
//! cookie check — so a callback hardcoded to the first tenant's API host made
//! Google Drive, Gmail and TikTok connections fail for every other tenant.

/// The first tenant's API host, which every flow used to hardcode. Kept only
/// as the fallback for a deployment that never set its own origin, so one
/// that relied on the old value keeps working unchanged.
const LEGACY_CALLBACK_ORIGIN: &str = "https://signal-api.virya.music";

/// The callback URL for `provider`, on this deployment's own public API
/// origin (`CROWDRELAY_PUBLIC_API_ORIGIN`).
///
/// The provider only honours a redirect registered on its OAuth client, so a
/// new tenant's callback must be added there as well — this puts the right
/// address in the request, and cannot register it.
pub(crate) fn oauth_redirect_uri(public_api_origin: Option<&url::Url>, provider: &str) -> String {
    let origin = match public_api_origin {
        Some(origin) => origin.as_str().trim_end_matches('/').to_owned(),
        None => {
            tracing::warn!(
                provider,
                "CROWDRELAY_PUBLIC_API_ORIGIN is unset; the OAuth callback falls back to the \
                 legacy shared host, which only works for the tenant that host serves"
            );
            LEGACY_CALLBACK_ORIGIN.to_owned()
        }
    };
    format!("{origin}/v1/public/connections/{provider}/callback")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_lands_on_this_deployments_own_host() -> Result<(), url::ParseError> {
        let origin = url::Url::parse("https://api.band.example")?;
        assert_eq!(
            oauth_redirect_uri(Some(&origin), "gdrive"),
            "https://api.band.example/v1/public/connections/gdrive/callback"
        );
        Ok(())
    }

    #[test]
    fn the_first_tenants_configured_origin_yields_its_existing_callback()
    -> Result<(), url::ParseError> {
        // Virya's production env sets exactly this origin, so its registered
        // callbacks are unchanged by deriving them.
        let origin = url::Url::parse("https://signal-api.virya.music")?;
        for provider in ["gdrive", "gmail", "tiktok"] {
            assert_eq!(
                oauth_redirect_uri(Some(&origin), provider),
                format!("https://signal-api.virya.music/v1/public/connections/{provider}/callback")
            );
        }
        Ok(())
    }

    #[test]
    fn an_unset_origin_keeps_the_legacy_callback() {
        assert_eq!(
            oauth_redirect_uri(None, "gmail"),
            "https://signal-api.virya.music/v1/public/connections/gmail/callback"
        );
    }
}
