//! The autopilot half of an archive promote wave — re-reads the segment at
//! run time and drives the shared `promote_fan_wave_in_tx` inside the action
//! transaction. The approval authorized a bounded wave of whatever the
//! evidence head holds *now*, not the rows a days-old card was raised on;
//! and because both launch paths call the same mechanism, suppression,
//! cooldown, city carry and the one-commit mark cannot drift apart.

use super::*;
use sqlx::{PgPool, Postgres, Transaction};

pub(in crate::autopilot) async fn run_archive_promote_wave(
    transaction: &mut Transaction<'_, Postgres>,
    pool: &PgPool,
    workspace_id: WorkspaceId,
    limit: i64,
    reason: Option<String>,
) -> Result<(), RepositoryError> {
    let repo = crate::gdrive::PostgresGDriveRepository::new(pool.clone());
    let contacts = repo
        .staged_fan_contacts_in_segment(
            workspace_id.into_uuid(),
            crate::gdrive::ContactSegment::LikelyFan,
            Some(limit),
        )
        .await
        .map_err(|error| match error {
            crate::gdrive::GDriveError::Database(inner) => map_sqlx(inner),
            _ => RepositoryError::Unexpected,
        })?;
    // A backlog drained between raise and approval leaves nothing to send —
    // an outcome, not an error.
    if contacts.is_empty() {
        return Ok(());
    }
    let invitation = crate::fan_import::InvitationContext {
        locale: crate::tenant_settings::TenantSettingsRepository::new(pool.clone())
            .crew_locale_if_set(workspace_id.into_uuid())
            .await
            .map_err(map_sqlx)?,
        reason,
    };
    crate::gdrive::promote_fan_wave_in_tx(
        transaction,
        &repo,
        workspace_id.into_uuid(),
        &contacts,
        &invitation,
    )
    .await
    .map_err(|error| match error {
        crate::gdrive::PromoteWaveError::GDrive(crate::gdrive::GDriveError::Database(inner))
        | crate::gdrive::PromoteWaveError::Import(crate::fan_import::FanImportError::Database(
            inner,
        )) => map_sqlx(inner),
        _ => RepositoryError::Unexpected,
    })?;
    Ok(())
}
