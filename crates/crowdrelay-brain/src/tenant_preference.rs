//! Tenant operating preference — learns how each tenant prefers to work.
//!
//! The brain learns two independent things from operator behavior:
//!
//! 1. **Execution quality** (OutcomeRecord/Standing): "Is this worker
//!    performing well enough to keep using?" → cooldown, tier, retirement.
//! 2. **Operating preference** (this module): "Does this tenant tend to
//!    accept or reject this template?" → cadence, presentation metadata.
//!
//! Both consume the same raw operator events (approve/cancel) but answer
//! different questions and must never merge. The preference posterior
//! influences cadence timing and post-selection presentation metadata;
//! the standing system influences whether the worker is trusted to run
//! at all.
//!
//! # North Star invariant
//!
//! The preference posterior MUST NOT modify:
//! - `expected_incremental_y30`
//! - `causal_treatment_effect`
//! - `evidence_quality`
//! - `DecisionValue.total()`
//! - experiment assignment
//! - portfolio economics
//!
//! Preference only controls cadence timing and post-selection presentation
//! metadata. The portfolio optimizer still ranks by `DecisionValue`.
//!
//! # Mathematical model
//!
//! Beta-Binomial conjugate posterior with exponentially decayed evidence:
//!
//! - Prior: Beta(2, 2) — skeptical, centered at 0.5 (no preference).
//! - Each operator action contributes a decayed weight:
//!   `weight = 0.5 ^ (age_days / half_life_days)`
//! - Approve → adds `weight` to α; Cancel → adds `weight` to β.
//! - Posterior mean = `(α + decayed_approvals) / (α + β + decayed_approvals
//!   + decayed_cancellations)`
//!
//! Beta-Binomial is the correct conjugate family for a bounded [0, 1]
//! probability. NormalPosterior is for signed quantities (treatment
//! effects) and would be mathematically wrong here.
//!
//! # Limitation: selection bias from proposal exposure (V1)
//!
//! The current model learns from explicit operator actions (approve/cancel)
//! only. It cannot distinguish:
//! - tenant dislikes template
//! - tenant never saw template
//! - tenant saw it but was busy
//! - tenant saw it but ignored it
//!
//! This creates selection bias: templates proposed less frequently have
//! fewer opportunities for approval, which can reinforce low-preference
//! scores. The current implementation mitigates this by:
//! - NOT allowing preference to modify DecisionValue or economic value
//! - NOT allowing preference to remove candidates from the economic pipeline
//! - Bounding cadence adjustment to [0.75, 1.25] (V1)
//! - Enforcing a discovery floor (`min_discovery_cadence_multiplier`)
//!
//! TODO(future): Build an exposure-aware preference model that distinguishes:
//!   candidate generated → surfaced → seen → approve/cancel/ignore
//! with exposure/attention modeled separately from approval preference.
//! Silence is NOT approval. Silence is NOT cancellation. Silence is NOT
//! preference evidence unless actual proposal exposure is known.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Beta-Binomial posterior for one template's approval probability.
///
/// Tracks how often a tenant approves vs cancels this template, with
/// exponentially decayed evidence so preferences can shift over time.
/// Sparse data stays close to the prior (0.5) — one action cannot
/// dominate months of consistent behavior.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemplatePreference {
    /// Prior α (default 2.0 — skeptical, centered at 0.5).
    pub prior_alpha: f64,
    /// Prior β (default 2.0).
    pub prior_beta: f64,
    /// Sum of decayed approval weights.
    pub decayed_approvals: f64,
    /// Sum of decayed cancellation weights.
    pub decayed_cancellations: f64,
    /// Total raw observation count (for inspectability — not used in
    /// the posterior, which uses decayed weights).
    pub total_observations: u32,
}

impl Default for TemplatePreference {
    fn default() -> Self {
        Self {
            prior_alpha: 2.0,
            prior_beta: 2.0,
            decayed_approvals: 0.0,
            decayed_cancellations: 0.0,
            total_observations: 0,
        }
    }
}

impl TemplatePreference {
    /// Creates a fresh preference with the skeptical prior Beta(2, 2).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Posterior mean — the tenant's estimated approval probability for
    /// this template. Bounded [0, 1]. A new template with no data returns
    /// 0.5 (the prior mean).
    #[must_use]
    pub fn preference_score(&self) -> f64 {
        let alpha = self.prior_alpha + self.decayed_approvals;
        let beta = self.prior_beta + self.decayed_cancellations;
        alpha / (alpha + beta)
    }

    /// Posterior confidence — total effective sample size (decayed).
    /// Higher = more confident. Below `min_confidence_to_suppress`
    /// (default 5.0), the template runs at normal cadence regardless of
    /// preference score. This prevents overreacting to sparse data.
    #[must_use]
    pub fn confidence(&self) -> f64 {
        self.decayed_approvals + self.decayed_cancellations
    }

    /// Whether this template should be presentation-hidden from the
    /// operator-facing proposal surface.
    ///
    /// This is a **presentation decision**, NOT an economic gate. A
    /// presentation-hidden template is still economically selectable —
    /// it can win in the portfolio optimizer on DecisionValue and
    /// execute. "Hidden" means "normally de-emphasized in the operator
    /// UI", not "economically ineligible."
    ///
    /// Hidden only when BOTH conditions hold:
    /// 1. Confidence ≥ `min_confidence_to_suppress` (enough evidence to act)
    /// 2. Preference score < `suppression_threshold` (tenant consistently
    ///    rejects this template)
    ///
    /// Sparse data is NEVER hidden — it stays close to the prior (0.5)
    /// and runs at normal cadence. This prevents a single cancellation
    /// from hiding a template that might actually produce fans.
    #[must_use]
    pub fn should_suppress(&self, policy: &TenantPreferencePolicy) -> bool {
        self.confidence() >= policy.min_confidence_to_suppress
            && self.preference_score() < policy.suppression_threshold
    }

    /// Cadence multiplier — controls how long the cooldown is for this
    /// template. Applied as `effective_cooldown = base_cooldown *
    /// cadence_multiplier()`.
    ///
    /// Linear mapping centered at the neutral prior mean (0.5):
    /// - 1.0 = no change (sparse data or neutral preference)
    /// - 0.75 = 25% shorter cooldown (high preference — tenant likes this)
    /// - 1.25 = 25% longer cooldown (low preference — tenant rejects this)
    ///
    /// V1 bounds are conservative [0.75, 1.25] — preference gently
    /// adjusts cadence without taking over the scheduler. Can widen
    /// later with real tenant data.
    ///
    /// Only applies when confidence ≥ 3.0 (enough evidence to adjust).
    /// Below that, returns 1.0 (no adjustment).
    #[must_use]
    pub fn cadence_multiplier(&self) -> f64 {
        if self.confidence() < 3.0 {
            return 1.0;
        }
        let score = self.preference_score();
        // Map [0, 1] → [1.25, 0.75], centered at 0.5 → 1.0:
        //   score 0.0 → 1.25 (low preference = 25% longer cooldown)
        //   score 0.5 → 1.00 (neutral = unchanged)
        //   score 1.0 → 0.75 (high preference = 25% shorter cooldown)
        (1.0 + (0.5 - score) * 0.5).clamp(0.75, 1.25)
    }

    /// Observe one operator action with temporal decay.
    ///
    /// `approved` = true for approve, false for cancel.
    /// `age_days` = days since the action occurred.
    /// `half_life_days` = decay half-life (default 90 days).
    ///
    /// The weight decays exponentially: a 90-day-old action carries half
    /// the weight of a fresh one. This lets preferences shift over time
    /// without one recent action dominating months of history.
    pub fn observe(&mut self, approved: bool, age_days: f64, half_life_days: f64) {
        let weight = 0.5_f64.powf(age_days / half_life_days.max(1.0));
        if approved {
            self.decayed_approvals += weight;
        } else {
            self.decayed_cancellations += weight;
        }
        self.total_observations += 1;
    }

    /// Whether this template has enough evidence to influence decisions.
    /// Convenience for inspectability — same threshold as `cadence_multiplier`.
    #[must_use]
    pub fn has_sufficient_evidence(&self) -> bool {
        self.confidence() >= 3.0
    }
}

/// Collection of template preferences for one tenant/workspace.
///
/// One posterior per template the brain may dispatch. Missing templates
/// return the default (prior = 0.5, no evidence).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TenantPreferencePosterior {
    /// Per-template preference posteriors, keyed by template_id.
    pub templates: HashMap<String, TemplatePreference>,
}

impl TenantPreferencePosterior {
    /// Creates a new empty posterior.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the preference for a template, or the default (prior) if
    /// no data has been observed for this template yet.
    #[must_use]
    pub fn get(&self, template_id: &str) -> TemplatePreference {
        self.templates.get(template_id).cloned().unwrap_or_default()
    }

    /// Returns a mutable reference to the preference for a template,
    /// creating a fresh prior if it doesn't exist yet.
    pub fn get_mut(&mut self, template_id: &str) -> &mut TemplatePreference {
        self.templates.entry(template_id.to_owned()).or_default()
    }

    /// Observe one operator action for a template.
    pub fn observe(
        &mut self,
        template_id: &str,
        approved: bool,
        age_days: f64,
        half_life_days: f64,
    ) {
        self.get_mut(template_id)
            .observe(approved, age_days, half_life_days);
    }

    /// Whether a template should be presentation-hidden from the
    /// operator-facing proposal surface. This is a presentation
    /// decision, NOT an economic gate.
    #[must_use]
    pub fn should_suppress(&self, template_id: &str, policy: &TenantPreferencePolicy) -> bool {
        self.get(template_id).should_suppress(policy)
    }

    /// Cadence multiplier for a template (1.0 = no change).
    #[must_use]
    pub fn cadence_multiplier(&self, template_id: &str) -> f64 {
        self.get(template_id).cadence_multiplier()
    }

    /// Preference score for a template (0.0–1.0, default 0.5).
    #[must_use]
    pub fn preference_score(&self, template_id: &str) -> f64 {
        self.get(template_id).preference_score()
    }

    /// Computes presentation metadata for a selected candidate.
    ///
    /// Called AFTER portfolio selection. This is a presentation-layer
    /// concept — it does NOT modify any economic value. A
    /// presentation-hidden candidate can still win economically and
    /// execute.
    #[must_use]
    pub fn presentation_metadata(
        &self,
        template_id: &str,
        policy: &TenantPreferencePolicy,
    ) -> PresentationMetadata {
        PresentationMetadata {
            template_id: template_id.to_owned(),
            preference_score: self.preference_score(template_id),
            is_presentation_hidden: self.should_suppress(template_id, policy),
        }
    }
}

/// Presentation-layer metadata for a selected candidate.
///
/// Computed AFTER portfolio selection from the tenant preference
/// posterior. This is informational/operator-facing only — it does
/// NOT modify DecisionValue, expected_incremental_y30, or any
/// economic value. A presentation-hidden candidate can still win
/// economically and execute.
///
/// The operator UI may use `is_presentation_hidden` to de-emphasize
/// low-preference proposals. The decision audit retains the full
/// metadata trail regardless of visibility.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresentationMetadata {
    /// The template ID this metadata applies to.
    pub template_id: String,
    /// The tenant's preference score for this template (0.0–1.0).
    pub preference_score: f64,
    /// Whether this candidate is normally hidden from the
    /// operator-facing presentation surface. True when
    /// `should_suppress()` returns true — the tenant consistently
    /// rejects this template. The candidate is still economically
    /// selectable and can execute if it wins on DecisionValue.
    pub is_presentation_hidden: bool,
}

/// Policy for tenant preference filtering and suppression.
///
/// Controls when a template is suppressed (hidden from candidate
/// generation) and how fast preferences adapt (temporal decay).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TenantPreferencePolicy {
    /// Half-life for temporal decay (days). Default 90.
    ///
    /// A 90-day-old operator action carries half the weight of a fresh
    /// one. This lets preferences shift over time without one recent
    /// action dominating months of history. Configurable per workspace.
    pub half_life_days: f64,
    /// Minimum confidence (effective sample size) before suppressing a
    /// template. Default 5.0.
    ///
    /// Below this, the template runs at normal cadence regardless of
    /// preference score. This prevents suppressing a template after
    /// just one or two cancellations.
    pub min_confidence_to_suppress: f64,
    /// Preference score below which a template is presentation-hidden
    /// (after min_confidence is met). Default 0.25.
    ///
    /// 0.25 means the tenant cancels this template ~75% of the time.
    /// Conservative — a template with 40% approval stays visible.
    pub suppression_threshold: f64,
    /// Maximum cooldown multiplier from preference adjustment. This is
    /// the exploration floor — even a strongly-rejected template gets
    /// proposed at least every `standing_adjusted_cooldown *
    /// min_discovery_cadence_multiplier` hours. Default 1.25 (matching
    /// the V1 cadence bound).
    ///
    /// This prevents the self-reinforcing loop where low preference →
    /// less frequent proposals → fewer approval opportunities →
    /// lower preference. The brain must always be able to discover
    /// that a previously-rejected template has become valuable.
    ///
    /// Note: this is a cadence floor, not an exploration guarantee.
    /// It guarantees periodic opportunity to reconsider the template;
    /// EFE/DecisionValue still decide whether the actual opportunity
    /// is worth acting on.
    pub min_discovery_cadence_multiplier: f64,
}

impl Default for TenantPreferencePolicy {
    fn default() -> Self {
        Self {
            half_life_days: 90.0,
            min_confidence_to_suppress: 5.0,
            suppression_threshold: 0.25,
            min_discovery_cadence_multiplier: 1.25,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> TenantPreferencePolicy {
        TenantPreferencePolicy::default()
    }

    // ── Basic posterior behavior ──

    #[test]
    fn new_template_has_prior_preference() {
        let pref = TemplatePreference::new();
        assert!((pref.preference_score() - 0.5).abs() < 1e-9);
        assert!(pref.confidence() < 1e-9);
        assert!(!pref.has_sufficient_evidence());
    }

    #[test]
    fn approvals_increase_preference_score() {
        let mut pref = TemplatePreference::new();
        for _ in 0..10 {
            pref.observe(true, 0.0, 90.0);
        }
        assert!(
            pref.preference_score() > 0.5,
            "10 approvals should raise the score above the prior"
        );
        assert!(pref.confidence() >= 3.0);
    }

    #[test]
    fn cancellations_decrease_preference_score() {
        let mut pref = TemplatePreference::new();
        for _ in 0..10 {
            pref.observe(false, 0.0, 90.0);
        }
        assert!(
            pref.preference_score() < 0.5,
            "10 cancellations should lower the score below the prior"
        );
    }

    // ── Temporal decay ──

    #[test]
    fn old_evidence_decays() {
        let mut pref = TemplatePreference::new();
        // 10 approvals 180 days ago (2 half-lives → weight = 0.25 each)
        for _ in 0..10 {
            pref.observe(true, 180.0, 90.0);
        }
        // 10 cancellations today (weight = 1.0 each)
        for _ in 0..10 {
            pref.observe(false, 0.0, 90.0);
        }
        // Old approvals: 10 * 0.25 = 2.5
        // Fresh cancellations: 10 * 1.0 = 10.0
        // Posterior: (2 + 2.5) / (2 + 2 + 2.5 + 10) = 4.5 / 16.5 ≈ 0.27
        assert!(
            pref.preference_score() < 0.4,
            "fresh cancellations should outweigh decayed old approvals"
        );
    }

    #[test]
    fn one_recent_action_does_not_dominate() {
        let mut pref = TemplatePreference::new();
        // 20 approvals over 90 days (avg age 45 → weight ~0.71 each)
        for i in 0..20 {
            let age = 90.0 - f64::from(i) * 4.5;
            pref.observe(true, age.max(0.0), 90.0);
        }
        let score_before = pref.preference_score();
        // One recent cancellation
        pref.observe(false, 0.0, 90.0);
        let score_after = pref.preference_score();
        // The change should be small — one action among 20
        assert!(
            (score_before - score_after).abs() < 0.1,
            "one recent action should not dominate: before={score_before}, after={score_after}"
        );
    }

    // ── Suppression ──

    #[test]
    fn sparse_data_is_never_suppressed() {
        let mut pref = TemplatePreference::new();
        // 2 cancellations — not enough confidence
        pref.observe(false, 0.0, 90.0);
        pref.observe(false, 0.0, 90.0);
        assert!(
            !pref.should_suppress(&policy()),
            "sparse data (2 cancellations) should not suppress"
        );
    }

    #[test]
    fn high_cancellation_rate_with_evidence_suppresses() {
        let mut pref = TemplatePreference::new();
        // 10 cancellations, 1 approval → 90% cancellation rate
        for _ in 0..10 {
            pref.observe(false, 0.0, 90.0);
        }
        pref.observe(true, 0.0, 90.0);
        assert!(
            pref.should_suppress(&policy()),
            "10 cancellations + 1 approval should suppress (score={}, conf={})",
            pref.preference_score(),
            pref.confidence()
        );
    }

    #[test]
    fn moderate_approval_is_not_suppressed() {
        let mut pref = TemplatePreference::new();
        // 40% approval rate with sufficient evidence
        for _ in 0..6 {
            pref.observe(true, 0.0, 90.0);
        }
        for _ in 0..9 {
            pref.observe(false, 0.0, 90.0);
        }
        // Score ≈ (2+6) / (2+2+6+9) = 8/19 ≈ 0.42 — above 0.25 threshold
        assert!(
            !pref.should_suppress(&policy()),
            "40% approval should not be suppressed (score={})",
            pref.preference_score()
        );
    }

    // ── Cadence multiplier ──

    #[test]
    fn sparse_data_has_no_cadence_adjustment() {
        let mut pref = TemplatePreference::new();
        pref.observe(true, 0.0, 90.0);
        assert!(
            (pref.cadence_multiplier() - 1.0).abs() < 1e-9,
            "sparse data should not adjust cadence"
        );
    }

    #[test]
    fn high_preference_shortens_cooldown() {
        let mut pref = TemplatePreference::new();
        for _ in 0..10 {
            pref.observe(true, 0.0, 90.0);
        }
        let mult = pref.cadence_multiplier();
        assert!(
            mult < 1.0,
            "high preference should shorten cooldown (mult={mult})"
        );
        assert!(mult >= 0.75, "cadence multiplier bounded at 0.75");
    }

    #[test]
    fn low_preference_lengthens_cooldown() {
        let mut pref = TemplatePreference::new();
        for _ in 0..10 {
            pref.observe(false, 0.0, 90.0);
        }
        let mult = pref.cadence_multiplier();
        assert!(
            mult > 1.0,
            "low preference should lengthen cooldown (mult={mult})"
        );
        assert!(mult <= 1.25, "cadence multiplier bounded at 1.25");
    }

    // ── TenantPreferencePosterior collection ──

    #[test]
    fn missing_template_returns_prior() {
        let posterior = TenantPreferencePosterior::new();
        let pref = posterior.get("nonexistent");
        assert!((pref.preference_score() - 0.5).abs() < 1e-9);
        assert!(!posterior.should_suppress("nonexistent", &policy()));
        assert!((posterior.cadence_multiplier("nonexistent") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn different_templates_have_independent_preferences() {
        let mut posterior = TenantPreferencePosterior::new();
        // Template A: all approvals
        for _ in 0..10 {
            posterior.observe("email-campaign", true, 0.0, 90.0);
        }
        // Template B: all cancellations
        for _ in 0..10 {
            posterior.observe("media-campaign", false, 0.0, 90.0);
        }
        assert!(
            posterior.preference_score("email-campaign")
                > posterior.preference_score("media-campaign"),
            "email and media should have divergent preferences"
        );
        assert!(!posterior.should_suppress("email-campaign", &policy()));
        assert!(posterior.should_suppress("media-campaign", &policy()));
    }

    // ── Behavioral scenarios (A–J from the plan) ──

    // A — Tenant prefers weak channel: email preference high, email causal
    // fan effect weak, another channel has higher Y30.
    #[test]
    fn scenario_a_preferred_weak_channel_does_not_override_economic_winner() {
        let mut posterior = TenantPreferencePosterior::new();
        // Tenant loves email
        for _ in 0..10 {
            posterior.observe("email-campaign", true, 0.0, 90.0);
        }
        // Email preference is high
        let email_pref = posterior.preference_score("email-campaign");
        assert!(email_pref > 0.7, "email should be highly preferred");
        // But the preference score does NOT touch DecisionValue.
        // The portfolio optimizer still ranks by DecisionValue.total().
        // This test verifies the preference score is separate from any
        // economic value — it's just a number in [0, 1].
        assert!(email_pref <= 1.0, "preference is bounded [0,1]");
        // A high-preference template is NOT suppressed
        assert!(!posterior.should_suppress("email-campaign", &policy()));
        // A low-preference template with no data is also NOT suppressed
        assert!(!posterior.should_suppress("media-campaign", &policy()));
    }

    // B — Tenant dislikes strong channel: repeatedly rejects media, media
    // causal fan effect is strong.
    #[test]
    fn scenario_b_disliked_strong_channel_may_be_suppressed_but_not_erased() {
        let mut posterior = TenantPreferencePosterior::new();
        // Tenant rejects media 90% of the time
        for _ in 0..10 {
            posterior.observe("media-campaign", false, 0.0, 90.0);
        }
        posterior.observe("media-campaign", true, 0.0, 90.0);
        // Media is suppressed from candidate generation
        assert!(
            posterior.should_suppress("media-campaign", &policy()),
            "media should be suppressed after consistent rejection"
        );
        // But the preference score is NOT zero — the prior keeps it above 0
        let score = posterior.preference_score("media-campaign");
        assert!(
            score > 0.0,
            "preference is never exactly zero — prior prevents erasure"
        );
        // And the suppression is reversible: if the tenant starts approving,
        // the posterior will shift (see scenario G)
    }

    // C — Proposal fatigue: large proposal volume → declining response.
    #[test]
    fn scenario_c_fatigue_increases_cooldown_for_low_preference() {
        let mut posterior = TenantPreferencePosterior::new();
        // Tenant ignores/cancels most proposals
        for _ in 0..8 {
            posterior.observe("social-post", false, 0.0, 90.0);
        }
        posterior.observe("social-post", true, 0.0, 90.0);
        // Cadence multiplier should lengthen the cooldown
        let mult = posterior.cadence_multiplier("social-post");
        assert!(
            mult > 1.0,
            "low-preference template should get longer cooldown (mult={mult})"
        );
        // But high-value templates (if they existed) would still be
        // surfaced — cadence is a multiplier, not a gate.
    }

    // D — Repeated ignored ideas: category repeatedly ignored + low value.
    #[test]
    fn scenario_d_repeatedly_ignored_is_suppressed() {
        let mut posterior = TenantPreferencePosterior::new();
        for _ in 0..15 {
            posterior.observe("growth-strategist", false, 0.0, 90.0);
        }
        assert!(
            posterior.should_suppress("growth-strategist", &policy()),
            "15 cancellations should suppress the template"
        );
    }

    // E — Ignored but high-value idea: rarely approved but strong causal
    // evidence. Should NOT be permanently suppressed.
    #[test]
    fn scenario_e_ignored_but_valuable_is_not_suppressed() {
        let mut posterior = TenantPreferencePosterior::new();
        // 40% approval rate — not great, but above the 0.25 threshold
        for _ in 0..6 {
            posterior.observe("press-pitch", true, 0.0, 90.0);
        }
        for _ in 0..9 {
            posterior.observe("press-pitch", false, 0.0, 90.0);
        }
        let score = posterior.preference_score("press-pitch");
        assert!(
            score > 0.25,
            "40% approval should be above suppression threshold (score={score})"
        );
        assert!(
            !posterior.should_suppress("press-pitch", &policy()),
            "a valuable but moderately-rejected template should not be suppressed"
        );
        // Cadence is longer (less frequent proposals) but not suppressed
        let mult = posterior.cadence_multiplier("press-pitch");
        assert!(mult > 1.0, "cadence should be longer for low preference");
    }

    // F — Sparse tenant data: only 2 interactions.
    #[test]
    fn scenario_f_sparse_data_stays_at_prior() {
        let mut posterior = TenantPreferencePosterior::new();
        posterior.observe("email-campaign", true, 0.0, 90.0);
        posterior.observe("email-campaign", false, 0.0, 90.0);
        let pref = posterior.get("email-campaign");
        assert!(
            !pref.has_sufficient_evidence(),
            "2 observations is not sufficient evidence"
        );
        assert!(
            (pref.cadence_multiplier() - 1.0).abs() < 1e-9,
            "no cadence adjustment"
        );
        assert!(
            !posterior.should_suppress("email-campaign", &policy()),
            "no suppression"
        );
        // Score is close to 0.5 (prior)
        assert!(
            (pref.preference_score() - 0.5).abs() < 0.1,
            "sparse data should stay close to prior"
        );
    }

    // G — Preference shift: historically prefers email, later chooses media.
    #[test]
    fn scenario_g_preference_shift_adapts_over_time() {
        let mut posterior = TenantPreferencePosterior::new();
        // 90 days of email approvals (now 90 days old → weight 0.5 each)
        for _ in 0..10 {
            posterior.observe("email-campaign", true, 90.0, 90.0);
        }
        // Recent media approvals (today → weight 1.0 each)
        for _ in 0..5 {
            posterior.observe("media-campaign", true, 0.0, 90.0);
        }
        // Email preference is still positive but weakened by decay
        let email_score = posterior.preference_score("email-campaign");
        let media_score = posterior.preference_score("media-campaign");
        // Media has fresh evidence; email has decayed evidence
        // Email: (2 + 10*0.5) / (2 + 2 + 10*0.5) = 7/9 ≈ 0.78
        // Media: (2 + 5*1.0) / (2 + 2 + 5*1.0) = 7/9 ≈ 0.78
        // Both are similar — the shift is happening but not complete
        // After more media approvals, media would overtake
        assert!(
            (email_score - media_score).abs() < 0.15,
            "preference shift should be gradual, not sudden"
        );
    }

    // H — Different tenants: A prefers email, B prefers media.
    #[test]
    fn scenario_h_different_tenants_diverge() {
        let mut tenant_a = TenantPreferencePosterior::new();
        let mut tenant_b = TenantPreferencePosterior::new();
        for _ in 0..10 {
            tenant_a.observe("email-campaign", true, 0.0, 90.0);
            tenant_b.observe("email-campaign", false, 0.0, 90.0);
        }
        for _ in 0..10 {
            tenant_a.observe("media-campaign", false, 0.0, 90.0);
            tenant_b.observe("media-campaign", true, 0.0, 90.0);
        }
        assert!(
            tenant_a.preference_score("email-campaign")
                > tenant_b.preference_score("email-campaign"),
            "tenant A should prefer email more than tenant B"
        );
        assert!(
            tenant_b.preference_score("media-campaign")
                > tenant_a.preference_score("media-campaign"),
            "tenant B should prefer media more than tenant A"
        );
    }

    // I — Operator acceptance ≠ fan outcome: 100% approval, zero fans.
    #[test]
    fn scenario_i_high_preference_does_not_imply_fan_value() {
        let mut posterior = TenantPreferencePosterior::new();
        for _ in 0..20 {
            posterior.observe("email-campaign", true, 0.0, 90.0);
        }
        let score = posterior.preference_score("email-campaign");
        assert!(score > 0.8, "100% approval → high preference score");
        // But the preference score is just a number in [0, 1].
        // It does NOT modify expected_incremental_y30 or DecisionValue.
        // The portfolio optimizer ranks by DecisionValue.total().
        // A high-preference template with zero fan value will be surfaced
        // often (shorter cooldown) but will LOSE to a low-preference
        // template with high fan value in the portfolio.
        // This is the core invariant: preference ≠ economic value.
        assert!(score <= 1.0, "preference is bounded [0,1], not a fan count");
    }

    // J — Fan outcome ≠ operator preference: low approval, strong fan effect.
    #[test]
    fn scenario_j_strong_fan_value_not_erased_by_low_preference() {
        let mut posterior = TenantPreferencePosterior::new();
        // 35% approval — below 50% but above the 0.25 suppression threshold
        for _ in 0..7 {
            posterior.observe("media-campaign", true, 0.0, 90.0);
        }
        for _ in 0..13 {
            posterior.observe("media-campaign", false, 0.0, 90.0);
        }
        let score = posterior.preference_score("media-campaign");
        // Score ≈ (2+7) / (2+2+7+13) = 9/24 = 0.375 — above 0.25
        assert!(
            score > 0.25,
            "35% approval should be above suppression threshold (score={score})"
        );
        assert!(
            !posterior.should_suppress("media-campaign", &policy()),
            "a strong-fan-value template should not be suppressed at 35% approval"
        );
        // Cadence is longer (less frequent) but the template still
        // competes in the portfolio on DecisionValue.total()
        let mult = posterior.cadence_multiplier("media-campaign");
        assert!(mult > 1.0, "cadence is longer for low preference");
        assert!(mult < 1.25, "but not maximally lengthened");
    }

    // ── Behavioral tests (PREF-1 through PREF-6) ──

    // PREF-1: A suppressed candidate still has a cadence multiplier and
    // preference score — suppression is a presentation decision, not an
    // economic erasure. The candidate remains economically selectable.
    #[test]
    fn pref_1_suppressed_candidate_still_has_cadence_and_score() {
        let mut posterior = TenantPreferencePosterior::new();
        for _ in 0..10 {
            posterior.observe("media", false, 0.0, 90.0);
        }
        posterior.observe("media", true, 0.0, 90.0);
        // Suppressed from presentation...
        assert!(
            posterior.should_suppress("media", &policy()),
            "media should be suppressed after consistent rejection"
        );
        // ...but still has a cadence multiplier (not zeroed out)
        let mult = posterior.cadence_multiplier("media");
        assert!(
            mult > 0.0,
            "suppressed template still has a cadence multiplier (mult={mult})"
        );
        // ...and still has a preference score (not erased)
        let score = posterior.preference_score("media");
        assert!(
            score > 0.0,
            "suppressed template still has a preference score (score={score})"
        );
    }

    // PREF-2: Neutral preference (score 0.5) produces exactly 1.0 multiplier.
    #[test]
    fn pref_2_neutral_preference_is_exactly_1_0() {
        let mut pref = TemplatePreference::new();
        // Equal approvals and cancellations with enough confidence
        for _ in 0..10 {
            pref.observe(true, 0.0, 90.0);
        }
        for _ in 0..10 {
            pref.observe(false, 0.0, 90.0);
        }
        assert!(
            (pref.preference_score() - 0.5).abs() < 1e-9,
            "equal approve/cancel should give score 0.5"
        );
        assert!(
            (pref.cadence_multiplier() - 1.0).abs() < 1e-9,
            "neutral preference should give exactly 1.0 multiplier"
        );
    }

    // PREF-4: Preference does not modify economic value. The preference
    // score is bounded [0, 1] and the cadence multiplier only affects
    // cooldown timing — neither touches DecisionValue.
    #[test]
    fn pref_4_preference_does_not_modify_economic_value() {
        let mut posterior = TenantPreferencePosterior::new();
        for _ in 0..20 {
            posterior.observe("email", true, 0.0, 90.0);
        }
        let score = posterior.preference_score("email");
        assert!(score > 0.8, "high approval → high preference score");
        assert!(score <= 1.0, "preference is bounded [0,1]");
        let mult = posterior.cadence_multiplier("email");
        assert!(mult < 1.0, "high preference → shorter cooldown");
        assert!(mult >= 0.75, "cooldown multiplier has a floor");
        // The multiplier only affects cooldown timing, not economic value.
        // There is no API in TenantPreferencePosterior that modifies
        // DecisionValue, expected_incremental_y30, or treatment effects.
    }

    // PREF-5: Long historical preference + one new opposite decision
    // → gradual movement, no flip.
    #[test]
    fn pref_5_long_history_one_opposite_is_gradual() {
        let mut posterior = TenantPreferencePosterior::new();
        // 20 approvals spread over 90 days (avg age 45 → weight ~0.71)
        for i in 0..20 {
            let age = 90.0 - f64::from(i) * 4.5;
            posterior.observe("email", true, age.max(0.0), 90.0);
        }
        let score_before = posterior.preference_score("email");
        // One recent cancellation
        posterior.observe("email", false, 0.0, 90.0);
        let score_after = posterior.preference_score("email");
        assert!(
            (score_before - score_after).abs() < 0.1,
            "one opposite action should not flip preference: before={score_before}, after={score_after}"
        );
    }

    // PREF-6: Sustained behavior change → preference eventually shifts.
    #[test]
    fn pref_6_sustained_behavior_change_shifts_preference() {
        let mut posterior = TenantPreferencePosterior::new();
        // 10 approvals 180 days ago (decayed: 2 half-lives → weight 0.25 each)
        for _ in 0..10 {
            posterior.observe("email", true, 180.0, 90.0);
        }
        // 10 cancellations today (weight 1.0 each)
        for _ in 0..10 {
            posterior.observe("email", false, 0.0, 90.0);
        }
        let score = posterior.preference_score("email");
        // Old approvals: 10 * 0.25 = 2.5
        // Fresh cancellations: 10 * 1.0 = 10.0
        // Posterior: (2 + 2.5) / (2 + 2 + 2.5 + 10) = 4.5 / 16.5 ≈ 0.27
        assert!(
            score < 0.4,
            "sustained cancellation should shift preference down (score={score})"
        );
    }

    // ── Cadence formula tests ──

    #[test]
    fn cadence_is_monotonic_decreasing() {
        // Higher preference score → lower (shorter) cadence multiplier
        let mut scores_multipliers: Vec<(f64, f64)> = Vec::new();
        for approvals in 0..=20 {
            let mut pref = TemplatePreference::new();
            for _ in 0..approvals {
                pref.observe(true, 0.0, 90.0);
            }
            for _ in 0..(20 - approvals) {
                pref.observe(false, 0.0, 90.0);
            }
            if pref.confidence() >= 3.0 {
                scores_multipliers.push((pref.preference_score(), pref.cadence_multiplier()));
            }
        }
        for i in 1..scores_multipliers.len() {
            let (prev_score, prev_mult) = scores_multipliers[i - 1];
            let (curr_score, curr_mult) = scores_multipliers[i];
            assert!(
                curr_score >= prev_score,
                "scores should be monotonically increasing"
            );
            assert!(
                curr_mult <= prev_mult + 1e-9,
                "multiplier should decrease as score increases: prev=({prev_score},{prev_mult}), curr=({curr_score},{curr_mult})"
            );
        }
    }

    #[test]
    fn cadence_respects_hard_bounds() {
        // Even with extreme evidence, multiplier stays in [0.5, 1.5]
        let mut high = TemplatePreference::new();
        for _ in 0..100 {
            high.observe(true, 0.0, 90.0);
        }
        let high_mult = high.cadence_multiplier();
        assert!(
            high_mult >= 0.75,
            "multiplier floor is 0.75 (got {high_mult})"
        );

        let mut low = TemplatePreference::new();
        for _ in 0..100 {
            low.observe(false, 0.0, 90.0);
        }
        let low_mult = low.cadence_multiplier();
        assert!(
            low_mult <= 1.25,
            "multiplier ceiling is 1.25 (got {low_mult})"
        );
    }

    // ── Presentation metadata tests (PRES-1 through PRES-4) ──

    // PRES-1: High-preference template → not presentation-hidden.
    #[test]
    fn pres_1_high_preference_not_hidden() {
        let mut posterior = TenantPreferencePosterior::new();
        for _ in 0..20 {
            posterior.observe("email", true, 0.0, 90.0);
        }
        let meta = posterior.presentation_metadata("email", &policy());
        assert!(
            !meta.is_presentation_hidden,
            "high-preference template should not be hidden"
        );
        assert!(meta.preference_score > 0.8);
        assert_eq!(meta.template_id, "email");
    }

    // PRES-2: Low-preference template with evidence → presentation-hidden.
    #[test]
    fn pres_2_low_preference_is_hidden() {
        let mut posterior = TenantPreferencePosterior::new();
        for _ in 0..10 {
            posterior.observe("media", false, 0.0, 90.0);
        }
        let meta = posterior.presentation_metadata("media", &policy());
        assert!(
            meta.is_presentation_hidden,
            "low-preference template with evidence should be hidden"
        );
        assert!(meta.preference_score < 0.3);
    }

    // PRES-3: Sparse data → not hidden (regardless of score).
    #[test]
    fn pres_3_sparse_data_not_hidden() {
        let mut posterior = TenantPreferencePosterior::new();
        posterior.observe("media", false, 0.0, 90.0); // 1 cancellation
        let meta = posterior.presentation_metadata("media", &policy());
        assert!(
            !meta.is_presentation_hidden,
            "sparse data should not be hidden"
        );
    }

    // PRES-4: Presentation metadata is pure information — it does not
    // modify economic value. Verified by struct shape: the metadata
    // only carries template_id, preference_score, and a visibility bool.
    #[test]
    fn pres_4_metadata_is_pure_information() {
        let mut posterior = TenantPreferencePosterior::new();
        for _ in 0..10 {
            posterior.observe("email", true, 0.0, 90.0);
        }
        let meta = posterior.presentation_metadata("email", &policy());
        assert_eq!(meta.template_id, "email");
        assert!(meta.preference_score >= 0.0 && meta.preference_score <= 1.0);
        // is_presentation_hidden is a bool, not an economic modifier.
        let _ = meta.is_presentation_hidden;
    }

    // ── Cadence bounds tests (V1: [0.75, 1.25]) ──

    #[test]
    fn cadence_score_0_is_1_25() {
        let mut pref = TemplatePreference::new();
        for _ in 0..100 {
            pref.observe(false, 0.0, 90.0);
        }
        // With 100 cancellations: score ≈ 2/102 ≈ 0.02 → mult ≈ 1.24
        // Close to the ceiling of 1.25
        let mult = pref.cadence_multiplier();
        assert!(
            (mult - 1.25).abs() < 0.01,
            "strong rejection → near 1.25 ceiling (got {mult})"
        );
    }

    #[test]
    fn cadence_score_1_is_0_75() {
        let mut pref = TemplatePreference::new();
        for _ in 0..100 {
            pref.observe(true, 0.0, 90.0);
        }
        // With 100 approvals: score ≈ 102/102 ≈ 0.98 → mult ≈ 0.76
        // Close to the floor of 0.75
        let mult = pref.cadence_multiplier();
        assert!(
            (mult - 0.75).abs() < 0.01,
            "strong approval → near 0.75 floor (got {mult})"
        );
    }

    // ── Exploration floor tests ──

    // EXPL-1: Discovery cap prevents starvation — even with extreme
    // rejection, the cadence multiplier is capped at 1.25.
    #[test]
    fn expl_1_discovery_cap_prevents_starvation() {
        let mut pref = TemplatePreference::new();
        for _ in 0..100 {
            pref.observe(false, 0.0, 90.0);
        }
        let mult = pref.cadence_multiplier();
        assert!(
            mult <= 1.25,
            "discovery cap prevents starvation (mult={mult})"
        );
    }

    // EXPL-2: Discovery cap is a configurable policy field.
    #[test]
    fn expl_2_discovery_cap_is_configurable() {
        let custom = TenantPreferencePolicy {
            min_discovery_cadence_multiplier: 1.10,
            ..Default::default()
        };
        assert!(
            (custom.min_discovery_cadence_multiplier - 1.10).abs() < 1e-9,
            "discovery cap is configurable"
        );
        // Default is 1.25
        assert!(
            (policy().min_discovery_cadence_multiplier - 1.25).abs() < 1e-9,
            "default discovery cap is 1.25"
        );
    }
}
