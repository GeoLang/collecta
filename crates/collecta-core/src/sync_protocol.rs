//! Wire types for the client/server sync protocol.
//!
//! Shared by the server endpoints and the client sync queue so the two
//! sides cannot drift. Push is idempotent on the client-generated
//! submission id: re-pushing a batch yields `Duplicate` results, never
//! duplicate rows.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::form::Form;
use crate::submission::Submission;

/// Body of `POST /api/v1/sync/push`: a batch of queued submissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    pub submissions: Vec<Submission>,
}

/// Per-item outcome of a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushItemStatus {
    /// Stored for the first time.
    Accepted,
    /// Already stored (same submission id); nothing changed.
    Duplicate,
    /// Rejected — see `message`.
    Error,
}

/// Result for one submission in a push batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushItemResult {
    pub id: Uuid,
    pub status: PushItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Response of `POST /api/v1/sync/push`, one result per pushed submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    pub results: Vec<PushItemResult>,
}

/// Response of `GET /api/v1/sync/forms?since=<cursor>`.
///
/// `cursor` is an opaque string the client stores and sends back as `since`
/// on the next pull to receive only forms updated afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormsPullResponse {
    pub forms: Vec<Form>,
    pub cursor: String,
}
