//! What a connector may read (1A.6, open decision 11).
//!
//! `fanbase_connections.scan_scope` is the tenant's recorded answer to
//! "what is this connection allowed to scan". The column is an opaque jsonb
//! object; this module is the vocabulary. A scope the platform cannot
//! honour is a parse error at write time, never a silently wider read at
//! scan time.
//!
//! The empty state is not a scope: a NULL column means the question was
//! never answered, and the workers treat it as "scan nothing and say why"
//! rather than "everything". An absent boundary must fail closed.

/// The boundary a tenant chose for one connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanScope {
    /// The operator's explicit choice to scan the whole account. Stored,
    /// not assumed — the JSON `{"kind": "whole_account"}` is the record.
    WholeAccount,
    /// Drive only: files inside these folders (subfolders are expanded by
    /// the connector, bounded).
    Folders { folder_ids: Vec<String> },
    /// Drive only: one shared drive.
    SharedDrive { drive_id: String },
    /// Gmail only: mail the tenant sent — the contacts the band actually
    /// wrote to, which is the whole point of the scan.
    SentOnly,
    /// Gmail only: one label, by name.
    Label { label: String },
    /// Drive files modified on/after the date; Gmail mail dated on/after
    /// it. The resource differs, the boundary is the same.
    Since { since: time::Date },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanScopeError {
    /// The object parses but the kind does not exist on this platform —
    /// a `sent_only` Drive or a `folder` Gmail is a caller's bug, not a
    /// tenant's typo.
    KindNotForPlatform,
    /// The object is malformed: unknown kind, missing fields, bad types.
    Malformed,
}

impl std::fmt::Display for ScanScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KindNotForPlatform => {
                write!(f, "that scope does not exist for this platform")
            }
            Self::Malformed => write!(f, "the scope is not a shape this platform understands"),
        }
    }
}

impl std::error::Error for ScanScopeError {}

/// Drive file/folder/shared-drive ids are `[A-Za-z0-9_-]+`. Anything
/// outside that alphabet — a quote above all — would land inside the Drive
/// query string and break the scan, so the charset is part of "well formed".
fn is_drive_id(id: &str) -> bool {
    id.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ScanScope {
    /// Parse and platform-check a stored/requested scope. `platform` is the
    /// `fanbase_connections.platform` value — the vocabulary is per-platform
    /// because a Drive folder means nothing to Gmail and a label means
    /// nothing to Drive.
    pub fn parse(platform: &str, scope: &serde_json::Value) -> Result<ScanScope, ScanScopeError> {
        let kind = scope
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or(ScanScopeError::Malformed)?;
        match (platform, kind) {
            ("gdrive", "whole_account") | ("gmail", "whole_account") => Ok(ScanScope::WholeAccount),
            ("gdrive", "folder") => {
                // All-or-nothing: one bad id in a list of three used to be
                // dropped, scanning a narrower scope than the tenant chose
                // without ever saying so. A malformed entry fails the whole
                // write — the operator fixes the input instead of trusting
                // a scope that silently covers less.
                let ids = scope
                    .get("folder_ids")
                    .and_then(serde_json::Value::as_array)
                    .ok_or(ScanScopeError::Malformed)?
                    .iter()
                    .map(|entry| {
                        entry
                            .as_str()
                            .map(str::trim)
                            .filter(|id| !id.is_empty() && id.len() <= 128 && is_drive_id(id))
                            .map(str::to_string)
                            .ok_or(ScanScopeError::Malformed)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                // An empty folder list is not "nothing" — it is a scope that
                // can never match, which a tenant reads as a broken scan.
                // Refusing it at write time keeps that confusion out.
                if ids.is_empty() || ids.len() > 20 {
                    return Err(ScanScopeError::Malformed);
                }
                Ok(ScanScope::Folders { folder_ids: ids })
            }
            ("gdrive", "shared_drive") => {
                let drive_id = scope
                    .get("drive_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty() && id.len() <= 128 && is_drive_id(id))
                    .ok_or(ScanScopeError::Malformed)?;
                Ok(ScanScope::SharedDrive {
                    drive_id: drive_id.to_string(),
                })
            }
            ("gmail", "sent_only") => Ok(ScanScope::SentOnly),
            ("gmail", "label") => {
                let label = scope
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && l.len() <= 128)
                    .ok_or(ScanScopeError::Malformed)?;
                Ok(ScanScope::Label {
                    label: label.to_string(),
                })
            }
            ("gdrive", "since") | ("gmail", "since") => {
                let raw = scope
                    .get("since")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .ok_or(ScanScopeError::Malformed)?;
                let since =
                    time::Date::parse(raw, &time::format_description::well_known::Iso8601::DATE)
                        .map_err(|_| ScanScopeError::Malformed)?;
                Ok(ScanScope::Since { since })
            }
            ("gdrive", _) | ("gmail", _) => Err(ScanScopeError::KindNotForPlatform),
            // Scoping is only defined for the connectors that read the
            // account. Any other platform storing a scope is a caller bug —
            // refuse it rather than keep a promise nothing enforces.
            (_, _) => Err(ScanScopeError::KindNotForPlatform),
        }
    }

    /// Whether this connection has a chosen boundary at all. A `None` stored
    /// column is "not chosen" — the workers scan nothing and report why.
    pub fn stored(
        platform: &str,
        stored: Option<&serde_json::Value>,
    ) -> Result<Option<ScanScope>, ScanScopeError> {
        stored
            .map(|scope| ScanScope::parse(platform, scope))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_account_is_an_explicit_choice() {
        assert_eq!(
            ScanScope::parse("gdrive", &json!({"kind": "whole_account"})),
            Ok(ScanScope::WholeAccount)
        );
        assert_eq!(
            ScanScope::parse("gmail", &json!({"kind": "whole_account"})),
            Ok(ScanScope::WholeAccount)
        );
    }

    #[test]
    fn each_platform_refuses_the_others_vocabulary() {
        assert_eq!(
            ScanScope::parse("gdrive", &json!({"kind": "sent_only"})),
            Err(ScanScopeError::KindNotForPlatform)
        );
        assert_eq!(
            ScanScope::parse("gmail", &json!({"kind": "folder", "folder_ids": ["a"]})),
            Err(ScanScopeError::KindNotForPlatform)
        );
        assert_eq!(
            ScanScope::parse("spotify", &json!({"kind": "whole_account"})),
            Err(ScanScopeError::KindNotForPlatform)
        );
    }

    #[test]
    fn malformed_scopes_fail_closed() {
        for scope in [
            json!({}),
            json!({"kind": "folder"}),
            json!({"kind": "folder", "folder_ids": []}),
            json!({"kind": "shared_drive"}),
            json!({"kind": "since", "since": "not a date"}),
            json!({"kind": "label"}),
            json!({"kind": "something_else"}),
            // A quote in an id would land inside the Drive query string —
            // the id alphabet is part of "well formed", not the worker's
            // problem.
            json!({"kind": "folder", "folder_ids": ["a'b"]}),
            json!({"kind": "shared_drive", "drive_id": "0' OR 1=1"}),
        ] {
            for platform in ["gdrive", "gmail"] {
                assert!(
                    ScanScope::parse(platform, &scope).is_err(),
                    "{platform} must refuse {scope}"
                );
            }
        }
    }

    #[test]
    fn unset_is_not_a_scope() {
        assert_eq!(ScanScope::stored("gdrive", None), Ok(None));
        assert_eq!(
            ScanScope::stored("gmail", Some(&json!({"kind": "sent_only"}))),
            Ok(Some(ScanScope::SentOnly))
        );
    }
}
