// The durable action-kind vocabulary, split out of `model.rs` to keep that file
// under the source-size ratchet. It is one match over every payload variant and
// nothing else, which is also the reason it reads better alone: the list of
// every kind the ledger can hold is a table, not a paragraph.

impl AutopilotActionPayload {
    #[must_use]
    pub const fn action_kind(&self) -> &'static str {
        match self {
            Self::ChangeTicketPrice { .. } => "ticket.price.change",
            Self::ChangeTicketCapacity { .. } => "ticket.capacity.change",
            Self::SetEventTicketUrl { .. } => "event.ticket_url.set",
            Self::RequestFanLifecycleMessage { .. } => "fan.lifecycle.message.request",
            Self::RequestMerchReorder { .. } => "merch.reorder.request",
            Self::ChangeMerchPrice { .. } => "merch.price.change",
            Self::RequestBookingOutreach { .. } => "booking.outreach.request",
            Self::RequestGigOutreach { .. } => "gig.outreach.request",
            Self::RequestLatarnikInvite { .. } => "latarnik.invite.request",
            Self::RequestAudienceCampaign { .. } => "audience.campaign.request",
            Self::RequestMerchBundle { .. } => "merch.bundle.request",
            Self::RequestOutreach { .. } => "outreach.request",
            Self::RequestRepresentationApproach { .. } => "representation.approach.request",
            Self::RequestBookingAgentApproach { .. } => "booking_agent.approach.request",
            Self::RequestBeaconDiscovery { .. } => "beacon.discovery.request",
            Self::RequestBookingTargetDiscovery { .. } => "booking.target_discovery.request",
            Self::RequestBeaconInviteBatch { .. } => "beacon.invite_batch.request",
            Self::RequestOutreachDiscovery { .. } => "outreach.discovery.request",
            Self::RequestBeaconOutreach { .. } => "beacon.outreach.request",
            Self::RequestShowGrowth { .. } => "show.growth.request",
            Self::RequestContentArtifact { .. } => "content.artifact.request",
            Self::AdjustExperiment {
                complete: false, ..
            } => "experiment.allocation.change",
            Self::AdjustExperiment { complete: true, .. } => "experiment.complete",
            Self::CompleteShowTask { .. } => "show.task.complete",
            Self::EscalateShowTask { .. } => "show.task.escalate",
            Self::RequestPromotionBudgetChange { .. } => "promotion.budget_change.request",
            Self::ExecuteReleaseMilestone { .. } => "release.milestone.execute",
            Self::ApplyLiveOpportunity { .. } => "opportunity.live.apply",
            Self::VerifyPlaylistPlacement { .. } => "playlist.placement.verify",
            Self::EscalateEditorialPitch { .. } => "release.editorial_pitch.escalate",
            Self::CounterLiveOpportunityTerms { .. } => "opportunity.terms.counter",
            Self::AcceptLiveOpportunityTerms { .. } => "opportunity.terms.accept",
            Self::IssueCounterpartyReport { .. } => "opportunity.counterparty_report.issue",
            Self::PrepareFundingPackage { .. } => "funding.package.prepare",
            Self::SubmitFundingApplication { .. } => "funding.application.submit",
            Self::RaiseGrowthOpportunity { .. } => "growth.opportunity.raise",
            Self::RaiseDeclineAdvisory { .. } => "community.decline.advisory",
            Self::RaiseGrowthDebt { .. } => "growth.debt.raise",
            Self::IssueReferralCode { .. } => "referral.code.issue",
            Self::RaiseContentSuggestion { .. } => "content.suggestion.raise",
            Self::RaiseContentArc { .. } => "content.arc.raise",
            Self::RunPlayStep { .. } => "play.step.run",
            Self::SendTeamAssignmentEmail { .. } => "team.assignment.email",
            Self::RequestAgentContent { .. } => "agent.content.request",
            Self::RequestOutreachTarget { .. } => "outreach.target.request",
            Self::RequestAgentRun { .. } => "agent.run.request",
            Self::RequestCommunityEngagement { .. } => "community.engage.request",
            Self::RequestSignalPush { .. } => "signal.push.request",
        }
    }
}
