// Deterministic person discovery -> canonical FAN SCOUT prospect.
//
// The language model is allowed to SELECT a candidate_ref and explain why it
// thinks the candidate fits. It is never allowed to author the identity we
// store. Identity, source URL, time and evidence come from
// agent_service_tasks.metadata.fan_scout_candidates, which the deterministic
// discovery tool wrote before the model ran.

use time::format_description::well_known::Rfc3339;

#[derive(Debug, serde::Deserialize)]
struct TrustedFanProspectCandidate {
    candidate_ref: String,
    platform: String,
    #[serde(default)]
    platform_user_id: Option<String>,
    #[serde(default)]
    handle: Option<String>,
    display_identity: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    profile_url: Option<String>,
    source_ref: String,
    source_url: String,
    observed_at: String,
    evidence: String,
}

fn ungrounded_fan_prospect(reason: impl Into<String>) -> OutcomeRejection {
    OutcomeRejection::UngroundedFanProspect {
        reason: reason.into(),
    }
}

fn bandcamp_url(raw: &str) -> bool {
    url::Url::parse(raw).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|host| {
                let host = host.to_ascii_lowercase();
                host == "bandcamp.com" || host.ends_with(".bandcamp.com")
            })
    })
}

fn validate_fan_prospect_candidate(
    item: &Value,
    task_metadata: &Value,
    now: OffsetDateTime,
) -> Result<TrustedFanProspectCandidate, OutcomeRejection> {
    let candidate_ref = item
        .get("candidate_ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .ok_or_else(|| ungrounded_fan_prospect("candidate_ref is missing or invalid"))?;

    let candidates = task_metadata
        .get("fan_scout_candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ungrounded_fan_prospect(
                "the task has no deterministic fan_scout_candidates evidence",
            )
        })?;
    let value = candidates
        .iter()
        .find(|candidate| {
            candidate.get("candidate_ref").and_then(Value::as_str) == Some(candidate_ref)
        })
        .ok_or_else(|| {
            ungrounded_fan_prospect(
                "candidate_ref is not one of the deterministic candidates shown to the model",
            )
        })?;
    let candidate: TrustedFanProspectCandidate = serde_json::from_value(value.clone())
        .map_err(|_| ungrounded_fan_prospect("candidate metadata is malformed"))?;

    if candidate.platform != "bandcamp" {
        return Err(ungrounded_fan_prospect(
            "bandcamp-scanner candidate platform is not bandcamp",
        ));
    }
    if candidate.candidate_ref != candidate_ref {
        return Err(ungrounded_fan_prospect("candidate_ref metadata mismatch"));
    }
    if !bandcamp_url(&candidate.source_url) {
        return Err(ungrounded_fan_prospect(
            "candidate source_url is not a public Bandcamp page",
        ));
    }
    if let Some(profile_url) = candidate.profile_url.as_deref()
        && !bandcamp_url(profile_url)
    {
        return Err(ungrounded_fan_prospect(
            "candidate profile_url is not a Bandcamp page",
        ));
    }
    if candidate.source_ref.trim().is_empty() || candidate.evidence.trim().is_empty() {
        return Err(ungrounded_fan_prospect(
            "candidate is missing deterministic source identity/evidence",
        ));
    }
    if candidate.platform_user_id.as_deref().is_none_or(str::is_empty)
        && candidate.handle.as_deref().is_none_or(str::is_empty)
    {
        return Err(ungrounded_fan_prospect(
            "candidate has neither stable Bandcamp user id nor public handle",
        ));
    }
    let observed_at = OffsetDateTime::parse(candidate.observed_at.trim(), &Rfc3339)
        .map_err(|_| ungrounded_fan_prospect("candidate observed_at is not RFC3339"))?;
    if observed_at > now + time::Duration::hours(1) {
        return Err(ungrounded_fan_prospect(
            "candidate observed_at is in the future",
        ));
    }
    Ok(candidate)
}

impl AgentOutcomeWorker {
    async fn fan_prospect_subject(
        &self,
        outcome: &ValidatedOutcome,
        producing_task: Option<&(String, String, Value)>,
    ) -> Result<(&'static str, Uuid), AgentOutcomeError> {
        let Some(item) = outcome.payload.item.as_ref() else {
            return Ok(("agent_outcome", outcome.id));
        };
        let Some((template, _, task_metadata)) = producing_task else {
            return Err(ungrounded_fan_prospect("producing task is unavailable").into());
        };
        if template != "bandcamp-scanner" {
            return Err(ungrounded_fan_prospect(format!(
                "fan_prospects may only come from bandcamp-scanner in this slice, got {template:?}"
            ))
            .into());
        }
        let candidate =
            validate_fan_prospect_candidate(item, task_metadata, OffsetDateTime::now_utc())?;
        let observed_at = OffsetDateTime::parse(candidate.observed_at.trim(), &Rfc3339)
            .map_err(|_| ungrounded_fan_prospect("candidate observed_at is not RFC3339"))?;
        let seen = crowdrelay_infra::fan_prospects::ObservedPerson {
            source: crowdrelay_domain::fan_prospect::ProspectSource::BandcampCollectors,
            platform: "bandcamp",
            platform_user_id: candidate.platform_user_id.as_deref(),
            handle: candidate.handle.as_deref(),
            display_identity: &candidate.display_identity,
            display_name: candidate.display_name.as_deref(),
            profile_url: candidate.profile_url.as_deref(),
            kind: crowdrelay_domain::fan_prospect::ObservationKind::CollectsSimilarMusic,
            source_ref: &candidate.source_ref,
            source_url: Some(&candidate.source_url),
            observed_at,
            evidence: &candidate.evidence,
            // This is not the model's confidence. The identity and membership
            // in the public collector list were parsed by deterministic code
            // and frozen on the task before generation.
            confidence_basis_points: 10_000,
        };
        match crowdrelay_infra::fan_prospects::observe(
            &self.pool,
            outcome.workspace_id,
            &seen,
        )
        .await?
        {
            crowdrelay_infra::fan_prospects::ObserveOutcome::Created { prospect_id }
            | crowdrelay_infra::fan_prospects::ObserveOutcome::Known {
                prospect_id,
                ..
            } => Ok(("fan_prospect", prospect_id)),
            crowdrelay_infra::fan_prospects::ObserveOutcome::NotCollected { .. } => {
                Err(ungrounded_fan_prospect(
                    "the known prospect is refused or suppressed; no further collection is allowed",
                )
                .into())
            }
            crowdrelay_infra::fan_prospects::ObserveOutcome::NotAnIdentity => {
                Err(ungrounded_fan_prospect("candidate identity is unusable").into())
            }
            crowdrelay_infra::fan_prospects::ObserveOutcome::IdentityConflict => {
                Err(ungrounded_fan_prospect(
                    "candidate stable id and handle already belong to different people",
                )
                .into())
            }
        }
    }
}

#[cfg(test)]
mod fan_prospect_outcome_tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn metadata() -> Value {
        json!({
            "fan_scout_candidates": [{
                "candidate_ref": "bc_123",
                "platform": "bandcamp",
                "platform_user_id": "123",
                "handle": "metal_kuba",
                "display_identity": "metal_kuba",
                "display_name": "Kuba",
                "profile_url": "https://bandcamp.com/metal_kuba",
                "source_ref": "bc_123",
                "source_url": "https://exampleband.bandcamp.com/album/heavy",
                "observed_at": "2026-10-02T12:00:00Z",
                "evidence": "Public collector entry for Heavy by Example Band"
            }]
        })
    }

    #[test]
    fn the_model_can_only_select_a_candidate_the_tool_recorded() {
        let good = json!({"candidate_ref":"bc_123"});
        let candidate = validate_fan_prospect_candidate(
            &good,
            &metadata(),
            datetime!(2026-10-02 13:00 UTC),
        )
        .expect("grounded");
        assert_eq!(candidate.platform_user_id.as_deref(), Some("123"));

        let invented = json!({"candidate_ref":"bc_invented"});
        let reason = validate_fan_prospect_candidate(
            &invented,
            &metadata(),
            datetime!(2026-10-02 13:00 UTC),
        )
        .expect_err("must reject");
        assert!(reason.to_string().contains("deterministic candidates"));
    }

    #[test]
    fn a_non_bandcamp_source_cannot_ride_the_bandcamp_person_lane() {
        let mut bad = metadata();
        bad["fan_scout_candidates"][0]["source_url"] =
            json!("https://evil.example/collector");
        assert!(
            validate_fan_prospect_candidate(
                &json!({"candidate_ref":"bc_123"}),
                &bad,
                datetime!(2026-10-02 13:00 UTC),
            )
            .is_err()
        );
    }
}
