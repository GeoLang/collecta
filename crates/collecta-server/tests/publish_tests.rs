// End-to-end tests of `POST /api/v1/forms/{id}/publish` through the real
// router, against a fake ptolemy served in-process that records every request
// it receives.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Json, Response};
use collecta_core::form::{FieldType, Form, FormField};
use collecta_core::submission::{FieldValue, GeoPoint, Submission};
use collecta_server::auth::{TokenResponse, hash_password};
use collecta_server::store::{FormWriter, Store, UserRecord};
use collecta_server::{Config, router};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const TEST_SECRET: &str = "test-secret-0123456789abcdef0123456789abcdef";
const OWNER_EMAIL: &str = "owner@example.com";
const VIEWER_EMAIL: &str = "viewer@example.com";
const TEST_PASSWORD: &str = "correct horse battery staple";
const BASE_URL: &str = "https://collect.example.org";
const FORM_TITLE: &str = "Site Inspection";

/// The batch size the publish route commits in, mirrored here so the
/// multi-batch test can build one submission more than a single batch holds.
const PUBLISH_BATCH_SIZE: usize = 500;

// ─── the fake ptolemy ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct FakeState {
    requests: Arc<Mutex<Vec<Recorded>>>,
    dataset_id: Uuid,
    branch_id: Uuid,
    /// Answer every request with this status instead of serving it.
    refuse_with: Arc<Mutex<Option<StatusCode>>>,
    /// Fail commits from this one on, counting from 1.
    fail_commits_from: Arc<Mutex<Option<usize>>>,
}

struct FakePtolemy {
    base_url: String,
    state: FakeState,
}

impl FakePtolemy {
    fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    fn paths(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|recorded| format!("{} {}", recorded.method, recorded.path))
            .collect()
    }

    fn commits(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .filter(|recorded| recorded.path.ends_with("/commit"))
            .map(|recorded| recorded.body.clone())
            .collect()
    }

    fn body_of(&self, method: &str, path: &str) -> Value {
        self.requests()
            .iter()
            .find(|recorded| recorded.method == method && recorded.path == path)
            .unwrap_or_else(|| panic!("no {method} {path} was sent"))
            .body
            .clone()
    }

    fn fail_commits_from(&self, commit: Option<usize>) {
        *self.state.fail_commits_from.lock().unwrap() = commit;
    }

    fn forget(&self) {
        self.state.requests.lock().unwrap().clear();
    }
}

async fn fake_ptolemy() -> FakePtolemy {
    let state = FakeState {
        requests: Arc::new(Mutex::new(Vec::new())),
        dataset_id: Uuid::new_v4(),
        branch_id: Uuid::new_v4(),
        refuse_with: Arc::new(Mutex::new(None)),
        fail_commits_from: Arc::new(Mutex::new(None)),
    };
    let app = Router::new().fallback(serve).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakePtolemy {
        base_url: format!("http://{address}"),
        state,
    }
}

async fn serve(
    State(state): State<FakeState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let commit = path.ends_with("/commit");
    let commits_so_far = {
        let mut requests = state.requests.lock().unwrap();
        requests.push(Recorded {
            method: method.to_string(),
            path: path.clone(),
            authorization: headers
                .get("authorization")
                .map(|value| value.to_str().unwrap().to_string()),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        requests
            .iter()
            .filter(|recorded| recorded.path.ends_with("/commit"))
            .count()
    };

    if let Some(status) = *state.refuse_with.lock().unwrap() {
        return (status, Json(json!({ "error": "no" }))).into_response();
    }
    if commit {
        if let Some(first_failure) = *state.fail_commits_from.lock().unwrap()
            && commits_so_far >= first_failure
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "commit exploded" })),
            )
                .into_response();
        }
        return created(Uuid::new_v4());
    }
    if path.ends_with("/schema") {
        return StatusCode::OK.into_response();
    }
    if path.ends_with("/branches") {
        return match method {
            Method::GET => (StatusCode::OK, Json(json!([]))).into_response(),
            _ => created(state.branch_id),
        };
    }
    created(state.dataset_id)
}

fn created(id: Uuid) -> Response {
    (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
}

// ─── the collecta side ──────────────────────────────────────────────

struct Harness {
    app: Router,
    store: Store,
    owner_token: String,
    owner_id: Uuid,
    viewer_token: String,
}

async fn harness(ptolemy_url: Option<String>) -> Harness {
    let store = Store::connect(":memory:").await.unwrap();
    let owner_id = seed_user(&store, OWNER_EMAIL, "admin").await;
    seed_user(&store, VIEWER_EMAIL, "viewer").await;
    let app = router(
        store.clone(),
        Config {
            jwt_secret: TEST_SECRET.to_string(),
            data_dir: std::env::temp_dir().join("collecta-publish-tests-unused"),
            base_url: Some(BASE_URL.to_string()),
            ptolemy_url,
        },
    );
    let owner_token = login(&app, OWNER_EMAIL).await;
    let viewer_token = login(&app, VIEWER_EMAIL).await;
    Harness {
        app,
        store,
        owner_token,
        owner_id,
        viewer_token,
    }
}

async fn seed_user(store: &Store, email: &str, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    store
        .create_user(&UserRecord {
            id,
            email: email.to_string(),
            password_hash: hash_password(TEST_PASSWORD),
            role: role.to_string(),
        })
        .await
        .unwrap();
    id
}

async fn login(app: &Router, email: &str) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({ "email": email, "password": TEST_PASSWORD })).unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    json_body::<TokenResponse>(response).await.token
}

async fn publish(app: &Router, form_id: Uuid, token: &str) -> Response {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/forms/{form_id}/publish"))
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

async fn json_body<T: serde::de::DeserializeOwned>(response: Response) -> T {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn field(name: &str, field_type: FieldType) -> FormField {
    let mut field = FormField::text(name, name);
    field.field_type = field_type;
    field
}

/// A form with one geometry field and one of every attribute kind published.
fn inspection_form() -> Form {
    let mut form = Form::new(FORM_TITLE);
    for built in [
        field("site_name", FieldType::Text),
        field("location", FieldType::GeoPoint),
        field("poles", FieldType::Integer),
        field("height", FieldType::Decimal),
        field("safe", FieldType::Boolean),
        field("condition", FieldType::Select),
        field("hazards", FieldType::MultiSelect),
        field("photo", FieldType::Photo),
    ] {
        form.add_field(built);
    }
    form
}

async fn store_form(harness: &Harness, form: &Form) {
    harness
        .store
        .insert_form(
            form,
            FormWriter {
                id: Some(harness.owner_id),
                overwrite_any: false,
            },
        )
        .await
        .unwrap();
}

async fn store_submission(harness: &Harness, submission: &Submission) {
    assert!(harness.store.insert_submission(submission).await.unwrap());
}

/// A completed submission of `form` at one point, with values for every
/// attribute field.
fn filled_submission(form: &Form, site: &str) -> Submission {
    let mut submission = Submission::new(form.id, form.version);
    submission.set_value("site_name", FieldValue::Text(site.to_string()));
    submission.set_value(
        "location",
        FieldValue::GeoPoint(GeoPoint::new(51.5, -0.125)),
    );
    submission.set_value("poles", FieldValue::Integer(4));
    submission.set_value("height", FieldValue::Decimal(2.5));
    submission.set_value("safe", FieldValue::Boolean(true));
    submission.set_value("condition", FieldValue::Choice("good".to_string()));
    submission.set_value(
        "hazards",
        FieldValue::MultiChoice(vec!["ice".to_string(), "traffic".to_string()]),
    );
    submission.collector_id = Some(Uuid::new_v4().to_string());
    submission.complete();
    submission
}

// ─── tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn first_publish_creates_the_dataset_schema_and_branch() {
    let ptolemy = fake_ptolemy().await;
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let form = inspection_form();
    store_form(&harness, &form).await;

    let attachment_id = Uuid::new_v4();
    let mut first = filled_submission(&form, "alpha");
    first.set_value("photo", FieldValue::Attachment(attachment_id));
    let second = filled_submission(&form, "beta");
    store_submission(&harness, &first).await;
    store_submission(&harness, &second).await;

    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = json_body(response).await;

    let dataset_id = ptolemy.state.dataset_id;
    let branch_id = ptolemy.state.branch_id;
    assert_eq!(
        body,
        json!({
            "dataset_id": dataset_id,
            "branch_id": branch_id,
            "published": 2,
            "skipped": 0,
            "total_published": 2,
        })
    );

    assert_eq!(
        ptolemy.paths(),
        vec![
            "POST /api/v1/datasets".to_string(),
            format!("PUT /api/v1/datasets/{dataset_id}/schema"),
            format!("GET /api/v1/datasets/{dataset_id}/branches"),
            format!("POST /api/v1/datasets/{dataset_id}/branches"),
            format!("POST /api/v1/branches/{branch_id}/commit"),
        ]
    );

    // the caller's own token, never one collecta holds
    for recorded in ptolemy.requests() {
        assert_eq!(
            recorded.authorization,
            Some(format!("Bearer {}", harness.owner_token))
        );
    }

    assert_eq!(
        ptolemy.body_of("POST", "/api/v1/datasets"),
        json!({
            "name": FORM_TITLE,
            "srid": 4326,
            "geometry_type": "point",
            "created_by": harness.owner_id.to_string(),
        })
    );
    assert_eq!(
        ptolemy.body_of("PUT", &format!("/api/v1/datasets/{dataset_id}/schema")),
        json!({
            "fields": [
                { "name": "site_name", "field_type": "string", "required": false },
                { "name": "poles", "field_type": "integer", "required": false },
                { "name": "height", "field_type": "float", "required": false },
                { "name": "safe", "field_type": "boolean", "required": false },
                { "name": "condition", "field_type": "string", "required": false },
                { "name": "hazards", "field_type": "array", "required": false },
                { "name": "photo", "field_type": "string", "required": false },
            ]
        })
    );

    let commits = ptolemy.commits();
    assert_eq!(commits.len(), 1);
    let commit = &commits[0];
    assert_eq!(commit["author"], json!(harness.owner_id.to_string()));
    assert_eq!(
        commit["message"],
        json!(format!("collecta: 2 submissions of {FORM_TITLE}"))
    );
    let operations = commit["operations"].as_array().unwrap();
    assert_eq!(
        operations
            .iter()
            .map(|operation| operation["feature_id"].clone())
            .collect::<Vec<_>>(),
        vec![json!(first.id), json!(second.id)]
    );
    assert_eq!(operations[0]["type"], json!("insert"));
    // little-endian point wkb: lon then lat
    assert_eq!(
        operations[0]["geometry_wkb_hex"],
        json!("0101000000000000000000c0bf0000000000c04940")
    );
    assert_eq!(
        operations[0]["properties"],
        json!({
            "site_name": "alpha",
            "poles": 4,
            "height": 2.5,
            "safe": true,
            "condition": "good",
            "hazards": ["ice", "traffic"],
            "photo": format!("{BASE_URL}/api/v1/attachments/{attachment_id}"),
            "submission_id": first.id.to_string(),
            "collector_id": first.collector_id,
            "completed_at": first.completed_at.unwrap().to_rfc3339(),
        })
    );

    let target = harness
        .store
        .publish_target(form.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.dataset_id, dataset_id);
    assert_eq!(target.branch_id, branch_id);
}

#[tokio::test]
async fn second_publish_sends_only_what_is_new() {
    let ptolemy = fake_ptolemy().await;
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let form = inspection_form();
    store_form(&harness, &form).await;
    store_submission(&harness, &filled_submission(&form, "alpha")).await;

    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    ptolemy.forget();

    let late = filled_submission(&form, "beta");
    store_submission(&harness, &late).await;
    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = json_body(response).await;
    assert_eq!(body["published"], json!(1));
    assert_eq!(body["skipped"], json!(0));
    assert_eq!(body["total_published"], json!(2));

    // the dataset already exists, so the second publish only commits
    assert_eq!(
        ptolemy.paths(),
        vec![format!(
            "POST /api/v1/branches/{}/commit",
            ptolemy.state.branch_id
        )]
    );
    let operations = ptolemy.commits()[0]["operations"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0]["feature_id"], json!(late.id));
}

#[tokio::test]
async fn a_failed_batch_leaves_the_accepted_ones_published() {
    let ptolemy = fake_ptolemy().await;
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let form = inspection_form();
    store_form(&harness, &form).await;

    let mut ids = Vec::new();
    for index in 0..PUBLISH_BATCH_SIZE + 1 {
        let submission = filled_submission(&form, &format!("site {index}"));
        ids.push(submission.id);
        store_submission(&harness, &submission).await;
    }

    ptolemy.fail_commits_from(Some(2));
    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: Value = json_body(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("500"),
        "{body} should name ptolemy's status"
    );
    assert_eq!(ptolemy.commits().len(), 2);
    assert_eq!(
        harness.store.published_count(form.id).await.unwrap(),
        PUBLISH_BATCH_SIZE
    );

    ptolemy.forget();
    ptolemy.fail_commits_from(None);
    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = json_body(response).await;
    assert_eq!(body["published"], json!(1));
    assert_eq!(body["total_published"], json!(PUBLISH_BATCH_SIZE + 1));

    let commits = ptolemy.commits();
    assert_eq!(commits.len(), 1);
    let operations = commits[0]["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0]["feature_id"], json!(ids[PUBLISH_BATCH_SIZE]));
}

#[tokio::test]
async fn a_viewer_is_refused() {
    let ptolemy = fake_ptolemy().await;
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let form = inspection_form();
    store_form(&harness, &form).await;
    store_submission(&harness, &filled_submission(&form, "alpha")).await;

    let response = publish(&harness.app, form.id, &harness.viewer_token).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(ptolemy.requests().is_empty());
}

#[tokio::test]
async fn a_missing_ptolemy_url_is_unavailable() {
    let harness = harness(None).await;
    let form = inspection_form();
    store_form(&harness, &form).await;
    store_submission(&harness, &filled_submission(&form, "alpha")).await;

    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = json_body(response).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("COLLECTA_PTOLEMY_URL"),
        "{body} should name the variable"
    );
}

#[tokio::test]
async fn a_token_ptolemy_refuses_is_forbidden() {
    let ptolemy = fake_ptolemy().await;
    *ptolemy.state.refuse_with.lock().unwrap() = Some(StatusCode::UNAUTHORIZED);
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let form = inspection_form();
    store_form(&harness, &form).await;
    store_submission(&harness, &filled_submission(&form, "alpha")).await;

    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: Value = json_body(response).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("refused the token"),
        "{body} should say ptolemy refused the token"
    );
    assert_eq!(harness.store.published_count(form.id).await.unwrap(), 0);
}

#[tokio::test]
async fn a_submission_with_no_geometry_is_skipped() {
    let ptolemy = fake_ptolemy().await;
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let form = inspection_form();
    store_form(&harness, &form).await;

    let placed = filled_submission(&form, "alpha");
    let mut nowhere = Submission::new(form.id, form.version);
    nowhere.set_value("site_name", FieldValue::Text("beta".to_string()));
    nowhere.complete();
    store_submission(&harness, &placed).await;
    store_submission(&harness, &nowhere).await;

    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = json_body(response).await;
    assert_eq!(body["published"], json!(1));
    assert_eq!(body["skipped"], json!(1));
    assert_eq!(body["total_published"], json!(1));

    let operations = ptolemy.commits()[0]["operations"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0]["feature_id"], json!(placed.id));
}

#[tokio::test]
async fn a_form_without_a_geometry_field_publishes_device_locations() {
    let ptolemy = fake_ptolemy().await;
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let mut form = Form::new("Water Samples");
    form.add_field(field("site_name", FieldType::Text));
    store_form(&harness, &form).await;

    let mut submission = Submission::new(form.id, form.version);
    submission.set_value("site_name", FieldValue::Text("alpha".to_string()));
    submission.device_location = Some(GeoPoint::new(51.5, -0.125));
    submission.complete();
    store_submission(&harness, &submission).await;

    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = json_body(response).await;
    assert_eq!(body["published"], json!(1));

    assert_eq!(
        ptolemy.body_of("POST", "/api/v1/datasets")["geometry_type"],
        json!("point")
    );
    let operations = ptolemy.commits()[0]["operations"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        operations[0]["geometry_wkb_hex"],
        json!("0101000000000000000000c0bf0000000000c04940")
    );
}

#[tokio::test]
async fn a_geoshape_form_publishes_polygons() {
    let ptolemy = fake_ptolemy().await;
    let harness = harness(Some(ptolemy.base_url.clone())).await;
    let mut form = Form::new("Parcels");
    form.add_field(field("outline", FieldType::GeoShape));
    store_form(&harness, &form).await;

    let mut submission = Submission::new(form.id, form.version);
    submission.set_value(
        "outline",
        FieldValue::GeoShape(vec![
            GeoPoint::new(0.0, 0.0),
            GeoPoint::new(0.0, 1.0),
            GeoPoint::new(1.0, 1.0),
        ]),
    );
    submission.complete();
    store_submission(&harness, &submission).await;

    let response = publish(&harness.app, form.id, &harness.owner_token).await;
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        ptolemy.body_of("POST", "/api/v1/datasets")["geometry_type"],
        json!("polygon")
    );
    let operations = ptolemy.commits()[0]["operations"]
        .as_array()
        .unwrap()
        .clone();
    let wkb = operations[0]["geometry_wkb_hex"].as_str().unwrap();
    // one ring of four points, the last closing back onto the first
    assert!(wkb.starts_with("010300000001000000040000"), "{wkb}");
    assert_eq!(wkb.len(), 2 * (1 + 4 + 4 + 4 + 4 * 16));
}
