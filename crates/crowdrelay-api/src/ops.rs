//! Administrative operations visibility and audited recovery actions.
//!
//! The control plane intentionally exposes metadata only: event payloads,
//! signing material, endpoint URLs, and fan data never leave this module.

mod database_runtime;

use std::{future::Future, time::Duration};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_application::autopilot::AutopilotControlRepository;
use crowdrelay_domain::{WorkspaceId, action_ledger::ActionState};
use crowdrelay_infra::lapsed_approvals::LapsedApprovals;
use crowdrelay_infra::sent_record::FailedSends;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use database_runtime::{DatabaseRuntimeRow, DatabaseRuntimeSummary};

use crate::{
    IDEMPOTENCY_KEY, Problem,
    ops_summary::{QueueSummary, WatchdogSummary, WorkerSummary},
    request_id,
};

const PRIVATE_NO_STORE: &str = "private, no-store";
const DEFAULT_PAGE_SIZE: i64 = 50;
const MAX_PAGE_SIZE: i64 = 100;

include!("ops/models.rs");

include!("ops_timeline.rs");
include!("ops/trace_timeline_pg_tests.rs");
include!("ops/action_states_pg_tests.rs");
use crowdrelay_application::self_assessment::{DailyNorthStar, assess};

include!("ops_action_ledger.rs");
include!("ops/handlers.rs");
include!("ops/attention.rs");
include!("ops/post_queues.rs");
include!("ops/post_queues_pg_tests.rs");
include!("ops/funnel.rs");
include!("ops/funnel_pg_tests.rs");
include!("ops/outcomes.rs");
include!("ops/outcomes_pg_tests.rs");
include!("ops/goal.rs");
include!("ops/goal_pg_tests.rs");
include!("ops/hooks.rs");
include!("ops/intelligence.rs");
include!("ops/processes.rs");

include!("ops/fan_out.rs");
include!("ops/fan_sources.rs");
include!("ops/query_support.rs");
include!("ops/metrics_snapshot.rs");
