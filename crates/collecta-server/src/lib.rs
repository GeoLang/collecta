//! # collecta-server
//!
//! REST API server for Collecta — form management, submission ingestion,
//! and sync endpoints. State is persisted to sqlite (see [`store::Store`]).
//!
//! All data endpoints require a bearer JWT issued by `POST /api/v1/auth/login`;
//! only `/health` and login itself are public. Users are admin-seeded via the
//! `create-user` CLI subcommand, there is no signup endpoint.

pub mod auth;
pub mod store;

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::Json;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use collecta_core::form::Form;
use collecta_core::submission::Submission;
use collecta_core::sync_protocol::{
    FormsPullResponse, PushItemResult, PushItemStatus, PushRequest, PushResponse,
};
use collecta_core::validation;

use store::Store;

/// Shared handler state: the store plus the JWT signing secret.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub jwt_secret: Arc<str>,
}

/// Build the router over an already-open [`Store`].
///
/// Panics if `jwt_secret` is shorter than 32 bytes: HS256 with a short
/// secret is brute-forceable, and this is the single construction path.
pub fn router(store: Store, jwt_secret: &str) -> Router {
    assert!(
        jwt_secret.len() >= 32,
        "jwt secret must be at least 32 bytes"
    );
    let state = AppState {
        store,
        jwt_secret: Arc::from(jwt_secret),
    };
    let protected = Router::new()
        .route("/api/v1/forms", get(list_forms).post(create_form))
        .route("/api/v1/forms/import", post(import_form))
        .route("/api/v1/forms/{form_id}", get(get_form))
        .route(
            "/api/v1/forms/{form_id}/submissions",
            get(list_submissions).post(submit),
        )
        .route("/api/v1/sync/status", get(sync_status))
        .route("/api/v1/sync/push", post(sync_push))
        .route("/api/v1/sync/forms", get(sync_forms))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/auth/login", post(auth::login))
        .merge(protected)
        .with_state(state)
}

/// Build the router, opening the database at `$COLLECTA_DB` (default
/// `./collecta.db`) and signing tokens with `$COLLECTA_JWT_SECRET` (required).
pub async fn app() -> Router {
    let jwt_secret = std::env::var("COLLECTA_JWT_SECRET")
        .expect("COLLECTA_JWT_SECRET must be set (32+ random bytes)");
    let store = open_store().await;
    router(store, &jwt_secret)
}

/// Open the database at `$COLLECTA_DB` (default `./collecta.db`).
pub async fn open_store() -> Store {
    let db_path = std::env::var("COLLECTA_DB").unwrap_or_else(|_| "./collecta.db".to_string());
    Store::connect(&db_path)
        .await
        .expect("failed to open collecta database")
}

async fn health() -> &'static str {
    "ok"
}

async fn list_forms(State(state): State<AppState>) -> Result<Json<Vec<FormSummary>>, ApiError> {
    let forms = state.store.list_forms().await?;
    Ok(Json(forms.iter().map(FormSummary::from).collect()))
}

async fn create_form(
    State(state): State<AppState>,
    Json(form): Json<Form>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let id = form.id;
    state.store.insert_form(&form).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

/// Import an XLSForm `.xlsx` (raw request body) and register the resulting form.
async fn import_form(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let form = collecta_xlsform::parse_bytes(&body)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let id = form.id;
    state.store.insert_form(&form).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn get_form(
    State(state): State<AppState>,
    Path(form_id): Path<Uuid>,
) -> Result<Json<Form>, ApiError> {
    state
        .store
        .get_form(form_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "form not found".to_string()))
}

async fn list_submissions(
    State(state): State<AppState>,
    Path(form_id): Path<Uuid>,
) -> Result<Json<Vec<Submission>>, ApiError> {
    Ok(Json(state.store.list_submissions(form_id).await?))
}

async fn submit(
    State(state): State<AppState>,
    Path(form_id): Path<Uuid>,
    Json(submission): Json<Submission>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let form = state
        .store
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
    state.store.insert_submission(&submission).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn sync_status(State(state): State<AppState>) -> Result<Json<SyncStatusResponse>, ApiError> {
    let counts = state.store.sync_counts().await?;
    Ok(Json(SyncStatusResponse {
        pending: counts.pending,
        synced: counts.synced,
        failed: counts.failed,
        abandoned: counts.abandoned,
        total: counts.total,
    }))
}

/// Batch upload of queued submissions, idempotent on submission id.
/// Items are processed independently; each gets its own result.
async fn sync_push(
    State(state): State<AppState>,
    Json(request): Json<PushRequest>,
) -> Result<Json<PushResponse>, ApiError> {
    let mut results = Vec::with_capacity(request.submissions.len());
    for submission in &request.submissions {
        results.push(push_one(&state.store, submission).await);
    }
    Ok(Json(PushResponse { results }))
}

async fn push_one(store: &Store, submission: &Submission) -> PushItemResult {
    let error = |message: String| PushItemResult {
        id: submission.id,
        status: PushItemStatus::Error,
        message: Some(message),
    };
    let form = match store.get_form(submission.form_id).await {
        Ok(Some(form)) => form,
        Ok(None) => return error(format!("unknown form {}", submission.form_id)),
        Err(e) => return error(e.to_string()),
    };
    let errors = validation::validate(&form, submission);
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return error(msg);
    }
    match store.insert_submission_if_new(submission).await {
        Ok(true) => PushItemResult {
            id: submission.id,
            status: PushItemStatus::Accepted,
            message: None,
        },
        Ok(false) => PushItemResult {
            id: submission.id,
            status: PushItemStatus::Duplicate,
            message: None,
        },
        Err(e) => error(e.to_string()),
    }
}

#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: String,
}

/// Form definitions updated since the client's cursor (all forms when absent).
async fn sync_forms(
    State(state): State<AppState>,
    Query(query): Query<SinceQuery>,
) -> Result<Json<FormsPullResponse>, ApiError> {
    let (forms, cursor) = state.store.list_forms_since(&query.since).await?;
    Ok(Json(FormsPullResponse {
        forms,
        cursor: cursor.unwrap_or(query.since),
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
