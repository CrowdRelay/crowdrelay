//! Execution boundary for a band-approved gig outreach (4G.4).
//!
//! # All of them, or none of them
//!
//! A gig proposal names everybody who books one room. §12-6 makes that one
//! action with a recipient set rather than one action each, and this is where
//! that matters: every recipient is locked and reserved with the contact
//! governor before anything is emitted, and all of it is the caller's
//! transaction. A promoter the governor refuses — a do-not-contact, a cooldown,
//! a sibling act in the same organisation holding the window — takes the whole
//! letter down with them.
//!
//! The alternative was measured in the design and rejected: three promoters who
//! book the same city talk to each other, and writing to two of them about one
//! night reads as a snub or a shambles depending on who compares notes. Nobody
//! hearing is recoverable in a sentence. Two of three hearing is not.
//!
//! Nothing the approval wrote is trusted past the moment it was checked: the
//! row lock re-pins each target's version, so a promoter edited between the
//! approval and the send fails rather than being written to under stale terms.

use serde_json::json;

use super::*;
use crate::autopilot::{
    emit_external_action, lock_booking_target_for_execution, reserve_contact_window,
};
use crowdrelay_application::autopilot::{AutopilotActionPayload, ClaimedAutopilotAction};

/// Writes the letter, or writes none of it.
///
/// Takes the claimed action rather than its fields: the dispatcher's `match` is one
/// read of every payload in the system and grows by one line per kind, while
/// the module that writes the letter is the one that should know its shape.
/// Any other payload is `Conflict` — a dispatcher that routed the wrong kind
/// here is a bug, and a bug must not become a letter.
pub(in crate::autopilot) async fn execute_gig_outreach(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let action_id = action.id;
    let AutopilotActionPayload::RequestGigOutreach {
        city_id,
        venue,
        recipients,
        opening_line,
        reasons,
    } = &action.payload
    else {
        return Err(RepositoryError::Conflict);
    };
    let city_id = *city_id;
    let mut addressed = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        let target = lock_booking_target_for_execution(
            transaction,
            workspace_id,
            city_id,
            recipient.target_id,
            recipient.target_version,
        )
        .await?;
        reserve_contact_window(
            transaction,
            workspace_id,
            action_id,
            "gig_outreach",
            &target.2,
            now,
        )
        .await?;
        addressed.push(json!({
            "target_id": recipient.target_id,
            "target_kind": target.0,
            "target_name": target.1,
            "contact_email": target.2,
        }));
    }

    emit_external_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.gig.outreach_requested",
        json!({
            "action_id": action_id,
            "city_id": city_id,
            "venue": venue,
            "template_key": "gig.proposal.v1",
            "opening_line": opening_line,
            "reasons": reasons,
            "recipients": addressed,
        }),
    )
    .await?;

    for recipient in recipients {
        let changed = sqlx::query(
            r#"
            UPDATE viryaos_booking_targets
            SET last_outreach_at = $4
            WHERE workspace_id = $1 AND id = $2 AND version = $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(recipient.target_id.into_uuid())
        .bind(recipient.target_version)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        if changed.rows_affected() != 1 {
            return Err(RepositoryError::Conflict);
        }
        // One interaction per promoter, not one per letter: the reply that
        // matters later is from a person, and 4G.5 asks which kind of evidence
        // predicts a booking — which it cannot answer if three promoters'
        // silence arrives as one row.
        sqlx::query(
            r#"
            INSERT INTO viryaos_booking_interactions(
                workspace_id,target_id,direction,phase,source_key,occurred_at,metadata
            ) VALUES($1,$2,'outbound','initial',$3,$4,
                     jsonb_build_object('action_id',$5::uuid,'gig_outreach',true))
            ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(recipient.target_id.into_uuid())
        .bind(format!("autopilot:{action_id}"))
        .bind(now)
        .bind(action_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}
