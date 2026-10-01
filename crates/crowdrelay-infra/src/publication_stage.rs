//! The publication measurement lifecycle as one operator-readable stage.
//!
//! A post and its content measurements tell six different stories an
//! operator must tell apart: still waiting on publication, live but
//! uninstrumented, window still maturing, matured to a real zero, matured
//! with conversions, and terminal states where the post never went out at
//! all. None of them is "succeeded" or "resolved" — the point of the stage
//! is that an unpublished draft and a published post that earned nobody are
//! opposite facts.
//!
//! "Outcome accepted by the learner" is not a stage of its own: the outcome
//! row commits in the same transaction as `succeeded`, so both terminal
//! measured stages already carry it. Callers surface it as the
//! `outcome_accepted` boolean beside the stage — true iff an
//! `autopilot_outcomes` row exists for the measurement.
//!
//! The vocabulary is shared by the post-queue lane aggregates and the video
//! scorecard, so it is defined once here as a SQL fragment and a matching
//! Rust enum. The fragment reads fixed aliases — `post` (any of the four
//! post ledgers), `measurement` (`autopilot_measurements`), `outcome`
//! (`autopilot_outcomes`, left-joined) — callers embed it in their own
//! FROM shapes.

/// SQL CASE producing the stage text. Required aliases:
///   `post.status`, `post.posted_at` — the publication row;
///   `measurement.status`, `measurement.last_error_kind` — the measurement row;
///   `outcome.observed_value` — NULL while unmeasured.
pub const PUBLICATION_STAGE_SQL: &str = r#"
    CASE
        WHEN post.posted_at IS NULL
             AND post.status IN ('pending', 'posting', 'rate_limited',
                                 'awaiting_manual_post')
            THEN 'awaiting_publication'
        WHEN post.posted_at IS NULL
            THEN 'never_published'
        WHEN measurement.status = 'failed'
             AND measurement.last_error_kind = 'no_tracked_link'
            THEN 'no_tracked_link'
        WHEN measurement.status = 'failed'
            THEN 'measurement_failed'
        WHEN measurement.status IS NULL
            THEN 'no_measurement'
        WHEN measurement.status IN ('pending', 'processing')
            THEN 'maturing'
        WHEN measurement.status = 'succeeded'
             AND COALESCE(outcome.observed_value, 0.0) > 0.0
            THEN 'conversions'
        WHEN measurement.status = 'succeeded'
            THEN 'mature_zero'
        ELSE 'measurement_failed'
    END
"#;

/// The stage vocabulary [`PUBLICATION_STAGE_SQL`] produces — kept in Rust so
/// consumers can match exhaustively instead of comparing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationMeasurementStage {
    /// Post exists but has not gone live (pending, posting, rate-limited,
    /// awaiting manual publication). Its measurements wait with it.
    AwaitingPublication,
    /// The post can never publish — failed or cancelled. Distinct from a
    /// measured zero: the content never reached an audience at all.
    NeverPublished,
    /// Published but the draft carried no tracked link — unmeasurable, not
    /// zero. The instrument was missing, the audience was not asked.
    NoTrackedLink,
    /// The measurement was claimed and could not be read (retries exhausted,
    /// unsupported kind). A broken measurement, not a broken post.
    MeasurementFailed,
    /// Published; the measurement row the scheduler was supposed to write
    /// is absent. A pipeline gap to alarm on, never a zero.
    NoMeasurement,
    /// Published and instrumented; the seven-day window is still open or
    /// the claim has not landed yet.
    Maturing,
    /// The window closed and the canonical ledger credited this post with
    /// signups.
    Conversions,
    /// The window closed and the measured answer is honestly zero — an
    /// outcome the learner must see, not a gap.
    MatureZero,
}

impl PublicationMeasurementStage {
    /// Parses the SQL fragment's text back into the enum. `None` for a
    /// string outside the vocabulary — a drift between the fragment and
    /// this type must surface, not silently classify as something else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "awaiting_publication" => Some(Self::AwaitingPublication),
            "never_published" => Some(Self::NeverPublished),
            "no_tracked_link" => Some(Self::NoTrackedLink),
            "measurement_failed" => Some(Self::MeasurementFailed),
            "no_measurement" => Some(Self::NoMeasurement),
            "maturing" => Some(Self::Maturing),
            "conversions" => Some(Self::Conversions),
            "mature_zero" => Some(Self::MatureZero),
            _ => None,
        }
    }

    /// Stable label — identical to the SQL fragment's output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingPublication => "awaiting_publication",
            Self::NeverPublished => "never_published",
            Self::NoTrackedLink => "no_tracked_link",
            Self::MeasurementFailed => "measurement_failed",
            Self::NoMeasurement => "no_measurement",
            Self::Maturing => "maturing",
            Self::Conversions => "conversions",
            Self::MatureZero => "mature_zero",
        }
    }
}
