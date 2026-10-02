// The authority decision for an agent outcome: which context it belongs to,
// and whether both authority axes let it run unattended. Split out of
// `agent_outcomes.rs` so the rule lives in one place with the tests that pin
// it, and so the ingestion worker stays readable -- `include!`d rather than
// declared as a module because it extends the same `impl` and the same test
// module, exactly as `press_recipient.rs` does.

impl AgentOutcomeWorker {
    /// Whether both authority axes permit this action to run unattended.
    ///
    /// The context's policy level answers "how much may this context do"; the
    /// class ceiling answers "how far may an action that costs *this* be
    /// allowed to go". `action_class` documents that the stricter of the two
    /// wins, and `evaluate::persist` applies exactly that rule to the brain's
    /// own candidates. This path read one policy row and called it authority,
    /// which is wrong in both directions: a class ceiling an operator had
    /// deliberately shut could be walked past by a context that happened to be
    /// open, and widening a class could not open a context.
    ///
    /// A row that is missing, or whose level this build cannot parse, is the
    /// safest value on that axis and never an absent limit. A row a newer
    /// deploy wrote is not a grant of authority to an older one.
    ///
    /// `target_key` names the one target a standing approval could cover.
    /// `None` means no grant is even looked for, which is the right default
    /// for an action whose reach is not one nameable thing: an operator
    /// cannot have judged "this target" when there is no this.
    /// Which standing answer, if any, authorizes this action to run
    /// unattended — the workspace's axes (`Policy`), a standing grant for
    /// its one named target (`Grant`), or neither (`Denied`).
    async fn may_auto_execute(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        context: &str,
        class: ActionClass,
        action_kind: &str,
        target_key: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<UnattendedAuthority, AgentOutcomeError> {
        let context_level: Option<String> = sqlx::query_scalar(
            r#"
            SELECT autonomy_level
            FROM autopilot_policies
            WHERE workspace_id = $1 AND context = $2
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(context)
        .fetch_optional(&mut **tx)
        .await?;
        // No policy row means the context has no authority at all, which is
        // stricter than `observe` and the right reading of a context nobody
        // provisioned.
        let Some(context_level) = context_level.as_deref().and_then(AutonomyLevel::parse) else {
            return Ok(UnattendedAuthority::Denied);
        };
        let ceiling: Option<String> = sqlx::query_scalar(
            r#"
            SELECT ceiling
            FROM growth_autonomy
            WHERE workspace_id = $1 AND action_class = $2
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(class.as_str())
        .fetch_optional(&mut **tx)
        .await?;
        let ceiling = ceiling
            .as_deref()
            .and_then(AutonomyLevel::parse)
            .unwrap_or_else(|| class.safest_ceiling());
        let authority = effective_authority(context_level, ceiling);
        // The two axes have agreed. What is left is the question they cannot
        // ask: has the operator already judged *this target* and said not to
        // be asked again. `standing_approval::unattended_authority` owns the
        // rule, including the three things a grant may not do, and the enum
        // tells the caller which standing answer fired — a relay batch
        // answers to `Policy` (the workspace spoke for the whole spread)
        // where `Grant` speaks for this one community alone.
        let grant = match target_key {
            Some(target_key) => self.standing_grant(tx, action_kind, target_key).await?,
            None => None,
        };
        Ok(unattended_authority(authority, grant, now))
    }

    /// The live standing grant for one target, if the operator wrote one.
    ///
    /// Revoked rows are read rather than filtered out in SQL: `is_live` owns
    /// what "live" means, and a second copy of that rule in a WHERE clause is
    /// a second place for it to drift. The class comes back from the row, so
    /// a grant written while an action kind carried one class cannot license
    /// that kind after it has been reclassified into another.
    async fn standing_grant(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        action_kind: &str,
        target_key: &str,
    ) -> Result<Option<StandingGrant>, AgentOutcomeError> {
        let row: Option<(String, OffsetDateTime, Option<OffsetDateTime>)> = sqlx::query_as(
            r#"
            SELECT action_class, expires_at, revoked_at
            FROM standing_approvals
            WHERE workspace_id = $1
              AND action_kind = $2
              AND target_key = $3
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(action_kind)
        .bind(target_key)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(row.and_then(|(class, expires_at, revoked_at)| {
            Some(StandingGrant {
                // A class this build cannot parse is not a grant. Same rule as
                // the authority rows above: unreadable is never permissive.
                class: ActionClass::parse(&class)?,
                expires_at,
                revoked_at,
            })
        }))
    }
}

/// What the authority axes may still do for text a language model wrote.
///
/// Everything this worker ingests was written by a model. A workspace-wide
/// answer — a context at `bounded_auto`, a class ceiling that allows it — is
/// not a decision about any particular text, and on 2026-08-31 and 2026-09-03
/// it let exactly that reach real people unread: four push notifications
/// promising fans a presale code and rehearsal footage that did not exist, and
/// an invented rehearsal story posted to a subreddit, all approved as
/// `policy:bounded_auto`. So `Policy` is withdrawn here.
///
/// A standing grant survives, because it is not a setting: it is an operator
/// who looked at one named community and said "stop asking me about this
/// one", and it is revocable per target. That, an approved relay card, or a
/// person approving the action itself are the only ways model text leaves.
fn model_text_authority(
    authority: UnattendedAuthority,
    strategic_review_passed: bool,
) -> UnattendedAuthority {
    if !strategic_review_passed {
        return UnattendedAuthority::Denied;
    }
    match authority {
        UnattendedAuthority::Policy => UnattendedAuthority::Denied,
        other => other,
    }
}

/// The community this post is for, or `None` when it is not a community post.
///
/// Pure payload read: `platform == "reddit"` plus a `target_id` that parses.
/// Whether that community exists and was admitted is a separate question the
/// admission gate answers against the database.
fn community_target_id(outcome: &ValidatedOutcome) -> Option<Uuid> {
    if outcome.kind != OutcomeKind::SocialPost {
        return None;
    }
    outcome
        .payload
        .item
        .as_ref()
        .and_then(|i| i.get("platform"))
        .and_then(Value::as_str)
        // The platforms a community may live on — the same set
        // `community_posts.platform` names. A `social_post` outcome naming
        // any other platform (an owned-channel post: instagram, facebook)
        // with a `target_id` still resolves nothing, because the admit gate
        // downstream only accepts a real admitted community target.
        .filter(|platform| {
            matches!(
                *platform,
                "reddit" | "discord" | "telegram" | "forum" | "lemmy" | "brutalland"
            )
        })
        .and_then(|_| {
            outcome
                .payload
                .item
                .as_ref()
                .and_then(|i| i.get("target_id"))
                .and_then(Value::as_str)
                .and_then(|s| Uuid::parse_str(s).ok())
        })
}

/// The autopilot context this outcome's decision and action belong to.
///
/// `OutcomeKind::autopilot_context` answers for the kind, and for a community
/// post the kind is not enough. A `SocialPost` maps to `promotion_budget`,
/// which is the money context: `GrowthPosture` pins it to `require_approval`
/// in every posture including `full_send` — asserted by
/// `gig_and_money_contexts_stay_human_even_at_full_send` — so while a forum
/// post answered to it, no posture an operator could choose ever let one go
/// out unattended. Measured against production: zero community posts
/// published, and the operator had no setting that would have changed it.
///
/// A post to a subreddit spends no money. It spends a relationship with a
/// community, which is `ActionClass::ThirdParty`, and `outreach` is the
/// context where free third-party contact already lives — the one `full_send`
/// promotes to `bounded_auto` and `working` keeps drafted. That ladder is the
/// behaviour the posture names promise.
///
/// The money gate is untouched. `paid` stays behind approval in every posture,
/// and an action that actually spends still reads `promotion_budget`.
fn effective_context(
    outcome: &ValidatedOutcome,
    community_target_id: Option<Uuid>,
) -> &'static str {
    if community_target_id.is_some() {
        "outreach"
    } else {
        outcome.kind.autopilot_context()
    }
}

#[cfg(test)]
mod authority_tests {
    use super::tests::make_outcome;
    use super::*;
    use crowdrelay_application::agent_outcomes::OutcomeKind;

        fn community_post(target_id: Uuid) -> ValidatedOutcome {
            make_outcome(
                OutcomeKind::SocialPost,
                8000,
                Some(json!({
                    "platform": "reddit",
                    "target_id": target_id.to_string(),
                    "title": "test",
                    "body": "test",
                })),
            )
        }

        #[test]
        fn model_text_needs_both_a_strategic_pass_and_existing_target_authority() {
            assert_eq!(
                model_text_authority(UnattendedAuthority::Grant, true),
                UnattendedAuthority::Grant
            );
            assert_eq!(
                model_text_authority(UnattendedAuthority::Grant, false),
                UnattendedAuthority::Denied,
                "a reviewer outage or negative review sends the draft to a person instead of using the standing grant"
            );
            assert_eq!(
                model_text_authority(UnattendedAuthority::Policy, true),
                UnattendedAuthority::Denied,
                "the reviewer is never a new source of workspace-wide authority"
            );
        }

        /// The whole of fix one: a forum post no longer answers to the money
        /// context. While it did, `GrowthPosture` pinned `promotion_budget` to
        /// `require_approval` in every posture, so no setting an operator could
        /// choose ever let a community post publish unattended.
        #[test]
        fn a_community_post_answers_to_outreach_not_the_money_context() {
            let target_id = Uuid::now_v7();
            let outcome = community_post(target_id);
            assert_eq!(
                outcome.kind.autopilot_context(),
                "promotion_budget",
                "the kind alone still says money — which is exactly why the kind alone is not enough"
            );
            assert_eq!(
                effective_context(&outcome, community_target_id(&outcome)),
                "outreach"
            );
        }

        /// The money gate is untouched. A social post with no community behind it
        /// is a campaign draft and keeps the context it always had.
        #[test]
        fn a_social_post_with_no_community_keeps_its_own_context() {
            let outcome = make_outcome(
                OutcomeKind::SocialPost,
                8000,
                Some(json!({"platform": "instagram", "title": "test", "body": "test"})),
            );
            assert_eq!(community_target_id(&outcome), None);
            assert_eq!(
                effective_context(&outcome, community_target_id(&outcome)),
                "promotion_budget"
            );
        }

        #[test]
        fn every_other_kind_keeps_the_context_its_kind_names() {
            for kind in [
                OutcomeKind::PressPitch,
                OutcomeKind::SignalPush,
                OutcomeKind::AudienceSegments,
                OutcomeKind::OutreachTargets,
                OutcomeKind::BeaconCandidates,
                OutcomeKind::ContactResearch,
                OutcomeKind::OpportunityFindings,
                OutcomeKind::CampaignInsight,
                OutcomeKind::ReleasePlanNote,
                OutcomeKind::GenericInsight,
            ] {
                let outcome = make_outcome(kind, 8000, Some(json!({"title": "t", "body": "b"})));
                assert_eq!(community_target_id(&outcome), None);
                assert_eq!(
                    effective_context(&outcome, None),
                    kind.autopilot_context(),
                    "{} must keep its own context",
                    kind.as_str()
                );
            }
        }

        /// A target_id that is not a UUID is a model that invented one. The post
        /// falls through to the generic content path rather than pointing at a
        /// community that does not exist — and so keeps the stricter context.
        #[test]
        fn an_unparseable_target_id_is_not_a_community_post() {
            let outcome = make_outcome(
                OutcomeKind::SocialPost,
                8000,
                Some(json!({"platform": "reddit", "target_id": "r/metal", "title": "t", "body": "b"})),
            );
            assert_eq!(community_target_id(&outcome), None);
            assert_eq!(
                effective_context(&outcome, community_target_id(&outcome)),
                "promotion_budget"
            );
        }

        /// The two axes together, without a database: a community post is
        /// `ThirdParty`, so `full_send` opens it and `working` keeps it drafted.
        /// The same class under `grounded` stays shut whatever the context says.
        #[test]
        fn the_stricter_axis_wins_for_a_community_post() {
            use crowdrelay_application::autopilot::{AutopilotContext, GrowthPosture};

            for (posture, expected) in [
                (GrowthPosture::Grounded, false),
                (GrowthPosture::Working, false),
                (GrowthPosture::FullSend, true),
            ] {
                let level = posture.context_level(AutopilotContext::Outreach);
                let ceiling = posture.ceiling(ActionClass::ThirdParty);
                assert_eq!(
                    effective_authority(level, ceiling).may_auto_execute(),
                    expected,
                    "{} must {} a community post to publish unattended",
                    posture.as_str(),
                    if expected { "let" } else { "not let" }
                );
            }
        }
}
