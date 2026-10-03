//! Asking the platform what the publish token may do, before it is used to post.
//!
//! Every read the worker makes with the Meta credential succeeds with a token
//! that cannot publish: a sync proves *read* access. The first Facebook post on
//! 2026-09-28 was refused with `(#200) … requires pages_manage_posts` although
//! the connection read `working`, and nothing had said so beforehand. Graph's
//! `debug_token` states a token's scopes without posting anything; this records
//! the answer so readiness can refuse to call a rail executable on a token the
//! platform says cannot publish.
//!
//! What this never does: post, write to a platform, log the token, or turn a
//! failure to ask into an answer. Anything short of the platform actually
//! stating the scopes is [`ScopeCheck::Unverifiable`] and records nothing — an
//! old answer ages out rather than being replaced by a guess.

use serde_json::Value;
use std::time::Duration;

use super::{GRAPH_API_VERSION, SocialPostExecutorWorker};

/// How often the token is re-checked. Scopes change rarely; a rotated or
/// downgraded token is caught within a working day.
pub(crate) const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// What came back from asking.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ScopeCheck {
    /// The platform stated the token's scopes (possibly none).
    Answered(Vec<String>),
    /// The platform stated the token is not valid: it can publish nothing.
    InvalidToken,
    /// Not an answer about scopes. The reason carries no token and no body.
    Unverifiable(String),
}

/// Reads a `debug_token` response. Pure, so every shape the endpoint can take is
/// a test and not a production surprise.
pub(crate) fn parse_debug_token(status: u16, body: &str) -> ScopeCheck {
    if status != 200 {
        return ScopeCheck::Unverifiable(format!("graph answered HTTP {status}"));
    }
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return ScopeCheck::Unverifiable("graph answer was not JSON".to_owned());
    };
    let Some(data) = json.get("data") else {
        return ScopeCheck::Unverifiable("graph answer had no data".to_owned());
    };
    if data.get("is_valid").and_then(Value::as_bool) == Some(false) {
        return ScopeCheck::InvalidToken;
    }
    let plain = data.get("scopes").and_then(Value::as_array);
    let granular = data.get("granular_scopes").and_then(Value::as_array);
    if plain.is_none() && granular.is_none() {
        // Valid token, no scope information: the platform did not say.
        return ScopeCheck::Unverifiable("graph answer carried no scopes".to_owned());
    }
    let mut scopes: Vec<String> = plain
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    scopes.extend(
        granular
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("scope").and_then(Value::as_str))
            .map(str::to_owned),
    );
    scopes.sort();
    scopes.dedup();
    ScopeCheck::Answered(scopes)
}

impl SocialPostExecutorWorker {
    /// Asks Graph what the publish token may do and records an actual answer.
    ///
    /// Read-only: `debug_token` inspects a token and posts nothing. Does nothing
    /// when no token is configured. Returns what was found, for logging.
    pub(crate) async fn verify_publish_scopes(&self) -> Option<ScopeCheck> {
        let token = self.facebook_page_access_token.as_deref()?;
        let response = self
            .http_client
            .get(format!(
                "https://graph.facebook.com/{GRAPH_API_VERSION}/debug_token"
            ))
            .query(&[("input_token", token), ("access_token", token)])
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await;
        let check = match response {
            Ok(response) => {
                let status = response.status().as_u16();
                match response.text().await {
                    Ok(body) => parse_debug_token(status, &body),
                    Err(_) => ScopeCheck::Unverifiable("graph answer unreadable".to_owned()),
                }
            }
            // `without_url`: the request URL carries the token in its query.
            Err(error) => {
                ScopeCheck::Unverifiable(format!("graph request failed: {}", error.without_url()))
            }
        };
        let recorded = match &check {
            ScopeCheck::Answered(scopes) => Some(scopes.as_slice()),
            ScopeCheck::InvalidToken => Some(&[][..]),
            ScopeCheck::Unverifiable(_) => None,
        };
        if let Some(scopes) = recorded
            && let Err(error) = crowdrelay_infra::lane_ledger::record_publish_scopes(
                &self.pool,
                self.workspace_id.into_uuid(),
                scopes,
            )
            .await
        {
            tracing::warn!(%error, "could not record the publish scope check");
        }
        Some(check)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_token_with_publish_scope_is_an_answer() {
        let body = r#"{"data":{"app_id":"1","type":"PAGE","is_valid":true,
            "scopes":["pages_manage_posts","pages_read_engagement","instagram_content_publish"]}}"#;
        assert_eq!(
            parse_debug_token(200, body),
            ScopeCheck::Answered(vec![
                "instagram_content_publish".to_owned(),
                "pages_manage_posts".to_owned(),
                "pages_read_engagement".to_owned(),
            ])
        );
    }

    #[test]
    fn granular_scopes_count_and_duplicates_collapse() {
        let body = r#"{"data":{"is_valid":true,"scopes":["pages_manage_posts"],
            "granular_scopes":[{"scope":"pages_manage_posts","target_ids":["1"]},
                               {"scope":"pages_read_engagement","target_ids":["1"]}]}}"#;
        assert_eq!(
            parse_debug_token(200, body),
            ScopeCheck::Answered(vec![
                "pages_manage_posts".to_owned(),
                "pages_read_engagement".to_owned(),
            ])
        );
    }

    #[test]
    fn a_read_only_token_is_a_real_answer_and_an_empty_list_too() {
        let read_only = r#"{"data":{"is_valid":true,"scopes":["pages_read_engagement"]}}"#;
        assert_eq!(
            parse_debug_token(200, read_only),
            ScopeCheck::Answered(vec!["pages_read_engagement".to_owned()])
        );
        let none = r#"{"data":{"is_valid":true,"scopes":[]}}"#;
        assert_eq!(
            parse_debug_token(200, none),
            ScopeCheck::Answered(Vec::new())
        );
    }

    #[test]
    fn a_token_the_platform_calls_invalid_publishes_nothing() {
        let body = r#"{"data":{"is_valid":false,"error":{"code":190,"message":"expired"}}}"#;
        assert_eq!(parse_debug_token(200, body), ScopeCheck::InvalidToken);
    }

    #[test]
    fn anything_that_is_not_the_platform_stating_scopes_is_unverifiable() {
        for (status, body) in [
            (
                400,
                r#"{"error":{"message":"(#100) Invalid parameter","code":100}}"#,
            ),
            (500, ""),
            (200, "<html>gateway</html>"),
            (200, r#"{"unexpected":true}"#),
            // Valid token but the answer does not carry scope information.
            (200, r#"{"data":{"is_valid":true,"type":"SYSTEM_USER"}}"#),
        ] {
            assert!(
                matches!(parse_debug_token(status, body), ScopeCheck::Unverifiable(_)),
                "{status} {body}"
            );
        }
    }

    #[test]
    fn an_unverifiable_reason_never_carries_a_body_or_a_token() {
        let leaky = r#"{"error":{"message":"Invalid OAuth access token EAAB-SECRET-TOKEN"}}"#;
        let ScopeCheck::Unverifiable(reason) = parse_debug_token(400, leaky) else {
            panic!("expected unverifiable");
        };
        assert!(!reason.contains("SECRET"), "{reason}");
    }
}
