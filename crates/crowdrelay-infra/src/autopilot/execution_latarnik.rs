// Sending one invitation (P.1).
//
// Included into `autopilot.rs` beside the beacon arm. The shape is the gig
// letter's, narrowed to one recipient: the letter was composed and approved
// upstream, so this re-pins the row, reserves the contact window and emits
// what was approved — it writes no words of its own.
//
// Every gate runs again here rather than being trusted from the approval. A
// beacon can be marked do-not-contact, edited, or deactivated between the click
// and the claim, and each of those is a reason this send stops.

// The dispatch arms in this crate take their payload apart at the call site, so
// the arity is the payload's rather than a design choice here.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_latarnik_invite(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    beacon_id: crowdrelay_domain::BeaconId,
    beacon_version: i64,
    recipient_email: &str,
    recipient_name: &str,
    reason: &str,
    draft: &crowdrelay_domain::latarnik_invite::Invite,
) -> Result<(), RepositoryError> {
    // The letter travels in the payload (O.1). A row queued without one is
    // refused rather than handed to an executor that would write its own —
    // this letter's whole value is that a person read it first.
    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
        return Err(RepositoryError::ConflictBecause(
            "latarnik invite refused: the action carries no letter, and nothing may write one \
             on the band's behalf now",
        ));
    }

    // Re-pinned under the version the approval read. A beacon edited since then
    // is a different record — possibly a different person at the same
    // organisation — and the send stops rather than guessing.
    let pinned = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT beacon.display_name, beacon.contact_email
        FROM beacons AS beacon
        WHERE beacon.workspace_id = $1
          AND beacon.id = $2
          AND beacon.version = $3
          AND beacon.active
          AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact
          AND beacon.contact_email IS NOT NULL
        FOR SHARE OF beacon
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(beacon_id.into_uuid())
    .bind(beacon_version)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    // The address is taken from the row, never from the payload. A payload that
    // disagrees with the record is the one case where trusting it would write
    // to somebody the operator never approved.
    if pinned.1.trim().to_lowercase() != recipient_email.trim().to_lowercase() {
        return Err(RepositoryError::Conflict);
    }

    // `latarnik_invite` is its own context so the once-ever rule can read it
    // back: the governor row remembers what the last contact was for, and the
    // eligibility read refuses anybody whose last context was this one.
    let now = now_of(transaction).await?;
    reserve_contact_window(
        transaction,
        workspace_id,
        action_id,
        "latarnik_invite",
        &pinned.1,
        now,
    )
    .await?;

    emit_outward_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.latarnik.invite_requested",
        format!("latarnik-invite:{beacon_id}"),
        "somebody the band already works with, asked once whether they also want the dates — \
         relationship on record, contact window reserved",
        json!({
            "action_id": action_id,
            "beacon_id": beacon_id,
            "recipient_name": recipient_name,
            "recipients": [{ "contact_email": pinned.1, "name": pinned.0 }],
            "reason": reason,
            "draft": draft,
        }),
    )
    .await?;
    Ok(())
}

/// The transaction's own clock.
///
/// Taken from the database rather than the process so the reservation window
/// and the row it guards are stamped by one clock. A worker whose host drifts
/// would otherwise reserve a window that starts in the past.
async fn now_of(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<OffsetDateTime, RepositoryError> {
    sqlx::query_scalar::<_, OffsetDateTime>("SELECT now()")
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)
}
