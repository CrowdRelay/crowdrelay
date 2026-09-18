//! The label's crossbill (5.15, §4h-7.4): §4i-4 across acts.
//!
//! Within a band, a fan who joined in month twenty never heard month
//! three. Across a label the same is true between acts — a fan of act A
//! has never heard act B's back catalogue, and the label holds both
//! rights. The grant is its own consent purpose (`catalogue_rotation`)
//! with its own monthly cap and cooldown, because "mail my fans the
//! labelmate's back catalogue" is a different ask than a release feature.
//!
//! This module is the deterministic half: which release a rotation
//! features next, and which consent edges currently have headroom to run
//! one. The pick walks the beneficiary's released catalogue oldest-first —
//! month three before month twenty — and skips releases the edge has
//! already rotated, so successive campaigns progress through the catalogue
//! rather than repeating the newest item. An edge whose catalogue is
//! exhausted is reported as such, not silently skipped: the label should
//! see the rotation finished.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::WorkspaceId;

/// One release the rotation can feature — a beneficiary's catalogue item
/// as the read assembled it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogueItem {
    pub release_id: Uuid,
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub release_at: OffsetDateTime,
    /// Whether the fan-facing message can carry a listen link. A release
    /// without one is still catalogue, but the rotation prefers items the
    /// fan can actually play.
    pub listen_url: Option<String>,
}

/// Which catalogue item an edge rotates next — oldest released first,
/// already-rotated items skipped. `None` means the catalogue is exhausted,
/// not that picking failed.
#[must_use]
pub fn pick_next_release(
    catalogue: &[CatalogueItem],
    already_rotated: &std::collections::BTreeSet<Uuid>,
    now: OffsetDateTime,
) -> Option<CatalogueItem> {
    catalogue
        .iter()
        .filter(|item| item.release_at <= now)
        .filter(|item| !already_rotated.contains(&item.release_id))
        .min_by_key(|item| (item.release_at, item.release_id))
        .cloned()
}

/// One consent edge's rotation proposal — the pair, the item, and the
/// headroom the cap leaves. Composed only for edges that can act.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogueRotationProposal {
    pub consent_id: Uuid,
    pub from_workspace_id: WorkspaceId,
    pub from_act: String,
    pub to_workspace_id: WorkspaceId,
    pub to_act: String,
    /// The release this rotation would feature.
    pub release: CatalogueItem,
    /// Active fans on the owner's side the campaign could reach — measured
    /// before the per-fan cooldown filter, so the label sees the ceiling.
    pub reachable_fans: u32,
    /// The edge's monthly cap and how much of it is already spent — the
    /// consent budget the rotation lands inside.
    pub max_campaigns_per_month: u16,
    pub campaigns_this_month: u16,
    /// The campaign reference the run would record — the labelling:
    /// `catalogue:<release_id>` marks the delivery as catalogue, not promo.
    pub campaign_reference: String,
    /// Why this edge proposes now, in one sentence.
    pub reason: String,
}

/// An edge whose catalogue is exhausted — every released item already
/// rotated. Reported, not skipped: the answer "nothing left to rotate"
/// is information.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExhaustedEdge {
    pub consent_id: Uuid,
    pub from_act: String,
    pub to_act: String,
    pub rotated_count: u32,
}

/// The roster's rotation plan: the proposals with headroom, and the edges
/// that have finished their catalogue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogueRotationPlan {
    pub organization_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub proposals: Vec<CatalogueRotationProposal>,
    pub exhausted: Vec<ExhaustedEdge>,
}

/// One edge's facts as the read assembled them — the input `compose`
/// decides over.
#[derive(Clone, Debug)]
pub struct RotationEdge {
    pub consent_id: Uuid,
    pub from_workspace_id: WorkspaceId,
    pub from_act: String,
    pub to_workspace_id: WorkspaceId,
    pub to_act: String,
    pub max_campaigns_per_month: u16,
    pub campaigns_this_month: u16,
    pub reachable_fans: u32,
    pub catalogue: Vec<CatalogueItem>,
    pub already_rotated: std::collections::BTreeSet<Uuid>,
}

/// The campaign reference a rotation records under — the labelling the
/// done-when names: catalogue, not promotion, and the release it carried.
#[must_use]
pub fn campaign_reference(release_id: Uuid) -> String {
    format!("catalogue:{release_id}")
}

/// Composes the rotation plan from assembled edges. An edge with no cap
/// headroom proposes nothing — the cap outvotes ambition — and an edge
/// with headroom but nothing left to rotate reports as exhausted.
#[must_use]
pub fn compose(
    organization_id: Uuid,
    generated_at: OffsetDateTime,
    edges: Vec<RotationEdge>,
) -> CatalogueRotationPlan {
    let mut proposals = Vec::new();
    let mut exhausted = Vec::new();
    for edge in edges {
        if edge.campaigns_this_month >= edge.max_campaigns_per_month {
            continue;
        }
        match pick_next_release(&edge.catalogue, &edge.already_rotated, generated_at) {
            Some(release) => {
                let reference = campaign_reference(release.release_id);
                proposals.push(CatalogueRotationProposal {
                    consent_id: edge.consent_id,
                    from_workspace_id: edge.from_workspace_id,
                    from_act: edge.from_act.clone(),
                    to_workspace_id: edge.to_workspace_id,
                    to_act: edge.to_act.clone(),
                    campaign_reference: reference,
                    reason: format!(
                        "{}'s fans have never been sent {}'s back catalogue; \"{}\" is the oldest release this edge has not rotated, and {}/{} of the edge's monthly cap is still unspent",
                        edge.from_act,
                        edge.to_act,
                        release.title,
                        edge.max_campaigns_per_month - edge.campaigns_this_month,
                        edge.max_campaigns_per_month,
                    ),
                    release,
                    reachable_fans: edge.reachable_fans,
                    max_campaigns_per_month: edge.max_campaigns_per_month,
                    campaigns_this_month: edge.campaigns_this_month,
                });
            }
            None => exhausted.push(ExhaustedEdge {
                consent_id: edge.consent_id,
                from_act: edge.from_act,
                to_act: edge.to_act,
                rotated_count: edge.already_rotated.len() as u32,
            }),
        }
    }
    proposals.sort_by(|a, b| {
        a.from_act
            .cmp(&b.from_act)
            .then(a.to_act.cmp(&b.to_act))
            .then(a.consent_id.cmp(&b.consent_id))
    });
    exhausted.sort_by_key(|edge| edge.consent_id);
    CatalogueRotationPlan {
        organization_id,
        generated_at,
        proposals,
        exhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn item(id: u128, title: &str, at: OffsetDateTime) -> CatalogueItem {
        CatalogueItem {
            release_id: Uuid::from_u128(id),
            title: title.to_owned(),
            release_at: at,
            listen_url: Some(format!("https://listen/{title}")),
        }
    }

    /// The rotation walks back catalogue oldest-first — month three before
    /// month twenty — and skips what the edge already carried.
    #[test]
    fn the_oldest_unrotated_release_is_next() {
        let now = datetime!(2026-10-01 12:00 UTC);
        let old = item(1, "Month three", datetime!(2026-03-01 12:00 UTC));
        let mid = item(2, "Month nine", datetime!(2026-06-01 12:00 UTC));
        let future = item(3, "Unreleased", datetime!(2026-12-01 12:00 UTC));
        let mut rotated = std::collections::BTreeSet::new();
        rotated.insert(old.release_id);
        let picked = pick_next_release(&[mid.clone(), old.clone(), future], &rotated, now)
            .expect("mid remains");
        assert_eq!(picked.title, "Month nine");
    }

    /// A fully-rotated catalogue picks nothing — exhausted is a state the
    /// plan reports, never a silent empty week.
    #[test]
    fn an_exhausted_catalogue_picks_nothing() {
        let now = datetime!(2026-10-01 12:00 UTC);
        let only = item(1, "Only", datetime!(2026-03-01 12:00 UTC));
        let mut rotated = std::collections::BTreeSet::new();
        rotated.insert(only.release_id);
        assert!(pick_next_release(&[only], &rotated, now).is_none());
    }

    /// An edge at its monthly cap proposes nothing — the cap outvotes.
    #[test]
    fn a_spent_edge_proposes_nothing() {
        let edge = RotationEdge {
            consent_id: Uuid::from_u128(9),
            from_workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(1)),
            from_act: "owner".to_owned(),
            to_workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(2)),
            to_act: "beneficiary".to_owned(),
            max_campaigns_per_month: 2,
            campaigns_this_month: 2,
            reachable_fans: 40,
            catalogue: vec![item(7, "Back then", datetime!(2026-03-01 12:00 UTC))],
            already_rotated: std::collections::BTreeSet::new(),
        };
        let plan = compose(
            Uuid::from_u128(4),
            datetime!(2026-10-01 12:00 UTC),
            vec![edge],
        );
        assert!(plan.proposals.is_empty());
        assert!(plan.exhausted.is_empty());
    }

    /// Headroom plus catalogue produces a proposal that names the pair, the
    /// release, and the cap it lands inside — labelled catalogue.
    #[test]
    fn an_open_edge_proposes_the_oldest_unrotated() {
        let edge = RotationEdge {
            consent_id: Uuid::from_u128(9),
            from_workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(1)),
            from_act: "owner".to_owned(),
            to_workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(2)),
            to_act: "beneficiary".to_owned(),
            max_campaigns_per_month: 2,
            campaigns_this_month: 1,
            reachable_fans: 40,
            catalogue: vec![item(7, "Back then", datetime!(2026-03-01 12:00 UTC))],
            already_rotated: std::collections::BTreeSet::new(),
        };
        let release_id = Uuid::from_u128(7);
        let plan = compose(
            Uuid::from_u128(4),
            datetime!(2026-10-01 12:00 UTC),
            vec![edge],
        );
        assert_eq!(plan.proposals.len(), 1);
        let proposal = &plan.proposals[0];
        assert_eq!(proposal.release.release_id, release_id);
        assert_eq!(
            proposal.campaign_reference,
            format!("catalogue:{release_id}")
        );
        assert!(proposal.reason.contains("1/2"));
    }
}
