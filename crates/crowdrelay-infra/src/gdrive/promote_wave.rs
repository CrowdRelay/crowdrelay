//! The bounded archive→fan promote wave, shared by the operator endpoint
//! and the autopilot executor. Whoever approved the wave the semantics are
//! the same: the staged contacts the caller read become pending fans under
//! the double opt-in and their staging rows are marked `promoted` inside
//! the caller's transaction — a wave lands whole or not at all.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{GDriveError, PostgresGDriveRepository, StagedFanContact};
use crate::fan_import::{
    FanImportError, ImportCounts, ImportEntry, InvitationContext, PostgresFanImportRepository,
};

#[derive(Debug, thiserror::Error)]
pub enum PromoteWaveError {
    #[error("gdrive repository operation failed")]
    GDrive(#[from] GDriveError),
    #[error("fan import operation failed")]
    Import(#[from] FanImportError),
}

/// What one wave did — the import counters plus the number of staging rows
/// actually marked `promoted`. Suppressed rows stay staged, so `promoted`
/// can be lower than the contacts read.
#[derive(Debug)]
pub struct PromotedFanWave {
    pub counts: ImportCounts,
    pub promoted: u64,
}

/// Confirmation-link TTL for archive waves: thirty days, because a list
/// import waits for a click, not for the minute the self-service flow gets.
pub const ARCHIVE_ACCESS_TOKEN_TTL_DAYS: i64 = 30;
/// Seconds before the same address may be mailed again inside a wave.
pub const ARCHIVE_RESEND_COOLDOWN_SECONDS: i64 = 300;

/// Imports `contacts` as pending fans and marks their staging rows, all
/// inside `tx`. The caller read the slice (bounded, evidence-ranked) and
/// resolved the invitation voice (`locale`/`reason` in `invitation`) — this
/// function invents neither; it is the mechanism both the endpoint and the
/// autopilot executor funnel through so the safeguards cannot diverge.
/// Token TTL and resend cooldown are the module constants above, identical
/// for every caller.
pub async fn promote_fan_wave_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    repo: &PostgresGDriveRepository,
    workspace_id: Uuid,
    contacts: &[StagedFanContact],
    invitation: &InvitationContext,
) -> Result<PromotedFanWave, PromoteWaveError> {
    // The sheet cities become fan city interests — resolved in one round
    // trip under the same uniqueness rule the beacon promote applies (an
    // ambiguous name resolves to nothing, never a guess).
    let city_texts: Vec<String> = contacts
        .iter()
        .filter_map(|contact| contact.city.clone())
        .collect();
    let city_ids = repo.staged_city_ids(&city_texts).await?;

    // `import_batch` takes one source per batch; a contact sighted in Drive
    // and Gmail imports as "gdrive+gmail", so the wave is grouped on the
    // joined source string.
    let mut groups: std::collections::BTreeMap<String, Vec<&StagedFanContact>> =
        std::collections::BTreeMap::new();
    for contact in contacts {
        groups
            .entry(contact.sources.join("+"))
            .or_default()
            .push(contact);
    }
    let import = PostgresFanImportRepository::new(repo.pool().clone());
    let mut counts = ImportCounts::default();
    let mut suppressed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (source, group) in &groups {
        let entries: Vec<ImportEntry> = group
            .iter()
            .map(|contact| ImportEntry {
                email: contact.normalized_email.clone(),
                display_name: contact.display_name.clone(),
                locale: None,
                city_id: contact
                    .city
                    .as_deref()
                    .and_then(|city| city_ids.get(&city.trim().to_lowercase()))
                    .copied(),
            })
            .collect();
        let outcome = import
            .import_batch_in_tx(
                tx,
                workspace_id,
                source,
                &entries,
                ARCHIVE_ACCESS_TOKEN_TTL_DAYS,
                ARCHIVE_RESEND_COOLDOWN_SECONDS,
                invitation,
            )
            .await?;
        counts.imported_pending += outcome.counts.imported_pending;
        counts.confirmation_resent += outcome.counts.confirmation_resent;
        counts.already_active += outcome.counts.already_active;
        counts.skipped_suppressed += outcome.counts.skipped_suppressed;
        counts.cooldown_skipped += outcome.counts.cooldown_skipped;
        suppressed.extend(outcome.suppressed_emails);
    }

    // A suppressed address is not promoted, whatever was read — same rule
    // the single promote applies.
    let promotable: Vec<Uuid> = contacts
        .iter()
        .filter(|contact| !suppressed.contains(contact.normalized_email.as_str()))
        .map(|contact| contact.id)
        .collect();
    let promoted = repo
        .mark_fans_promoted_by_ids(tx, workspace_id, &promotable)
        .await?;
    Ok(PromotedFanWave { counts, promoted })
}
