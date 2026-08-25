//! Publishing a form's submissions into one ptolemy dataset as versioned features.
//!
//! The caller's own bearer token is forwarded to ptolemy, so collecta stores no
//! credential and can publish only what the caller could have written by hand.
//!
//! The first publish of a form creates the dataset, its schema and its branch,
//! and records both ids on the form. Every publish after that commits the
//! submissions no earlier publish recorded, in batches, marking each batch
//! published as soon as ptolemy accepts it: a run that dies half way leaves the
//! accepted batches recorded, so the next one sends only the rest.

use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use collecta_core::form::{FieldType, Form, FormField};
use collecta_core::submission::{FieldValue, GeoPoint, Submission};

use crate::auth::{BEARER_PREFIX, Caller};
use crate::store::PublishTarget;
use crate::{ApiError, AppState, require_owner};

/// Environment variable holding the root ptolemy is served at.
pub const PTOLEMY_URL_VARIABLE: &str = "COLLECTA_PTOLEMY_URL";

/// Prefix every ptolemy route shares.
const PTOLEMY_API: &str = "/api/v1";

/// The branch features are committed to, matching what verne-load creates.
const BRANCH_NAME: &str = "main";

/// Published geometry is always lon/lat degrees, whatever the device recorded.
const DATASET_SRID: i32 = 4326;

/// How many features go in one commit.
const PUBLISH_BATCH_SIZE: usize = 500;

const PTOLEMY_TIMEOUT: Duration = Duration::from_secs(30);

/// Route the attachment URLs in a published feature point at.
const ATTACHMENT_ROUTE: &str = "/api/v1/attachments";

/// Properties every published feature carries on top of its field values.
const SUBMISSION_ID_PROPERTY: &str = "submission_id";
const COLLECTOR_ID_PROPERTY: &str = "collector_id";
const COMPLETED_AT_PROPERTY: &str = "completed_at";

/// ptolemy's attribute type names.
const ATTRIBUTE_STRING: &str = "string";
const ATTRIBUTE_INTEGER: &str = "integer";
const ATTRIBUTE_FLOAT: &str = "float";
const ATTRIBUTE_BOOLEAN: &str = "boolean";
const ATTRIBUTE_ARRAY: &str = "array";

/// ptolemy's geometry type names.
const GEOMETRY_POINT: &str = "point";
const GEOMETRY_LINESTRING: &str = "linestring";
const GEOMETRY_POLYGON: &str = "polygon";

/// Little-endian WKB: the byte order marker, the geometry codes, and the single
/// ring a published polygon carries.
const WKB_LITTLE_ENDIAN: u8 = 1;
const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_POLYGON_RINGS: u32 = 1;

/// `POST /api/v1/forms/{form_id}/publish`.
pub async fn publish(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(form_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<PublishResponse>, PublishError> {
    require_owner(&state, &caller, form_id).await?;
    let base = state.ptolemy_url.as_deref().ok_or_else(|| {
        PublishError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{PTOLEMY_URL_VARIABLE} is not set, so there is no ptolemy to publish to"),
        )
    })?;
    let token = bearer_token(&headers)?;
    let form = state
        .store
        .get_form(form_id)
        .await?
        .ok_or_else(|| PublishError(StatusCode::NOT_FOUND, "form not found".to_string()))?;

    let ptolemy = Ptolemy::new(base, token)?;
    let author = caller.id.to_string();
    let target = match state.store.publish_target(form_id).await? {
        Some(target) => target,
        None => {
            let target = ptolemy.create_target(&form, &author).await?;
            state.store.set_publish_target(form_id, target).await?;
            target
        }
    };

    let submissions = state.store.unpublished_submissions(form_id).await?;
    let mut operations = Vec::with_capacity(submissions.len());
    let mut skipped = 0;
    for submission in &submissions {
        match insert_operation(&form, submission, state.base_url.as_deref()) {
            Some(operation) => operations.push(operation),
            None => skipped += 1,
        }
    }

    let mut published = 0;
    for batch in operations.chunks(PUBLISH_BATCH_SIZE) {
        ptolemy
            .commit(
                target.branch_id,
                &commit_message(&form.title, batch.len()),
                &author,
                batch,
            )
            .await?;
        let ids: Vec<Uuid> = batch.iter().map(|operation| operation.feature_id).collect();
        state.store.record_published(form_id, &ids).await?;
        published += batch.len();
    }

    Ok(Json(PublishResponse {
        dataset_id: target.dataset_id,
        branch_id: target.branch_id,
        published,
        skipped,
        total_published: state.store.published_count(form_id).await?,
    }))
}

#[derive(Serialize)]
pub struct PublishResponse {
    dataset_id: Uuid,
    branch_id: Uuid,
    published: usize,
    skipped: usize,
    total_published: usize,
}

fn commit_message(title: &str, count: usize) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("collecta: {count} submission{plural} of {title}")
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, PublishError> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(BEARER_PREFIX))
        .ok_or_else(|| {
            PublishError(
                StatusCode::UNAUTHORIZED,
                "publishing forwards the caller's bearer token, and this request carries none"
                    .to_string(),
            )
        })
}

/// A thin ptolemy client holding the caller's token for the length of one publish.
struct Ptolemy {
    client: reqwest::Client,
    /// The base URL with no trailing slash, so a route can be appended as-is.
    base: String,
    token: String,
}

impl Ptolemy {
    fn new(base: &str, token: &str) -> Result<Self, PublishError> {
        let client = reqwest::Client::builder()
            .timeout(PTOLEMY_TIMEOUT)
            .build()
            .map_err(|error| {
                PublishError(
                    StatusCode::BAD_GATEWAY,
                    format!("cannot build a client for ptolemy: {error}"),
                )
            })?;
        Ok(Ptolemy {
            client,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    /// The dataset, its schema and its branch, in the order ptolemy needs them.
    async fn create_target(
        &self,
        form: &Form,
        author: &str,
    ) -> Result<PublishTarget, PublishError> {
        let route = format!("{PTOLEMY_API}/datasets");
        let created = self
            .post(
                &route,
                &DatasetBody {
                    name: &form.title,
                    srid: DATASET_SRID,
                    geometry_type: dataset_geometry_type(form),
                    created_by: author,
                },
            )
            .await?;
        let dataset_id = id_of(&route, &created)?;
        // the schema goes on before any feature, since ptolemy validates a
        // commit against it
        self.put(
            &format!("{PTOLEMY_API}/datasets/{dataset_id}/schema"),
            &SchemaBody {
                fields: schema_fields(form),
            },
        )
        .await?;
        Ok(PublishTarget {
            dataset_id,
            branch_id: self.branch(dataset_id, author).await?,
        })
    }

    /// The dataset's `main` branch, created when it has none. A dataset ptolemy
    /// made itself carries no branch, and a registered external one already does.
    async fn branch(&self, dataset_id: Uuid, author: &str) -> Result<Uuid, PublishError> {
        let route = format!("{PTOLEMY_API}/datasets/{dataset_id}/branches");
        let listed = self.get(&route).await?;
        if let Some(id) = named_id(&listed, BRANCH_NAME) {
            return Ok(id);
        }
        let created = self
            .post(
                &route,
                &BranchBody {
                    name: BRANCH_NAME,
                    created_by: author,
                },
            )
            .await?;
        id_of(&route, &created)
    }

    async fn commit(
        &self,
        branch_id: Uuid,
        message: &str,
        author: &str,
        operations: &[InsertOperation],
    ) -> Result<(), PublishError> {
        self.post(
            &format!("{PTOLEMY_API}/branches/{branch_id}/commit"),
            &CommitBody {
                message,
                author,
                operations,
            },
        )
        .await
        .map(drop)
    }

    async fn get(&self, route: &str) -> Result<Value, PublishError> {
        let sent = self
            .client
            .get(format!("{}{route}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await;
        self.read(route, sent).await
    }

    async fn post<T: Serialize>(&self, route: &str, body: &T) -> Result<Value, PublishError> {
        let sent = self
            .client
            .post(format!("{}{route}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await;
        self.read(route, sent).await
    }

    async fn put<T: Serialize>(&self, route: &str, body: &T) -> Result<Value, PublishError> {
        let sent = self
            .client
            .put(format!("{}{route}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await;
        self.read(route, sent).await
    }

    /// The answer's json, or the error the status says it is. A refused token is
    /// the caller's problem and comes back as 403. Anything else is ptolemy's
    /// and comes back as 502 naming the status.
    async fn read(
        &self,
        route: &str,
        sent: reqwest::Result<reqwest::Response>,
    ) -> Result<Value, PublishError> {
        let response = sent.map_err(|error| {
            PublishError(
                StatusCode::BAD_GATEWAY,
                format!("cannot reach ptolemy at {route}: {error}"),
            )
        })?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(PublishError(
                StatusCode::FORBIDDEN,
                format!("ptolemy refused the token on {route} with {status}"),
            ));
        }
        if !status.is_success() {
            return Err(PublishError(
                StatusCode::BAD_GATEWAY,
                format!("ptolemy answered {route} with {status}"),
            ));
        }
        // a PUT answers with a status and no body, which is not an error here
        Ok(response.json().await.unwrap_or(Value::Null))
    }
}

/// The id of the row called `name` in a listed array.
fn named_id(listed: &Value, name: &str) -> Option<Uuid> {
    listed.as_array()?.iter().find_map(|row| {
        (row.get("name")?.as_str()? == name)
            .then(|| row.get("id")?.as_str()?.parse().ok())
            .flatten()
    })
}

fn id_of(route: &str, body: &Value) -> Result<Uuid, PublishError> {
    body.get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| {
            PublishError(
                StatusCode::BAD_GATEWAY,
                format!("ptolemy answered {route} with no id"),
            )
        })
}

/// The form's first geometry field, which decides what the dataset holds and
/// where each submission's geometry is read from.
fn geometry_field(form: &Form) -> Option<&FormField> {
    form.fields.iter().find(|field| {
        matches!(
            field.field_type,
            FieldType::GeoPoint | FieldType::GeoTrace | FieldType::GeoShape
        )
    })
}

/// A form with no geometry field publishes its submissions' device locations,
/// which are points.
fn dataset_geometry_type(form: &Form) -> &'static str {
    match geometry_field(form).map(|field| &field.field_type) {
        Some(FieldType::GeoTrace) => GEOMETRY_LINESTRING,
        Some(FieldType::GeoShape) => GEOMETRY_POLYGON,
        _ => GEOMETRY_POINT,
    }
}

fn schema_fields(form: &Form) -> Vec<SchemaField> {
    form.fields
        .iter()
        .filter_map(|field| {
            attribute_type(&field.field_type).map(|field_type| SchemaField {
                name: field.name.clone(),
                field_type,
                // collecta already enforced its own required fields on ingest
                required: false,
            })
        })
        .collect()
}

/// The ptolemy attribute type a field's values arrive as, or `None` for the
/// geometry types, which are the feature's geometry rather than an attribute.
fn attribute_type(field_type: &FieldType) -> Option<&'static str> {
    Some(match field_type {
        FieldType::Integer => ATTRIBUTE_INTEGER,
        FieldType::Decimal => ATTRIBUTE_FLOAT,
        FieldType::Boolean => ATTRIBUTE_BOOLEAN,
        FieldType::MultiSelect | FieldType::Repeat => ATTRIBUTE_ARRAY,
        FieldType::GeoPoint | FieldType::GeoTrace | FieldType::GeoShape => return None,
        // the media types included: an attachment publishes as the url its
        // bytes are downloadable from
        _ => ATTRIBUTE_STRING,
    })
}

/// One submission as an insert, or `None` when it carries no geometry at all.
fn insert_operation(
    form: &Form,
    submission: &Submission,
    base_url: Option<&str>,
) -> Option<InsertOperation> {
    Some(InsertOperation {
        feature_id: submission.id,
        geometry_wkb_hex: feature_geometry(form, submission)?,
        properties: feature_properties(submission, base_url),
    })
}

fn feature_geometry(form: &Form, submission: &Submission) -> Option<String> {
    let from_field = geometry_field(form)
        .and_then(|field| submission.values.get(&field.name))
        .and_then(geometry_wkb_hex);
    from_field.or_else(|| submission.device_location.as_ref().map(point_wkb_hex))
}

fn geometry_wkb_hex(value: &FieldValue) -> Option<String> {
    match value {
        FieldValue::GeoPoint(point) => Some(point_wkb_hex(point)),
        FieldValue::GeoTrace(points) if !points.is_empty() => Some(linestring_wkb_hex(points)),
        FieldValue::GeoShape(points) if !points.is_empty() => Some(polygon_wkb_hex(points)),
        _ => None,
    }
}

fn point_wkb_hex(point: &GeoPoint) -> String {
    let mut bytes = wkb_header(WKB_POINT);
    push_coordinates(&mut bytes, point);
    hex(&bytes)
}

fn linestring_wkb_hex(points: &[GeoPoint]) -> String {
    let mut bytes = wkb_header(WKB_LINESTRING);
    push_points(&mut bytes, points);
    hex(&bytes)
}

fn polygon_wkb_hex(points: &[GeoPoint]) -> String {
    let mut bytes = wkb_header(WKB_POLYGON);
    bytes.extend_from_slice(&WKB_POLYGON_RINGS.to_le_bytes());
    let mut ring = points.to_vec();
    if ring.first() != ring.last() {
        ring.push(ring[0].clone());
    }
    push_points(&mut bytes, &ring);
    hex(&bytes)
}

fn wkb_header(geometry: u32) -> Vec<u8> {
    let mut bytes = vec![WKB_LITTLE_ENDIAN];
    bytes.extend_from_slice(&geometry.to_le_bytes());
    bytes
}

fn push_points(bytes: &mut Vec<u8>, points: &[GeoPoint]) {
    bytes.extend_from_slice(&(points.len() as u32).to_le_bytes());
    for point in points {
        push_coordinates(bytes, point);
    }
}

fn push_coordinates(bytes: &mut Vec<u8>, point: &GeoPoint) {
    bytes.extend_from_slice(&point.longitude.to_le_bytes());
    bytes.extend_from_slice(&point.latitude.to_le_bytes());
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn feature_properties(submission: &Submission, base_url: Option<&str>) -> Map<String, Value> {
    let mut properties = Map::new();
    for (name, value) in &submission.values {
        if let Some(property) = property_value(value, base_url) {
            properties.insert(name.clone(), property);
        }
    }
    // an openrosa upload records its bytes here rather than in the field value
    for attachment in &submission.attachments {
        properties.insert(
            attachment.field_name.clone(),
            Value::String(attachment_url(base_url, attachment.id)),
        );
    }
    properties.insert(
        SUBMISSION_ID_PROPERTY.to_string(),
        Value::String(submission.id.to_string()),
    );
    properties.insert(
        COLLECTOR_ID_PROPERTY.to_string(),
        match &submission.collector_id {
            Some(collector) => Value::String(collector.clone()),
            None => Value::Null,
        },
    );
    properties.insert(
        COMPLETED_AT_PROPERTY.to_string(),
        match submission.completed_at {
            Some(completed) => Value::String(completed.to_rfc3339()),
            None => Value::Null,
        },
    );
    properties
}

/// One field value as json, or `None` for a geometry, which is the feature's
/// geometry rather than one of its properties.
fn property_value(value: &FieldValue, base_url: Option<&str>) -> Option<Value> {
    Some(match value {
        FieldValue::Text(text)
        | FieldValue::Date(text)
        | FieldValue::DateTime(text)
        | FieldValue::Time(text)
        | FieldValue::Choice(text)
        | FieldValue::Barcode(text) => Value::String(text.clone()),
        FieldValue::Integer(number) => Value::from(*number),
        FieldValue::Decimal(number) => Value::from(*number),
        FieldValue::Boolean(flag) => Value::Bool(*flag),
        FieldValue::MultiChoice(choices) => {
            Value::Array(choices.iter().map(|c| Value::String(c.clone())).collect())
        }
        FieldValue::Attachment(id) => Value::String(attachment_url(base_url, *id)),
        FieldValue::Repeat(rows) => Value::Array(
            rows.iter()
                .map(|row| {
                    let mut object = Map::new();
                    for (name, value) in row {
                        if let Some(property) = property_value(value, base_url) {
                            object.insert(name.clone(), property);
                        }
                    }
                    Value::Object(object)
                })
                .collect(),
        ),
        FieldValue::Null => Value::Null,
        FieldValue::GeoPoint(_) | FieldValue::GeoTrace(_) | FieldValue::GeoShape(_) => return None,
    })
}

/// Where a published feature says its attachment can be downloaded. Without a
/// base url that is the route alone, since nothing else here knows the origin.
fn attachment_url(base_url: Option<&str>, id: Uuid) -> String {
    match base_url {
        Some(base) => format!("{}{ATTACHMENT_ROUTE}/{id}", base.trim_end_matches('/')),
        None => format!("{ATTACHMENT_ROUTE}/{id}"),
    }
}

/// `POST /api/v1/datasets`.
#[derive(Serialize)]
struct DatasetBody<'a> {
    name: &'a str,
    srid: i32,
    geometry_type: &'a str,
    created_by: &'a str,
}

/// `PUT /api/v1/datasets/{id}/schema`.
#[derive(Serialize)]
struct SchemaBody {
    fields: Vec<SchemaField>,
}

#[derive(Serialize)]
struct SchemaField {
    name: String,
    field_type: &'static str,
    required: bool,
}

/// `POST /api/v1/datasets/{id}/branches`.
#[derive(Serialize)]
struct BranchBody<'a> {
    name: &'a str,
    created_by: &'a str,
}

/// `POST /api/v1/branches/{id}/commit`.
#[derive(Serialize)]
struct CommitBody<'a> {
    message: &'a str,
    author: &'a str,
    operations: &'a [InsertOperation],
}

/// One insert operation of a commit, carrying the tag ptolemy reads the
/// operation kind from.
#[derive(Serialize)]
#[serde(tag = "type", rename = "insert")]
struct InsertOperation {
    feature_id: Uuid,
    geometry_wkb_hex: String,
    properties: Map<String, Value>,
}

/// Error for the publish route, answered as `{"error": "..."}` so a viewer can
/// show what ptolemy said.
pub struct PublishError(StatusCode, String);

impl From<ApiError> for PublishError {
    fn from(error: ApiError) -> Self {
        PublishError(error.0, error.1)
    }
}

impl From<sqlx::Error> for PublishError {
    fn from(error: sqlx::Error) -> Self {
        ApiError::from(error).into()
    }
}

impl IntoResponse for PublishError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
