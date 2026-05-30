//! # collecta-server
//!
//! REST API server for Collecta — form management, submission ingestion,
//! and sync endpoints.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use collecta_core::form::Form;
use collecta_core::submission::Submission;
use collecta_core::sync_queue::{SyncQueue, SyncStatus};
use collecta_core::validation;

/// Shared application state.
struct AppState {
    forms: RwLock<Vec<Form>>,
    submissions: RwLock<Vec<Submission>>,
    queue: RwLock<SyncQueue>,
}

/// Build the Axum router.
pub fn app() -> Router {
    let state = Arc::new(AppState {
        forms: RwLock::new(Vec::new()),
        submissions: RwLock::new(Vec::new()),
        queue: RwLock::new(SyncQueue::new()),
    });

    Router::new()
        .route("/health", get(health))
        .route("/api/v1/forms", get(list_forms).post(create_form))
        .route("/api/v1/forms/{form_id}", get(get_form))
        .route(
            "/api/v1/forms/{form_id}/submissions",
            get(list_submissions).post(submit),
        )
        .route("/api/v1/sync/status", get(sync_status))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn list_forms(State(state): State<Arc<AppState>>) -> Json<Vec<FormSummary>> {
    let forms = state.forms.read().await;
    let summaries: Vec<FormSummary> = forms
        .iter()
        .map(|f| FormSummary {
            id: f.id,
            title: f.title.clone(),
            version: f.version,
            field_count: f.fields.len(),
        })
        .collect();
    Json(summaries)
}

async fn create_form(
    State(state): State<Arc<AppState>>,
    Json(form): Json<Form>,
) -> (StatusCode, Json<IdResponse>) {
    let id = form.id;
    state.forms.write().await.push(form);
    (StatusCode::CREATED, Json(IdResponse { id }))
}

async fn get_form(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(form_id): axum::extract::Path<Uuid>,
) -> Result<Json<Form>, StatusCode> {
    let forms = state.forms.read().await;
    forms
        .iter()
        .find(|f| f.id == form_id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn list_submissions(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(form_id): axum::extract::Path<Uuid>,
) -> Json<Vec<Submission>> {
    let subs = state.submissions.read().await;
    let filtered: Vec<Submission> = subs
        .iter()
        .filter(|s| s.form_id == form_id)
        .cloned()
        .collect();
    Json(filtered)
}

async fn submit(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(form_id): axum::extract::Path<Uuid>,
    Json(submission): Json<Submission>,
) -> Result<(StatusCode, Json<IdResponse>), (StatusCode, String)> {
    // Find form
    let forms = state.forms.read().await;
    let form = forms
        .iter()
        .find(|f| f.id == form_id)
        .ok_or((StatusCode::NOT_FOUND, "form not found".to_string()))?;

    // Validate
    let errors = validation::validate(form, &submission);
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err((StatusCode::UNPROCESSABLE_ENTITY, msg));
    }
    drop(forms);

    let id = submission.id;
    state.submissions.write().await.push(submission);

    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn sync_status(State(state): State<Arc<AppState>>) -> Json<SyncStatusResponse> {
    let queue = state.queue.read().await;
    Json(SyncStatusResponse {
        pending: queue.count_by_status(SyncStatus::Pending),
        synced: queue.count_by_status(SyncStatus::Synced),
        failed: queue.count_by_status(SyncStatus::Failed),
        abandoned: queue.count_by_status(SyncStatus::Abandoned),
        total: queue.len(),
    })
}

#[derive(Serialize)]
struct FormSummary {
    id: Uuid,
    title: String,
    version: u32,
    field_count: usize,
}

#[derive(Serialize, Deserialize)]
struct IdResponse {
    id: Uuid,
}

#[derive(Serialize)]
struct SyncStatusResponse {
    pending: usize,
    synced: usize,
    failed: usize,
    abandoned: usize,
    total: usize,
}
