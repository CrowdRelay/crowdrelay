//! CRM contact import: screen the agent's proposed outreach targets, then
//! promote the ones an operator approves.
//!
//! The registry sweep lands `agent_outreach_targets` rows faster than an
//! operator can confirm them one at a time. This is the gate between that
//! pile and `outreach_targets`: the list route shows each row with the
//! verdict a deterministic screen gives it, and the approve route re-runs
//! the same screen inside its transaction so the stored answer — not the
//! page the operator read minutes ago — decides.
//!
//! Nothing here contacts anybody. Approval is the consent point: it only
//! makes an address eligible for the bounded outreach lane, whose own caps
//! and the contact governor still apply downstream.

use super::*;

use async_trait::async_trait;
use crowdrelay_application::autopilot::{
    AutopilotOutreachImportRepository, OutreachImportApproval, OutreachImportProposal,
    OutreachImportProposalPage, OutreachImportSelection,
};
use sqlx::Postgres;
use std::collections::{BTreeMap, HashSet};

/// The kinds a proposal may carry into `outreach_targets`. `community` is
/// deliberately absent: those rows feed the community-engager lane, not the
/// address book.
const IMPORTABLE_TARGET_KINDS: &[&str] = &[
    "press",
    "radio",
    "creator",
    "endorsement",
    "media_patronage",
    "playlist",
    "organiser",
];

/// One approval call — named ids or all-admitted of a kind — may promote at
/// most this many rows, so a mis-scoped approve cannot import the whole
/// registry.
const MAX_IMPORT_APPROVALS: usize = 200;
/// Page bound for the proposal list; mirrors the candidates list's clamp.
const MAX_IMPORT_PAGE: u32 = 500;

/// Local parts that are never a person worth mailing.
const ROLE_LOCALS: &[&str] = &["noreply", "no-reply", "postmaster"];

/// One proposed row plus the suppression facts the screen needs, so the
/// verdict is computed in Rust and stays unit-testable rather than hiding in
/// a CASE expression.
#[derive(Debug, FromRow)]
struct ImportProposalRow {
    id: Uuid,
    target_kind: String,
    display_name: String,
    contact_email: Option<String>,
    contact_domain: Option<String>,
    why_fit: String,
    row_do_not_contact: bool,
    already_target: bool,
    target_suppressed: bool,
    governor_suppressed: bool,
}

/// The email shape `outreach_targets.contact_email`'s CHECK enforces:
/// `^[^[:space:]@]+@[^[:space:]@]+\.[^[:space:]@]+$` under 320 characters.
/// Screened here rather than left to the CHECK so a malformed address gets a
/// verdict instead of aborting the batch's transaction.
fn valid_import_email(email: &str) -> bool {
    if email.is_empty() || email.chars().count() > 320 {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    if local.is_empty() || local.chars().any(char::is_whitespace) {
        return false;
    }
    if domain.is_empty() || domain.chars().any(|c| c.is_whitespace() || c == '@') {
        return false;
    }
    match domain.rsplit_once('.') {
        Some((before, after)) => !before.is_empty() && !after.is_empty(),
        None => false,
    }
}

/// The screen's verdict for one row. Order matters: an address that cannot
/// parse is invalid before it is anything else; a mailbox nobody reads is a
/// role address; suppression beats already-target because "do not contact"
/// is the answer the operator needs even when a row also exists; and only an
/// admitted email occupies its batch slot, so two rows for the same refused
/// address each keep their own reason.
fn import_verdict(row: &ImportProposalRow, seen: &mut HashSet<String>) -> &'static str {
    let email = row.contact_email.as_deref().unwrap_or("").trim();
    if !valid_import_email(email) {
        return "refuse:invalid_email";
    }
    let lowered = email.to_lowercase();
    let local = lowered.split('@').next().unwrap_or_default();
    if ROLE_LOCALS.contains(&local) {
        return "refuse:role_address";
    }
    if row.row_do_not_contact || row.target_suppressed || row.governor_suppressed {
        return "refuse:do_not_contact";
    }
    if row.already_target {
        return "refuse:already_target";
    }
    if !seen.insert(lowered) {
        return "refuse:duplicate_in_batch";
    }
    "admit"
}

/// The proposals the screen judges: proposed rows of an importable kind with
/// a non-blank address, each carrying the existence and suppression facts
/// the verdicts read. Workspace-scoped, ordered stably so the batch's first
/// row for an address is the same row on every read.
async fn fetch_import_proposals<'e, E>(
    executor: E,
    workspace_id: WorkspaceId,
    ids: Option<&[Uuid]>,
    target_kind: Option<&str>,
) -> Result<Vec<ImportProposalRow>, RepositoryError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, ImportProposalRow>(
        r#"
        SELECT t.id, t.target_kind, t.display_name, t.contact_email,
               t.contact_domain, t.why_fit,
               t.do_not_contact AS row_do_not_contact,
               EXISTS (
                   SELECT 1 FROM outreach_targets AS target
                   WHERE target.workspace_id = t.workspace_id
                     AND lower(target.contact_email) = lower(t.contact_email)
               ) AS already_target,
               EXISTS (
                   SELECT 1 FROM outreach_targets AS target
                   WHERE target.workspace_id = t.workspace_id
                     AND lower(target.contact_email) = lower(t.contact_email)
                     AND target.do_not_contact
               ) AS target_suppressed,
               EXISTS (
                   SELECT 1 FROM contact_governor AS governor
                   WHERE governor.workspace_id = t.workspace_id
                     AND governor.normalized_contact = lower(t.contact_email)
                     AND governor.do_not_contact
               ) AS governor_suppressed
        FROM agent_outreach_targets AS t
        WHERE t.workspace_id = $1
          AND t.status = 'proposed'
          AND t.target_kind = ANY($2)
          AND t.contact_email IS NOT NULL
          AND btrim(t.contact_email) <> ''
          AND ($3::uuid[] IS NULL OR t.id = ANY($3))
          AND ($4::text IS NULL OR t.target_kind = $4)
        ORDER BY t.created_at, t.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(IMPORTABLE_TARGET_KINDS)
    .bind(ids)
    .bind(target_kind)
    .fetch_all(executor)
    .await
    .map_err(map_sqlx)
}

fn proposal_view(row: &ImportProposalRow, verdict: &str) -> OutreachImportProposal {
    OutreachImportProposal {
        id: row.id,
        target_kind: row.target_kind.clone(),
        display_name: row.display_name.clone(),
        contact_email: row.contact_email.clone(),
        contact_domain: row.contact_domain.clone(),
        why_fit: row.why_fit.clone(),
        verdict: verdict.to_string(),
    }
}

#[async_trait]
impl AutopilotOutreachImportRepository for PostgresAutopilotRepository {
    async fn list_outreach_import_proposals(
        &self,
        workspace_id: WorkspaceId,
        target_kind: Option<String>,
        limit: u32,
    ) -> Result<OutreachImportProposalPage, RepositoryError> {
        self.bounded(async {
            // A pure read: no transaction, no audit row.
            let rows =
                fetch_import_proposals(&self.pool, workspace_id, None, target_kind.as_deref())
                    .await?;

            let mut seen = HashSet::new();
            let mut verdict_counts: BTreeMap<String, u32> = BTreeMap::new();
            let mut proposals = Vec::new();
            let limit = limit.clamp(1, MAX_IMPORT_PAGE) as usize;
            for row in &rows {
                let verdict = import_verdict(row, &mut seen);
                *verdict_counts.entry(verdict.to_string()).or_insert(0) += 1;
                if proposals.len() < limit {
                    proposals.push(proposal_view(row, verdict));
                }
            }
            Ok(OutreachImportProposalPage {
                proposals,
                verdict_counts,
            })
        })
        .await
    }

    async fn approve_outreach_import_proposals(
        &self,
        workspace_id: WorkspaceId,
        selection: OutreachImportSelection,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachImportApproval, RepositoryError> {
        self.bounded(async {
            let (ids, kind): (Option<Vec<Uuid>>, Option<String>) = match &selection {
                OutreachImportSelection::Ids(ids) => {
                    if ids.is_empty() || ids.len() > MAX_IMPORT_APPROVALS {
                        return Err(RepositoryError::Unexpected);
                    }
                    (Some(ids.clone()), None)
                }
                OutreachImportSelection::AllAdmitted { target_kind } => {
                    if !IMPORTABLE_TARGET_KINDS.contains(&target_kind.as_str()) {
                        return Err(RepositoryError::Unexpected);
                    }
                    (None, Some(target_kind.clone()))
                }
            };

            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            // The batch has no subject of its own, so like the sweep ingestion
            // the workspace stands in. Details carry the selection verbatim —
            // sorted so a replay with the same ids in another order reads as
            // the same operation rather than a conflicting one.
            let details = match &selection {
                OutreachImportSelection::Ids(ids) => {
                    let mut sorted = ids.clone();
                    sorted.sort();
                    json!({ "selection": "ids", "ids": sorted })
                }
                OutreachImportSelection::AllAdmitted { target_kind } => {
                    json!({ "selection": "all_admitted", "target_kind": target_kind })
                }
            };
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "approve_outreach_import_proposals",
                "outreach_import_batch",
                workspace_id.into_uuid(),
                "admin_api_key",
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(OutreachImportApproval {
                    operation_id: existing,
                    admitted: 0,
                    refused_by_reason: BTreeMap::new(),
                    created_target_ids: Vec::new(),
                    replayed: true,
                });
            }

            let rows = fetch_import_proposals(
                &mut *transaction,
                workspace_id,
                ids.as_deref(),
                kind.as_deref(),
            )
            .await?;

            let mut seen = HashSet::new();
            let mut refused_by_reason: BTreeMap<String, u32> = BTreeMap::new();
            let mut admitted: Vec<&ImportProposalRow> = Vec::new();
            for row in &rows {
                let verdict = import_verdict(row, &mut seen);
                if verdict == "admit" && admitted.len() < MAX_IMPORT_APPROVALS {
                    admitted.push(row);
                } else if verdict != "admit" {
                    *refused_by_reason.entry(verdict.to_string()).or_insert(0) += 1;
                } else {
                    // Admitted past the per-call cap: neither promoted nor
                    // refused — it simply waits for the next call.
                }
            }

            // An address the band already holds stays the row it already is:
            // the conflict rule never resets an existing relationship, the
            // same rule the candidate-promotion path keeps.
            let mut created_target_ids = Vec::with_capacity(admitted.len());
            for row in &admitted {
                let email = row
                    .contact_email
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase();
                let created = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    INSERT INTO outreach_targets (
                        workspace_id, target_kind, display_name, contact_email,
                        active, verified, accepts_outreach, accepts_outreach_basis,
                        priority
                    ) VALUES ($1,$2,$3,$4,true,false,true,
                              'operator registry import',50)
                    ON CONFLICT (workspace_id, contact_email) DO NOTHING
                    RETURNING id
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(&row.target_kind)
                .bind(row.display_name.trim())
                .bind(&email)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if let Some(id) = created {
                    created_target_ids.push(id);
                }
            }

            let promoted_ids: Vec<Uuid> = admitted.iter().map(|row| row.id).collect();
            sqlx::query(
                r#"
                UPDATE agent_outreach_targets
                SET status = 'promoted',
                    screened_at = now(),
                    screening_verdict = 'admitted'
                WHERE workspace_id = $1
                  AND id = ANY($2)
                  AND status = 'proposed'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&promoted_ids)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(OutreachImportApproval {
                operation_id,
                admitted: u32::try_from(admitted.len()).unwrap_or(u32::MAX),
                refused_by_reason,
                created_target_ids,
                replayed: false,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(email: Option<&str>) -> ImportProposalRow {
        ImportProposalRow {
            id: Uuid::now_v7(),
            target_kind: "press".to_string(),
            display_name: "Zine".to_string(),
            contact_email: email.map(str::to_string),
            contact_domain: None,
            why_fit: String::new(),
            row_do_not_contact: false,
            already_target: false,
            target_suppressed: false,
            governor_suppressed: false,
        }
    }

    #[test]
    fn a_clean_address_is_admitted() {
        let mut seen = HashSet::new();
        assert_eq!(
            import_verdict(&row(Some("editor@zine.example")), &mut seen),
            "admit"
        );
    }

    #[test]
    fn an_unparseable_address_is_invalid() {
        let mut seen = HashSet::new();
        for bad in [
            "not-an-address",
            "@zine.example",
            "editor@",
            "editor@zine",
            "editor@zine.",
            "editor@.example",
            "edi tor@zine.example",
            "editor@zi ne.example",
            &"x".repeat(400),
        ] {
            assert_eq!(
                import_verdict(&row(Some(bad)), &mut seen),
                "refuse:invalid_email",
                "{bad}"
            );
        }
    }

    #[test]
    fn role_mailboxes_are_refused() {
        let mut seen = HashSet::new();
        for local in ["noreply", "no-reply", "postmaster", "NOREPLY"] {
            let email = format!("{local}@zine.example");
            assert_eq!(
                import_verdict(&row(Some(&email)), &mut seen),
                "refuse:role_address",
                "{email}"
            );
        }
    }

    #[test]
    fn suppression_comes_from_three_places() {
        for suppressed in [
            ImportProposalRow {
                row_do_not_contact: true,
                ..row(Some("a@zine.example"))
            },
            ImportProposalRow {
                target_suppressed: true,
                ..row(Some("a@zine.example"))
            },
            ImportProposalRow {
                governor_suppressed: true,
                ..row(Some("a@zine.example"))
            },
        ] {
            let mut seen = HashSet::new();
            assert_eq!(
                import_verdict(&suppressed, &mut seen),
                "refuse:do_not_contact"
            );
        }
    }

    #[test]
    fn an_existing_target_is_not_reimported() {
        let mut seen = HashSet::new();
        let mut existing = row(Some("a@zine.example"));
        existing.already_target = true;
        assert_eq!(
            import_verdict(&existing, &mut seen),
            "refuse:already_target"
        );
    }

    #[test]
    fn the_second_row_for_an_admitted_address_is_the_duplicate() {
        let mut seen = HashSet::new();
        assert_eq!(
            import_verdict(&row(Some("a@zine.example")), &mut seen),
            "admit"
        );
        assert_eq!(
            import_verdict(&row(Some("A@zine.example")), &mut seen),
            "refuse:duplicate_in_batch"
        );
        // A row refused for its own reason does not occupy the slot, so a
        // later identical address keeps its own verdict instead of reading
        // as the duplicate of a refusal.
        let mut suppressed = row(Some("b@zine.example"));
        suppressed.target_suppressed = true;
        assert_eq!(
            import_verdict(&suppressed, &mut seen),
            "refuse:do_not_contact"
        );
        assert_eq!(
            import_verdict(&suppressed, &mut seen),
            "refuse:do_not_contact"
        );
    }
}
