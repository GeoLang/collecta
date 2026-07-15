//! # collecta-server
//!
//! REST API server for Collecta — form management, submission ingestion,
//! and sync endpoints. State is persisted to sqlite (see [`store::Store`]).

pub mod store;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use collecta_core::form::Form;
use collecta_core::submission::Submission;
use collecta_core::validation;

use store::Store;

/// Build the router over an already-open [`Store`].
pub fn router(store: Store) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/forms", get(list_forms).post(create_form))
        .route("/api/v1/forms/import", post(import_form))
        .route("/api/v1/forms/{form_id}", get(get_form))
        .route(
            "/api/v1/forms/{form_id}/submissions",
            get(list_submissions).post(submit),
        )
        .route("/api/v1/sync/status", get(sync_status))
        .with_state(store)
}

/// Build the router, opening the database at `$COLLECTA_DB` (default `./collecta.db`).
pub async fn app() -> Router {
    let db_path = std::env::var("COLLECTA_DB").unwrap_or_else(|_| "./collecta.db".to_string());
    let store = Store::connect(&db_path)
        .await
        .expect("failed to open collecta database");
    router(store)
}

async fn health() -> &'static str {
    "ok"
}

async fn list_forms(State(store): State<Store>) -> Result<Json<Vec<FormSummary>>, ApiError> {
    let forms = store.list_forms().await?;
    Ok(Json(forms.iter().map(FormSummary::from).collect()))
}

async fn create_form(
    State(store): State<Store>,
    Json(form): Json<Form>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let id = form.id;
    store.insert_form(&form).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

/// Import an XLSForm `.xlsx` (raw request body) and register the resulting form.
async fn import_form(
    State(store): State<Store>,
    body: Bytes,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let form = collecta_xlsform::parse_bytes(&body)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let id = form.id;
    store.insert_form(&form).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn get_form(
    State(store): State<Store>,
    Path(form_id): Path<Uuid>,
) -> Result<Json<Form>, ApiError> {
    store
        .get_form(form_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "form not found".to_string()))
}

async fn list_submissions(
    State(store): State<Store>,
    Path(form_id): Path<Uuid>,
) -> Result<Json<Vec<Submission>>, ApiError> {
    Ok(Json(store.list_submissions(form_id).await?))
}

async fn submit(
    State(store): State<Store>,
    Path(form_id): Path<Uuid>,
    Json(submission): Json<Submission>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let form = store
        .get_form(form_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "form not found".to_string()))?;

    let errors = validation::validate(&form, &submission);
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, msg));
    }

    let id = submission.id;
    store.insert_submission(&submission).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn sync_status(State(store): State<Store>) -> Result<Json<SyncStatusResponse>, ApiError> {
    let counts = store.sync_counts().await?;
    Ok(Json(SyncStatusResponse {
        pending: counts.pending,
        synced: counts.synced,
        failed: counts.failed,
        abandoned: counts.abandoned,
        total: counts.total,
    }))
}

/// Error carrying an HTTP status and message; storage errors map to 500.
struct ApiError(StatusCode, String);

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, self.1).into_response()
    }
}

#[derive(Serialize)]
struct FormSummary {
    id: Uuid,
    title: String,
    version: u32,
    field_count: usize,
}

impl From<&Form> for FormSummary {
    fn from(f: &Form) -> Self {
        FormSummary {
            id: f.id,
            title: f.title.clone(),
            version: f.version,
            field_count: f.fields.len(),
        }
    }
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
