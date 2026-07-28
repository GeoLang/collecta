//! OpenRosa compatibility layer, so ODK Collect can use collecta as its server.
//!
//! Mounted at the server root next to the JSON API: `/formList`, the per-form
//! XForm download, and `/submission`. These routes authenticate with HTTP Basic
//! against the same users table as the JWT API; ODK Collect has no concept of a
//! bearer token.
//!
//! Spec: <https://docs.getodk.org/openrosa/>. Every response carries
//! `X-OpenRosa-Version: 1.0` and a `Date`, including the 401 challenge, because
//! Collect uses those headers to decide it is talking to an OpenRosa server
//! rather than a captive portal.

pub mod xform;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_TYPE, DATE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{BASIC_CHALLENGE, parse_basic_header, verify_basic};
use crate::store::UserRecord;

/// Largest POST body this server tells clients to send, in bytes.
///
/// Doubles as the per-part cap: a single attachment may fill a whole request.
pub const MAX_CONTENT_LENGTH: usize = 52_428_800;

/// Slack over [`MAX_CONTENT_LENGTH`] for multipart boundaries and part headers,
/// so a client that fills its budget with payload is not rejected on framing.
const MULTIPART_OVERHEAD: usize = 1_048_576;

/// Hard limit enforced on the submission request body.
pub const MAX_REQUEST_BODY: usize = MAX_CONTENT_LENGTH + MULTIPART_OVERHEAD;

const XML_CONTENT_TYPE: &str = "text/xml; charset=utf-8";
const OPENROSA_VERSION: HeaderName = HeaderName::from_static("x-openrosa-version");
const ACCEPT_CONTENT_LENGTH: HeaderName =
    HeaderName::from_static("x-openrosa-accept-content-length");

/// OpenRosa routes, Basic-authenticated, with the protocol headers on every
/// response.
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/formList", get(form_list))
        .route("/forms/{form_id}/form.xml", get(download_form))
        .route_layer(middleware::from_fn_with_state(state, require_basic))
        // outside the auth layer so the 401 challenge is tagged too.
        .layer(middleware::from_fn(openrosa_headers))
}

// ---- auth --------------------------------------------------------------

/// The user a Basic-authenticated OpenRosa request belongs to.
///
/// Handlers read this from request extensions; it is only ever inserted after
/// a successful password verification.
#[derive(Clone)]
pub struct OpenRosaUser {
    pub id: Uuid,
}

/// HTTP Basic against the users table.
///
/// Any missing, malformed, or wrong credential yields the same 401 challenge:
/// the failure reason is never disclosed, and nothing derived from the header
/// is logged.
async fn require_basic(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let user: Option<UserRecord> = match header.and_then(parse_basic_header) {
        Some(credentials) => match verify_basic(&state.store, &credentials).await {
            Ok(user) => user,
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not verify credentials",
                )
                .into_response();
            }
        },
        None => None,
    };

    let Some(user) = user else {
        return challenge();
    };
    request
        .extensions_mut()
        .insert(OpenRosaUser { id: user.id });
    next.run(request).await
}

/// 401 carrying the Basic challenge ODK Collect waits for before it will send
/// credentials at all.
fn challenge() -> Response {
    let mut response = error(StatusCode::UNAUTHORIZED, "authentication required").into_response();
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static(BASIC_CHALLENGE));
    response
}

// ---- shared response plumbing ------------------------------------------

/// Adds `X-OpenRosa-Version` and `Date` to everything the OpenRosa router
/// returns.
async fn openrosa_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(OPENROSA_VERSION, HeaderValue::from_static("1.0"));
    if let Ok(date) = HeaderValue::from_str(&http_date()) {
        headers.insert(DATE, date);
    }
    response
}

/// IMF-fixdate, the format the OpenRosa HTTP spec requires.
fn http_date() -> String {
    chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// An error carrying an `OpenRosaResponse` envelope, which is what Collect
/// surfaces to the user.
pub struct OpenRosaError(pub StatusCode, pub String);

pub fn error(status: StatusCode, message: impl Into<String>) -> OpenRosaError {
    OpenRosaError(status, message.into())
}

impl From<sqlx::Error> for OpenRosaError {
    fn from(_: sqlx::Error) -> Self {
        // the storage error text can carry query and schema detail; keep it
        // out of a response that goes to a field device.
        OpenRosaError(StatusCode::INTERNAL_SERVER_ERROR, "storage error".into())
    }
}

impl IntoResponse for OpenRosaError {
    fn into_response(self) -> Response {
        (self.0, xml_headers(), envelope(&self.1)).into_response()
    }
}

/// `Content-Type` plus the accepted-length advertisement carried on every
/// OpenRosa body.
fn xml_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(XML_CONTENT_TYPE));
    headers.insert(
        ACCEPT_CONTENT_LENGTH,
        HeaderValue::from(MAX_CONTENT_LENGTH as u64),
    );
    headers
}

/// The `OpenRosaResponse` envelope, with `message` escaped.
pub fn envelope(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for c in message.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(c),
        }
    }
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<OpenRosaResponse xmlns=\"http://openrosa.org/http/response\">\n",
            "  <message nature=\"\">{}</message>\n",
            "</OpenRosaResponse>\n"
        ),
        escaped
    )
}

// ---- form list and download --------------------------------------------

/// `GET /formList` — the forms this server can serve to Collect.
///
/// A form whose field names cannot be expressed as XML elements is left out:
/// Collect could not render it, and listing an entry whose download 500s would
/// just wedge the client's refresh.
async fn form_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, OpenRosaError> {
    let base = base_url(&state, &headers);
    let forms = state.store.list_forms().await?;

    let mut body = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    body.push_str("<xforms xmlns=\"http://openrosa.org/xforms/xformsList\">\n");
    for form in &forms {
        let Ok(xml) = xform::render(form) else {
            continue;
        };
        body.push_str("  <xform>\n");
        body.push_str(&format!("    <formID>{}</formID>\n", form.id));
        body.push_str(&format!("    <name>{}</name>\n", escape_text(&form.title)));
        body.push_str(&format!("    <version>{}</version>\n", form.version));
        body.push_str(&format!("    <hash>{}</hash>\n", xform::form_hash(&xml)));
        body.push_str(&format!(
            "    <downloadUrl>{}/forms/{}/form.xml</downloadUrl>\n",
            escape_text(&base),
            form.id
        ));
        body.push_str("  </xform>\n");
    }
    body.push_str("</xforms>\n");

    Ok((StatusCode::OK, xml_headers(), body).into_response())
}

/// `GET /forms/{form_id}/form.xml` — the form rendered as an XForm.
async fn download_form(
    State(state): State<AppState>,
    Path(form_id): Path<Uuid>,
) -> Result<Response, OpenRosaError> {
    let form = state
        .store
        .get_form(form_id)
        .await?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "form not found"))?;
    let xml = xform::render(&form).map_err(|e| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("form cannot be rendered as an xform: {e}"),
        )
    })?;
    Ok((StatusCode::OK, xml_headers(), xml).into_response())
}

/// Absolute base for `downloadUrl`.
///
/// `COLLECTA_BASE_URL` wins when configured. Otherwise it is reconstructed from
/// the request, which is client-controlled but only ever echoed back to that
/// same client, so a spoofed `Host` misdirects nobody else.
fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(base) = &state.base_url {
        return base.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|s| *s == "https" || *s == "http")
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Directory holding one subdirectory of attachments per submission.
pub fn attachments_dir(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("attachments")
}

/// Shared across handlers that need the configured storage root.
pub type DataDir = Arc<std::path::Path>;
