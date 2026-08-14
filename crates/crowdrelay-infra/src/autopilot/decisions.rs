//! Split PostgreSQL Autopilot adapter implementation.

use super::*;

include!("decisions/core_reads.rs");
include!("decisions/opportunity_reads.rs");
include!("decisions/persist.rs");

#[async_trait]
impl AutopilotDecisionRepository for PostgresAutopilotRepository {
    decision_core_reads!();
    decision_opportunity_reads!();
    decision_persist!();
}
