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
use crowdrelay_domain::WorkspaceId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use database_runtime::{DatabaseRuntimeRow, DatabaseRuntimeSummary};

use crate::{
    IDEMPOTENCY_KEY, Problem,
    ops_summary::{QueueSummary, WatchdogSummary},
    request_id,
};

const PRIVATE_NO_STORE: &str = "private, no-store";
const DEFAULT_PAGE_SIZE: i64 = 50;
const MAX_PAGE_SIZE: i64 = 100;

include!("ops/models.rs");

include!("ops_timeline.rs");
include!("ops_action_ledger.rs");
include!("ops/handlers.rs");
include!("ops/attention.rs");

include!("ops/query_support.rs");
