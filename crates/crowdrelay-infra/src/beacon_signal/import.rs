//! Import researched contacts into the beacon roster.
//!
//! Two pools of researched contacts exist and neither was reachable from the
//! beacon console. `agent_outreach_targets` holds what the agents proposed;
//! `viryaos_outreach_candidates` holds routes read out of published pages and
//! screened. Between them production carries dozens of named stations,
//! curators and writers — while the roster itself had three rows.
//!
//! The beacon network's own `discover` path never ran (zero discovery runs in
//! production), so the research that *did* happen had nowhere to go. This
//! turns it into beacons.
//!
//! Two properties the import must hold:
//!
//! * **Idempotent.** Provenance goes into `metadata.imported_from`, and the
//!   roster's own uniqueness key catches the rest. Pressing Import twice adds
//!   nothing the second time.
//! * **Non-promoting.** An imported beacon is `verified=false` and
//!   `accepts_outreach=false`. Research says a contact exists, not that it
//!   consented to be contacted. Everything imported here has to pass the same
//!   operator approval as any other beacon before the invite pipeline will
//!   touch it — which the roster's partial index on
//!   `active AND verified AND accepts_outreach AND NOT do_not_contact`
//!   already enforces.

use sqlx::PgPool;
use uuid::Uuid;

use super::{OperatorActionRecord, record_operator_action};

/// What one press of Import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BeaconImportSummary {
    /// Rows added to the roster.
    pub imported: u64,
    /// Researched rows that were already on the roster.
    pub already_present: u64,
    /// Researched rows with no usable contact route, or a kind the roster has
    /// no category for.
    pub skipped_no_route: u64,
    /// Rows a source or an operator marked do-not-contact.
    pub skipped_do_not_contact: u64,
}

impl BeaconImportSummary {
    /// Everything the import looked at, whatever it decided.
    #[must_use]
    pub fn considered(&self) -> u64 {
        self.imported + self.already_present + self.skipped_no_route + self.skipped_do_not_contact
    }
}

/// Map a research vocabulary onto the roster's `beacon_kind`.
///
/// The two vocabularies were built for different jobs and do not line up.
/// Research asks what kind of route this is (`press`, `playlist`,
/// `media_patronage`); the roster asks what someone does for a release
/// (`local_press`, `creator`, `patron`). A playlist curator and a video
/// channel are both `creator` to the roster, because the roster only cares
/// that they put a record in front of an audience the band does not own.
///
/// An unrecognised kind returns `None` and the row stays in research rather
/// than being filed under a guess.
fn beacon_kind_for(target_kind: &str) -> Option<&'static str> {
    match target_kind {
        "radio" => Some("radio"),
        "press" => Some("local_press"),
        "creator" | "playlist" => Some("creator"),
        "endorsement" => Some("reviewer"),
        "media_patronage" => Some("patron"),
        "support_slot" => Some("promoter"),
        _ => None,
    }
}

/// A researched contact, flattened from whichever pool it came from.
struct ResearchedContact {
    source_table: &'static str,
    source_id: Uuid,
    target_kind: String,
    display_name: String,
    contact_email: Option<String>,
    destination_url: Option<String>,
    source_url: Option<String>,
    do_not_contact: bool,
}

/// Imports every researched contact that is not already a beacon.
///
/// Runs as one transaction with the operator-action audit row, so a partial
/// import cannot be recorded as a complete one.
///
/// # Errors
/// Returns the underlying `sqlx::Error` if the transaction cannot be
/// committed; nothing is written in that case.
pub async fn import_researched_beacons(
    pool: &PgPool,
    workspace_id: Uuid,
    idempotency_key: &str,
    request_id: Option<&str>,
) -> Result<BeaconImportSummary, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let agent_rows =
        sqlx::query_as::<_, (Uuid, String, String, Option<String>, Option<String>, bool)>(
            r#"
        SELECT id, target_kind, display_name, contact_email, contact_domain, do_not_contact
        FROM agent_outreach_targets
        WHERE workspace_id = $1 AND status <> 'discarded'
          -- A community is a public space the growth loop posts in, not a
          -- person the roster can write to. It has no address and never
          -- will, so importing one produces a beacon that can only ever sit
          -- there un-contactable. This mattered the moment community targets
          -- started existing: the audience graph carries dozens of them.
          AND target_kind <> 'community'
        ORDER BY created_at
        "#,
        )
        .bind(workspace_id)
        .fetch_all(&mut *tx)
        .await?;

    let candidate_rows = sqlx::query_as::<_, (Uuid, String, String, String, String, String, bool)>(
        r#"
        SELECT id, target_kind, display_name, route_kind, route_value,
               source_reference, route_is_published
        FROM viryaos_outreach_candidates
        WHERE workspace_id = $1 AND status IN ('admitted', 'promoted')
        ORDER BY created_at
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut contacts: Vec<ResearchedContact> =
        Vec::with_capacity(agent_rows.len() + candidate_rows.len());

    for (id, target_kind, display_name, contact_email, contact_domain, do_not_contact) in agent_rows
    {
        contacts.push(ResearchedContact {
            source_table: "agent_outreach_targets",
            source_id: id,
            target_kind,
            display_name,
            contact_email,
            destination_url: contact_domain.map(|domain| format!("https://{domain}")),
            source_url: None,
            do_not_contact,
        });
    }

    for (id, target_kind, display_name, route_kind, route_value, source_reference, published) in
        candidate_rows
    {
        let (contact_email, destination_url) = match route_kind.as_str() {
            "email" => (Some(route_value), None),
            "submission_form" | "handle" => (None, Some(route_value)),
            _ => (None, None),
        };
        contacts.push(ResearchedContact {
            source_table: "viryaos_outreach_candidates",
            source_id: id,
            target_kind,
            display_name,
            contact_email,
            destination_url,
            // The page the route was read out of. Kept so an operator
            // approving this beacon can check the extraction rather than
            // taking the roster's word for it.
            source_url: Some(source_reference),
            // A route somebody worked out rather than read from a published
            // page is stored refused upstream. Importing it as contactable
            // would launder that refusal into a roster row that looks fine.
            do_not_contact: !published,
        });
    }

    // Every source id already on the roster, in one query.
    //
    // This used to be a `SELECT count(*)` per contact inside the loop — 87
    // round trips for the pools production carries, all to answer a question
    // one query answers for every row at once. The set is small (the roster is
    // hundreds, not millions) and it is read inside the same transaction as
    // the inserts, so a concurrent import cannot make it stale: the roster's
    // own uniqueness key still catches anything this set misses.
    let already_imported: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        r#"
        SELECT metadata -> 'imported_from' ->> 'source_id'
        FROM viryaos_beacons
        WHERE workspace_id = $1
          AND metadata -> 'imported_from' ->> 'source_id' IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();

    let mut summary = BeaconImportSummary::default();

    for contact in contacts {
        if contact.do_not_contact {
            summary.skipped_do_not_contact += 1;
            continue;
        }
        let Some(beacon_kind) = beacon_kind_for(&contact.target_kind) else {
            summary.skipped_no_route += 1;
            continue;
        };
        if contact.contact_email.is_none() && contact.destination_url.is_none() {
            summary.skipped_no_route += 1;
            continue;
        }

        // Provenance first: it is what makes a second Import a no-op for rows
        // whose contact_email is NULL, where the roster's own uniqueness key
        // cannot tell two different contacts apart.
        if already_imported.contains(&contact.source_id.to_string()) {
            summary.already_present += 1;
            continue;
        }

        let metadata = serde_json::json!({
            "imported_from": {
                "source_table": contact.source_table,
                "source_id": contact.source_id.to_string(),
                "target_kind": contact.target_kind,
            }
        });

        // The roster has two partial unique indexes — one for email-routed
        // beacons, one for URL-routed beacons — and ON CONFLICT must name the
        // matching index columns *and* its WHERE predicate exactly. A single
        // generic ON CONFLICT cannot match either partial index, so branch on
        // which route the contact carries.
        let inserted = if contact.contact_email.is_some() {
            sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_beacons (
                  id, workspace_id, beacon_kind, display_name, contact_email,
                  destination_url, source_url, active, verified, accepts_outreach,
                  metadata
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,true,false,false,$8)
                ON CONFLICT (workspace_id, beacon_kind, city_id, contact_email)
                  WHERE contact_email IS NOT NULL
                  DO NOTHING
                RETURNING id
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id)
            .bind(beacon_kind)
            .bind(&contact.display_name)
            .bind(contact.contact_email.as_deref())
            .bind(contact.destination_url.as_deref())
            .bind(contact.source_url.as_deref())
            .bind(&metadata)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_beacons (
                  id, workspace_id, beacon_kind, display_name, contact_email,
                  destination_url, source_url, active, verified, accepts_outreach,
                  metadata
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,true,false,false,$8)
                ON CONFLICT (workspace_id, beacon_kind, city_id, destination_url)
                  WHERE contact_email IS NULL AND destination_url IS NOT NULL
                  DO NOTHING
                RETURNING id
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id)
            .bind(beacon_kind)
            .bind(&contact.display_name)
            .bind(contact.contact_email.as_deref())
            .bind(contact.destination_url.as_deref())
            .bind(contact.source_url.as_deref())
            .bind(&metadata)
            .fetch_optional(&mut *tx)
            .await?
        };

        if inserted.is_some() {
            summary.imported += 1;
        } else {
            summary.already_present += 1;
        }
    }

    record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "import_researched_beacons",
            target_type: "beacon_roster",
            target_id: workspace_id,
            idempotency_key,
            request_id,
            details: serde_json::json!({
                "imported": summary.imported,
                "alreadyPresent": summary.already_present,
                "skippedNoRoute": summary.skipped_no_route,
                "skippedDoNotContact": summary.skipped_do_not_contact,
            }),
        },
    )
    .await?;

    tx.commit().await?;
    Ok(summary)
}

// ── SubmitHub CSV import ──────────────────────────────────────────────

/// Map a SubmitHub outlet type onto the roster's `beacon_kind`.
///
/// SubmitHub categorises curators by the platform they operate on; the
/// roster asks what someone does for a release. A Spotify playlister and a
/// YouTube channel both put a record in front of an audience the band does
/// not own, so both are `creator`. A blog is `local_press`. Radio is radio.
fn beacon_kind_for_submithub(outlet_type: &str) -> Option<&'static str> {
    match outlet_type.trim().to_lowercase().as_str() {
        "spotify playlister" | "youtube channel" | "instagram" | "tiktok" => Some("creator"),
        "blog" => Some("local_press"),
        "radio" => Some("radio"),
        _ => None,
    }
}

/// One row from a SubmitHub Activity CSV, flattened and filtered.
///
/// The CSV carries curator names and what they did, but no contact routes.
/// Email addresses and submission URLs are exchanged in the SubmitHub chat
/// after a curator approves — so imported beacons start with neither and
/// the operator enriches them from the chat before approval.
pub struct SubmithubImportInput {
    pub outlet: String,
    pub outlet_type: String,
    pub action: String,
    pub song: String,
    pub country: String,
    pub feedback: String,
    pub action_timestamp: String,
}

/// Import SubmitHub curators as unverified beacons.
///
/// Only curators whose action was `approved` or `shared` are imported —
/// these are warm contacts who engaged positively. Declined curators are
/// real people too, but they have not signalled interest and importing
/// them would launder a soft no into a roster row that looks fine.
///
/// Dedup is provenance-based: the roster's partial unique indexes require
/// either `contact_email` or `destination_url` to be non-null, and
/// SubmitHub imports have neither. So we check
/// `metadata -> 'imported_from' ->> 'source' = 'submithub'` and a
/// `display_name` match before INSERT, the same pattern
/// `import_researched_beacons` uses for contacts with NULL email.
///
/// # Errors
/// Returns the underlying `sqlx::Error` if the transaction cannot be
/// committed; nothing is written in that case.
pub async fn import_submithub_beacons(
    pool: &PgPool,
    workspace_id: Uuid,
    inputs: &[SubmithubImportInput],
    idempotency_key: &str,
    request_id: Option<&str>,
) -> Result<BeaconImportSummary, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Every SubmitHub-sourced beacon already on the roster, by display name.
    let already_imported: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        r#"
        SELECT display_name
        FROM viryaos_beacons
        WHERE workspace_id = $1
          AND metadata -> 'imported_from' ->> 'source' = 'submithub'
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|name| name.to_lowercase())
    .collect();

    let mut summary = BeaconImportSummary::default();

    for input in inputs {
        // Only warm contacts: approved or shared.
        let action = input.action.trim().to_lowercase();
        if action != "approved" && action != "shared" {
            continue;
        }

        let display_name = input.outlet.trim();
        if display_name.is_empty() || display_name.len() > 240 {
            summary.skipped_no_route += 1;
            continue;
        }

        let Some(beacon_kind) = beacon_kind_for_submithub(&input.outlet_type) else {
            summary.skipped_no_route += 1;
            continue;
        };

        // Provenance-based dedup: same outlet name + already from SubmitHub.
        if already_imported.contains(&display_name.to_lowercase()) {
            summary.already_present += 1;
            continue;
        }

        let metadata = serde_json::json!({
            "imported_from": {
                "source": "submithub",
                "outlet_type": input.outlet_type.trim(),
                "action": action,
                "song": input.song.trim(),
                "country": input.country.trim(),
                "feedback": input.feedback.trim(),
                "action_timestamp": input.action_timestamp.trim(),
            }
        });

        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO viryaos_beacons (
              id, workspace_id, beacon_kind, display_name, contact_email,
              destination_url, source_url, active, verified, accepts_outreach,
              metadata
            ) VALUES ($1,$2,$3,$4,NULL,NULL,$5,true,false,false,$6)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(beacon_kind)
        .bind(display_name)
        .bind("https://submithub.com")
        .bind(&metadata)
        .fetch_optional(&mut *tx)
        .await?;

        if inserted.is_some() {
            summary.imported += 1;
        } else {
            summary.already_present += 1;
        }
    }

    record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "import_submithub_beacons",
            target_type: "beacon_roster",
            target_id: workspace_id,
            idempotency_key,
            request_id,
            details: serde_json::json!({
                "imported": summary.imported,
                "alreadyPresent": summary.already_present,
                "skippedNoRoute": summary.skipped_no_route,
                "skippedDoNotContact": summary.skipped_do_not_contact,
            }),
        },
    )
    .await?;

    tx.commit().await?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playlist_curator_and_a_channel_are_both_creators() {
        // The roster asks what someone does for a release, not what kind of
        // page they run. Both put a record in front of an audience the band
        // does not own.
        assert_eq!(beacon_kind_for("playlist"), Some("creator"));
        assert_eq!(beacon_kind_for("creator"), Some("creator"));
    }

    #[test]
    fn press_becomes_local_press_not_reviewer() {
        // `reviewer` is reserved for endorsement — someone who vouches for the
        // band. A press outlet that runs a news item has not endorsed anyone.
        assert_eq!(beacon_kind_for("press"), Some("local_press"));
        assert_eq!(beacon_kind_for("endorsement"), Some("reviewer"));
    }

    #[test]
    fn an_unknown_kind_is_left_in_research_rather_than_guessed() {
        // Filing an unrecognised kind under a default would put a contact on
        // the roster under a category nobody chose, and the roster is what the
        // invite pipeline reads.
        assert_eq!(beacon_kind_for("mystery"), None);
        assert_eq!(beacon_kind_for(""), None);
    }

    #[test]
    fn every_mapped_kind_is_a_kind_the_roster_accepts() {
        // viryaos_beacons.beacon_kind has a CHECK constraint. A mapping to a
        // value outside it compiles, lints and tests clean under runtime SQL,
        // then fails on the first import against a real database.
        const ROSTER_KINDS: [&str; 9] = [
            "radio",
            "local_press",
            "television",
            "reviewer",
            "creator",
            "photographer",
            "promoter",
            "patron",
            "community",
        ];
        for research_kind in [
            "radio",
            "press",
            "creator",
            "playlist",
            "endorsement",
            "media_patronage",
            "support_slot",
        ] {
            let mapped = beacon_kind_for(research_kind)
                .unwrap_or_else(|| panic!("{research_kind} should map to a roster kind"));
            assert!(
                ROSTER_KINDS.contains(&mapped),
                "{research_kind} maps to {mapped}, which viryaos_beacons.beacon_kind rejects",
            );
        }
    }

    #[test]
    fn the_summary_accounts_for_every_row_it_looked_at() {
        let summary = BeaconImportSummary {
            imported: 12,
            already_present: 5,
            skipped_no_route: 9,
            skipped_do_not_contact: 2,
        };
        assert_eq!(summary.considered(), 28);
    }

    #[test]
    fn submithub_playlister_and_youtube_are_both_creators() {
        assert_eq!(
            beacon_kind_for_submithub("Spotify Playlister"),
            Some("creator")
        );
        assert_eq!(
            beacon_kind_for_submithub("YouTube Channel"),
            Some("creator")
        );
        assert_eq!(beacon_kind_for_submithub("Instagram"), Some("creator"));
        assert_eq!(beacon_kind_for_submithub("TikTok"), Some("creator"));
    }

    #[test]
    fn submithub_blog_is_local_press() {
        assert_eq!(beacon_kind_for_submithub("Blog"), Some("local_press"));
    }

    #[test]
    fn submithub_radio_is_radio() {
        assert_eq!(beacon_kind_for_submithub("Radio"), Some("radio"));
    }

    #[test]
    fn submithub_unknown_outlet_type_is_skipped() {
        assert_eq!(beacon_kind_for_submithub("Podcast"), None);
        assert_eq!(beacon_kind_for_submithub(""), None);
    }

    #[test]
    fn submithub_mapping_is_case_insensitive() {
        assert_eq!(
            beacon_kind_for_submithub("spotify playlister"),
            Some("creator")
        );
        assert_eq!(beacon_kind_for_submithub("BLOG"), Some("local_press"));
    }

    #[test]
    fn every_submithub_mapped_kind_is_a_kind_the_roster_accepts() {
        const ROSTER_KINDS: [&str; 9] = [
            "radio",
            "local_press",
            "television",
            "reviewer",
            "creator",
            "photographer",
            "promoter",
            "patron",
            "community",
        ];
        for outlet_type in [
            "Spotify Playlister",
            "YouTube Channel",
            "Instagram",
            "TikTok",
            "Blog",
            "Radio",
        ] {
            let mapped = beacon_kind_for_submithub(outlet_type)
                .unwrap_or_else(|| panic!("{outlet_type} should map to a roster kind"));
            assert!(
                ROSTER_KINDS.contains(&mapped),
                "{outlet_type} maps to {mapped}, which viryaos_beacons.beacon_kind rejects",
            );
        }
    }
}
