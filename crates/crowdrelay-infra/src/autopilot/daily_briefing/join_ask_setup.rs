//! §5: the join-ask setup line — what the weekly ask needs from a person
//! before it can run at all.
//!
//! The briefing's other sections measure activity, and a workspace nobody has
//! set up has none. Without this line its briefing reads as quiet while its
//! growth loop sits unconfigured — or, worse, configured against the shipped
//! default site, which is another band's signup page. This is the one line
//! in the briefing that is fullest when the tenant is newest.
//!
//! Silent when the loop is ready: the brief breaks silence only for things
//! that lie when quiet, and a configured loop with nothing due is telling
//! the truth.

use crowdrelay_application::autopilot::BriefingLocale;
use crowdrelay_domain::join_ask::{JoinAskHold, join_ask_readiness};
use uuid::Uuid;

/// Appends the setup line and its `join_ask_setup` section, if anything is
/// missing.
///
/// Reads through the same pool-backed assembler the autopilot cycle and the
/// attention board use, so all three name the same blocker.
///
/// A failed read degrades rather than aborts. The briefing runs inside the
/// team sweep's transaction, and an advisory line erroring out would roll
/// back housekeeping that already succeeded — the failure `team.rs` already
/// documents for the email capability check. It must not degrade into a
/// quiet day either, so an unread readiness still inserts the section (as
/// `null`), which is enough to withhold the "nothing needs you" line.
pub(super) async fn append_join_ask_setup(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    locale: BriefingLocale,
    label: &str,
    sections: &mut serde_json::Map<String, serde_json::Value>,
    body: &mut String,
) {
    let readiness = match crate::join_ask::load_join_ask_snapshot(pool, workspace_id).await {
        Ok(snapshot) => join_ask_readiness(&snapshot),
        Err(error) => {
            tracing::warn!(
                workspace_id = %workspace_id,
                error = %error,
                "daily briefing could not read join-ask readiness"
            );
            sections.insert("join_ask_setup".to_owned(), serde_json::Value::Null);
            return;
        }
    };
    if readiness.is_empty() {
        return;
    }
    sections.insert(
        "join_ask_setup".to_owned(),
        readiness
            .iter()
            .map(|blocker| {
                serde_json::json!({
                    "platform": blocker.platform,
                    "reason": blocker.hold.as_str(),
                    "remedy": blocker.hold.remedy(),
                })
            })
            .collect(),
    );
    // Deduplicated by phrase: "connect the account" for two unconnected
    // platforms is one errand, and the brief has 1650 characters for the
    // whole day. The panel carries the per-platform breakdown.
    let mut phrases: Vec<&'static str> = Vec::new();
    for blocker in &readiness {
        let phrase = join_ask_hold_phrase(blocker.hold, locale);
        if !phrase.is_empty() && !phrases.contains(&phrase) {
            phrases.push(phrase);
        }
    }
    body.push_str(&format!("{label}: {}\n", phrases.join(" · ")));
}

/// One hold as a phrase somebody can act on, in the crew's own language.
///
/// The domain's `remedy()` is English and belongs to the console. The brief
/// is localized, so the mapping lives here rather than pushing a locale the
/// domain has no business knowing. Deliberately shorter than `remedy()`: the
/// body is capped at 1650 characters and the panel carries the full text.
const fn join_ask_hold_phrase(hold: JoinAskHold, locale: BriefingLocale) -> &'static str {
    match (hold, locale) {
        (JoinAskHold::NoVariants, BriefingLocale::Pl) => "napisz zaproszenie własnymi słowami",
        (JoinAskHold::NoVariants, BriefingLocale::En) => "write the ask in your own words",
        (JoinAskHold::NoSiteUrl, BriefingLocale::Pl) => "ustaw adres strony dla fanów",
        (JoinAskHold::NoSiteUrl, BriefingLocale::En) => "set the member site URL",
        (JoinAskHold::NotConnected, BriefingLocale::Pl) => "podłącz konto",
        (JoinAskHold::NotConnected, BriefingLocale::En) => "connect the account",
        (JoinAskHold::NoInstagramPhoto, BriefingLocale::Pl) => "dodaj zdjęcie",
        (JoinAskHold::NoInstagramPhoto, BriefingLocale::En) => "add a photo",
        (JoinAskHold::NoExecutor, BriefingLocale::Pl) => "kanał jeszcze nieobsługiwany",
        (JoinAskHold::NoExecutor, BriefingLocale::En) => "channel not wired yet",
        (JoinAskHold::SiteUrlInherited, BriefingLocale::Pl) => {
            "ustaw własny adres strony — teraz link prowadzi na domyślną"
        }
        (JoinAskHold::SiteUrlInherited, BriefingLocale::En) => {
            "set your own site URL — the link points at the default site"
        }
        // Never reported: `join_ask_readiness` excludes it, because an ask
        // that went out on schedule is the feature working, not a gap.
        (JoinAskHold::OnCadence, _) => "",
    }
}
