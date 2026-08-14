//! # collecta-server
//!
//! REST API server for Collecta — form management, submission ingestion,
//! and sync endpoints. State is persisted to sqlite (see [`store::Store`]).
//!
//! All data endpoints require a bearer JWT issued by `POST /api/v1/auth/login`;
//! only `/health` and login itself are public. Users are admin-seeded via the
//! `create-user` CLI subcommand, there is no signup endpoint.
//!
//! On top of that, each account carries a role ([`auth::Role`]): admins can do
//! everything, editors can create forms and submit, viewers can only read. Form
//! discovery is open to any authenticated account because collectors need it,
//! but a form's submissions are only readable by whoever created the form, by
//! an admin, and by any account the creator granted read on that one form.
//!
//! Every accepted submission carries the account that filed it in its
//! `collector_id`, taken from the caller's credentials on both surfaces. A row
//! without one predates the recording; nothing stored since can lack it.
//!
//! [`openrosa`] adds a second, Basic-authenticated surface at the server root
//! for ODK Collect. The two share the users table and nothing else.

pub mod attachment;
pub mod auth;
pub mod openrosa;
pub mod store;

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use collecta_core::form::Form;
use collecta_core::submission::Submission;
use collecta_core::sync_protocol::{
    FormsPullResponse, PushItemResult, PushItemStatus, PushRequest, PushResponse,
};
use collecta_core::validation;

use auth::Caller;
use store::{FormOwner, FormWriter, Store};

/// Server settings that are not the database.
pub struct Config {
    /// HS256 signing secret for the JSON API's JWTs. Must be 32+ bytes.
    pub jwt_secret: String,
    /// Root for server-written blobs; OpenRosa attachments live under it.
    pub data_dir: PathBuf,
    /// Absolute origin to advertise in OpenRosa `downloadUrl`s. When unset the
    /// URL is derived from each request.
    pub base_url: Option<String>,
}

impl Config {
    /// Config with a throwaway data directory, for tests and for callers that
    /// do not use the OpenRosa routes.
    pub fn new(jwt_secret: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            jwt_secret: jwt_secret.into(),
            data_dir: data_dir.into(),
            base_url: None,
        }
    }
}

/// Shared handler state: the store, the JWT signing secret, and where blobs go.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub jwt_secret: Arc<str>,
    pub data_dir: Arc<FsPath>,
    pub base_url: Option<Arc<str>>,
}

/// Build the router over an already-open [`Store`].
///
/// Panics if the JWT secret is shorter than 32 bytes: HS256 with a short
/// secret is brute-forceable, and this is the single construction path.
pub fn router(store: Store, config: Config) -> Router {
    assert!(
        config.jwt_secret.len() >= 32,
        "jwt secret must be at least 32 bytes"
    );
    let state = AppState {
        store,
        jwt_secret: Arc::from(config.jwt_secret.as_str()),
        data_dir: Arc::from(config.data_dir.as_path()),
        base_url: config.base_url.as_deref().map(Arc::from),
    };
    // form discovery, open to any authenticated account, plus the routes whose
    // handlers narrow to the form's creator, its grantees, and admins.
    let read = Router::new()
        .route("/api/v1/forms", get(list_forms))
        .route("/api/v1/forms/{form_id}", get(get_form))
        .route("/api/v1/forms/{form_id}/submissions", get(list_submissions))
        .route("/api/v1/forms/{form_id}/grants", get(list_grants))
        .route("/api/v1/attachments/{attachment_id}", get(get_attachment))
        .route("/api/v1/sync/forms", get(sync_forms));
    let write = Router::new()
        .route("/api/v1/forms", post(create_form))
        .route("/api/v1/forms/import", post(import_form))
        .route("/api/v1/forms/{form_id}", delete(delete_form))
        .route("/api/v1/forms/{form_id}/submissions", post(submit))
        .route(
            "/api/v1/forms/{form_id}/submissions/{submission_id}",
            delete(delete_submission),
        )
        .route("/api/v1/forms/{form_id}/grants", post(create_grant))
        .route(
            "/api/v1/forms/{form_id}/grants/{user_id}",
            delete(revoke_grant),
        )
        .route("/api/v1/sync/push", post(sync_push))
        .route_layer(middleware::from_fn(auth::require_write));
    // instance-wide counts, so there is no creator to compare against.
    let admin = Router::new()
        .route("/api/v1/sync/status", get(sync_status))
        .route_layer(middleware::from_fn(auth::require_admin));
    let protected = read
        .merge(write)
        .merge(admin)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/auth/login", post(auth::login))
        .merge(protected)
        .merge(openrosa::router(state.clone()))
        .with_state(state)
}

/// Build the router, opening the database at `$COLLECTA_DB` (default
/// `./collecta.db`) and signing tokens with `$COLLECTA_JWT_SECRET` (required).
pub async fn app() -> Router {
    let store = open_store().await;
    router(store, config_from_env())
}

/// Read [`Config`] from the environment. Panics when the JWT secret is absent
/// or empty: there is no unauthenticated fallback mode, and compose-style
/// `${VAR:-}` interpolation hands an unset variable through as empty.
pub fn config_from_env() -> Config {
    Config {
        jwt_secret: std::env::var("COLLECTA_JWT_SECRET")
            .ok()
            .filter(|secret| !secret.is_empty())
            .expect("COLLECTA_JWT_SECRET must be set (32+ random bytes)"),
        data_dir: std::env::var("COLLECTA_DATA_DIR")
            .unwrap_or_else(|_| "./collecta-data".to_string())
            .into(),
        base_url: std::env::var("COLLECTA_BASE_URL").ok(),
    }
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
    Extension(caller): Extension<Caller>,
    Json(form): Json<Form>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let id = form.id;
    store_form(&state, &caller, &form).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

/// Import an XLSForm `.xlsx` (raw request body) and register the resulting form.
async fn import_form(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let form = collecta_xlsform::parse_bytes(&body)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let id = form.id;
    store_form(&state, &caller, &form).await?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

/// Record a form as the caller's. A form carries its own id, so this doubles as
/// the update path: posting an id that already exists is refused unless the
/// caller created that form or is an admin.
async fn store_form(state: &AppState, caller: &Caller, form: &Form) -> Result<(), ApiError> {
    let writer = FormWriter {
        id: Some(caller.id),
        overwrite_any: caller.role.is_admin(),
    };
    if state.store.insert_form(form, writer).await? {
        return Ok(());
    }
    Err(ApiError(
        StatusCode::FORBIDDEN,
        "that form id belongs to another user".to_string(),
    ))
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

/// Collected data, readable by the form's creator, its grantees, and admins.
async fn list_submissions(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(form_id): Path<Uuid>,
) -> Result<Json<Vec<Submission>>, ApiError> {
    require_read(&state, &caller, form_id).await?;
    Ok(Json(state.store.list_submissions(form_id).await?))
}

// a form from before creators were recorded has nobody to match the caller
// against, so only an admin gets at it.
fn owns(caller: &Caller, owner: FormOwner) -> bool {
    caller.role.is_admin() || owner == FormOwner::User(caller.id)
}

/// The owner of a live form, or a 404 when it does not exist or was deleted.
async fn form_owner(state: &AppState, form_id: Uuid) -> Result<FormOwner, ApiError> {
    state
        .store
        .form_owner(form_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "form not found".to_string()))
}

/// Gate for reading a form's collected data: its creator, an admin, or an
/// account holding a grant on it.
async fn require_read(state: &AppState, caller: &Caller, form_id: Uuid) -> Result<(), ApiError> {
    let owner = form_owner(state, form_id).await?;
    if owns(caller, owner) || state.store.has_grant(form_id, caller.id).await? {
        return Ok(());
    }
    Err(ApiError(StatusCode::FORBIDDEN, "not your form".to_string()))
}

/// Gate for changing a form: its creator or an admin.
///
/// A grant deliberately does not pass this. Sharing a form is read-only, so a
/// grantee can neither delete the data nor hand the form on to someone else.
async fn require_owner(state: &AppState, caller: &Caller, form_id: Uuid) -> Result<(), ApiError> {
    let owner = form_owner(state, form_id).await?;
    if owns(caller, owner) {
        return Ok(());
    }
    Err(ApiError(StatusCode::FORBIDDEN, "not your form".to_string()))
}

/// Record who filed a submission, overwriting whatever the body claimed.
///
/// The token's subject is the only thing that can be evidence here: a
/// `collector_id` the client chose would let one account file data as another.
fn record_submitter(submission: &mut Submission, caller: &Caller) {
    submission.collector_id = Some(caller.id.to_string());
}

/// File one submission against the form named in the path.
///
/// The path is authoritative: a body naming a different form is refused rather
/// than corrected, since validating against one form and filing under another
/// would put unvalidated data in a form the caller may not even read.
async fn submit(
    State(state): State<AppState>,
    Path(form_id): Path<Uuid>,
    Extension(caller): Extension<Caller>,
    Json(mut submission): Json<Submission>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    if submission.form_id != form_id {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "submission form_id does not match the form in the path".to_string(),
        ));
    }
    record_submitter(&mut submission, &caller);
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
    // an id already on file is refused, never overwritten: submission ids are
    // client-chosen and unique across all forms, so a replace would let anyone
    // holding an id take the row, and its attachments, away from its form.
    if !state.store.insert_submission(&submission).await? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "a submission with this id already exists".to_string(),
        ));
    }
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

/// Delete a form and everything collected under it.
///
/// The form itself becomes a tombstone the forms pull hands to clients. Its
/// submissions, their queue entries, their attachments and the grants on it are
/// removed outright.
async fn delete_form(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(form_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, &caller, form_id).await?;
    let submissions = state.store.delete_form(form_id).await?;
    for submission_id in submissions {
        remove_attachment_files(&state, submission_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete one submission and its attachments.
///
/// No tombstone: submissions only travel client to server, so there is no pull
/// that could hand a deleted one back.
async fn delete_submission(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((form_id, submission_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, &caller, form_id).await?;
    if !state
        .store
        .delete_submission(form_id, submission_id)
        .await?
    {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "submission not found".to_string(),
        ));
    }
    remove_attachment_files(&state, submission_id).await;
    Ok(StatusCode::NO_CONTENT)
}

// the rows are already gone when this runs, so a failure here leaks disk rather
// than data and must not turn a completed delete into an error.
async fn remove_attachment_files(state: &AppState, submission_id: Uuid) {
    let directory = openrosa::attachments_dir(&state.data_dir).join(submission_id.to_string());
    let _ = tokio::fs::remove_dir_all(directory).await;
}

/// Bytes of one stored attachment, readable by whoever may read the submission
/// it hangs off.
///
/// Served as a download rather than inline, under a type from
/// [`attachment::recorded_content_type`] rather than the one the device claimed,
/// so a crafted html upload can never be rendered as a page on this origin.
async fn get_attachment(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(attachment_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let missing = || ApiError(StatusCode::NOT_FOUND, "attachment not found".to_string());
    let stored = state
        .store
        .find_attachment(attachment_id)
        .await?
        .ok_or_else(missing)?;
    // the id is the whole secret here, since nothing lists attachments a caller
    // may not read. A refusal therefore reads the same as an id nobody stored,
    // rather than confirming that this one exists.
    if let Err(error) = require_read(&state, &caller, stored.form_id).await {
        return Err(if error.0 == StatusCode::FORBIDDEN {
            missing()
        } else {
            error
        });
    }

    let bytes = tokio::fs::read(&stored.attachment.storage_path)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment bytes are missing".to_string(),
            )
        })?;
    // re-applied on the way out, so a row stored before the policy existed is
    // served under it too.
    let content_type = HeaderValue::from_static(attachment::recorded_content_type(
        &stored.attachment.content_type,
    ));
    let headers = [
        (CONTENT_TYPE, content_type),
        (CONTENT_DISPOSITION, HeaderValue::from_static("attachment")),
        (
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ),
    ];
    Ok((StatusCode::OK, headers, bytes).into_response())
}

/// Share one form's collected data with another account, owner or admin only.
async fn create_grant(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(form_id): Path<Uuid>,
    Json(request): Json<GrantRequest>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, &caller, form_id).await?;
    // granting to an id nobody holds is a typo, not a grant to be discovered
    // later when that uuid happens to be created.
    if !state.store.user_exists(request.user_id).await? {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no such user".to_string(),
        ));
    }
    state.store.grant_form(form_id, request.user_id).await?;
    Ok(StatusCode::CREATED)
}

async fn revoke_grant(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((form_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, &caller, form_id).await?;
    if !state.store.revoke_form(form_id, user_id).await? {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such grant".to_string()));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Who a form is shared with. Owner or admin only: a grantee cannot enumerate
/// the other accounts holding the same form.
async fn list_grants(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(form_id): Path<Uuid>,
) -> Result<Json<Vec<GrantResponse>>, ApiError> {
    require_owner(&state, &caller, form_id).await?;
    let grants = state.store.list_grants(form_id).await?;
    Ok(Json(
        grants
            .into_iter()
            .map(|grant| GrantResponse {
                user_id: grant.user_id,
                email: grant.email,
                granted_at: grant.granted_at,
            })
            .collect(),
    ))
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
///
/// Every item is filed under the pushing account, whatever the device recorded
/// while offline: a batch arrives over one authenticated connection, so that is
/// the only identity this server can vouch for.
async fn sync_push(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Json(mut request): Json<PushRequest>,
) -> Result<Json<PushResponse>, ApiError> {
    let mut results = Vec::with_capacity(request.submissions.len());
    for submission in &mut request.submissions {
        record_submitter(submission, &caller);
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
    // a per-item message travels back to a field device, so a storage failure
    // is reported as one rather than quoting the query that failed.
    let storage_error = |e: sqlx::Error| {
        eprintln!("error: storage error: {e}");
        error("storage error".to_string())
    };
    let form = match store.get_form(submission.form_id).await {
        Ok(Some(form)) => form,
        Ok(None) => return error(format!("unknown form {}", submission.form_id)),
        Err(e) => return storage_error(e),
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
        Err(e) => storage_error(e),
    }
}

#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: String,
}

/// Form definitions changed since the client's cursor (all forms when absent),
/// plus the ids of the ones deleted since.
///
/// Form discovery, so it is open to any authenticated account like
/// `GET /api/v1/forms` and OpenRosa's `/formList`: a collector has to be able to
/// pull a form they did not create in order to submit against it offline.
async fn sync_forms(
    State(state): State<AppState>,
    Query(query): Query<SinceQuery>,
) -> Result<Json<FormsPullResponse>, ApiError> {
    let (forms, deleted, cursor) = state.store.list_forms_since(&query.since).await?;
    Ok(Json(FormsPullResponse {
        forms,
        deleted,
        cursor: cursor.unwrap_or(query.since),
    }))
}

/// Error carrying an HTTP status and message; storage errors map to 500.
struct ApiError(StatusCode, String);

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        // the storage error text carries query and schema detail, so it goes to
        // the operator's log and never into the response, matching what the
        // openrosa surface already does.
        eprintln!("error: storage error: {e}");
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage error".to_string(),
        )
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

/// Body of `POST /api/v1/forms/{id}/grants`.
#[derive(Deserialize)]
struct GrantRequest {
    user_id: Uuid,
}

#[derive(Serialize)]
struct GrantResponse {
    user_id: Uuid,
    email: String,
    granted_at: String,
}

#[derive(Serialize)]
struct SyncStatusResponse {
    pending: usize,
    synced: usize,
    failed: usize,
    abandoned: usize,
    total: usize,
}
