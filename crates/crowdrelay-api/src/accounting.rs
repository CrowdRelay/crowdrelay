//! Administrative ticket accounting for Polish monthly sales reporting.
//!
//! The API exposes a preview, an immutable finalized WEW snapshot, a universal
//! semicolon-delimited CSV, and separate invoice-request data. It deliberately
//! does not pretend to submit documents to KSeF or Saldeo; those systems remain
//! the accounting system of record.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, Postgres, Transaction};
use time::{Date, Duration, Month, OffsetDateTime};
use tokio::time::timeout;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_DOCUMENT_NUMBER_CHARS: usize = 100;
const MAX_PROFILE_TEXT_CHARS: usize = 240;

include!("accounting/models.rs");
include!("accounting/handlers.rs");
include!("accounting/core.rs");
include!("accounting/csv_support.rs");
