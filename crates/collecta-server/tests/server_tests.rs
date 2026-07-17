// End-to-end tests through the real router: auth (login, token rejection),
// persistence across restart, sync push idempotency, and the forms cursor.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use collecta_core::form::{Form, FormField};
use collecta_core::submission::{FieldValue, Submission};
use collecta_core::sync_protocol::{FormsPullResponse, PushItemStatus, PushRequest, PushResponse};
use collecta_server::auth::{Claims, TokenResponse, hash_password};
use collecta_server::router;
use collecta_server::store::{Store, UserRecord};
use rust_xlsxwriter::Workbook;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tower::ServiceExt;
use uuid::Uuid;

const TEST_SECRET: &str = "test-secret-0123456789abcdef0123456789abcdef";
const TEST_EMAIL: &str = "admin@example.com";
const TEST_PASSWORD: &str = "correct horse battery staple";

async fn seeded_store(db_path: &str) -> Store {
    let store = Store::connect(db_path).await.unwrap();
    store
        .create_user(&UserRecord {
            id: Uuid::new_v4(),
            email: TEST_EMAIL.to_string(),
            password_hash: hash_password(TEST_PASSWORD),
            role: "admin".to_string(),
        })
        .await
        .unwrap();
    store
}

/// In-memory app with one seeded admin, plus a token from a real login.
async fn test_app() -> (axum::Router, String) {
    let app = router(seeded_store(":memory:").await, TEST_SECRET);
    let token = login(&app, TEST_EMAIL, TEST_PASSWORD).await;
    (app, token)
}

async fn login(app: &axum::Router, email: &str, password: &str) -> String {
    let resp = app
        .clone()
        .oneshot(post_json(
            "/api/v1/auth/login",
            "",
            &serde_json::json!({ "email": email, "password": password }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    json_body::<TokenResponse>(resp).await.token
}

#[tokio::test]
async fn login_rejects_bad_credentials() {
    let (app, _token) = test_app().await;

    for (email, password) in [
        (TEST_EMAIL, "wrong password"),
        ("nobody@example.com", TEST_PASSWORD),
    ] {
        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/v1/auth/login",
                "",
                &serde_json::json!({ "email": email, "password": password }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn data_endpoints_require_valid_token() {
    let (app, token) = test_app().await;

    // health stays public.
    let resp = app.clone().oneshot(get("/health", "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let expired = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &Claims {
            sub: Uuid::new_v4().to_string(),
            exp: (chrono::Utc::now() - chrono::Duration::hours(2)).timestamp() as usize,
            role: "admin".to_string(),
        },
        &jsonwebtoken::EncodingKey::from_secret(TEST_SECRET.as_bytes()),
    )
    .unwrap();
    let forged = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &Claims {
            sub: Uuid::new_v4().to_string(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp() as usize,
            role: "admin".to_string(),
        },
        &jsonwebtoken::EncodingKey::from_secret(b"attacker-controlled-secret-0123456789"),
    )
    .unwrap();

    for uri in ["/api/v1/forms", "/api/v1/sync/status", "/api/v1/sync/forms"] {
        // no token, malformed token, expired token, token signed elsewhere.
        for bad in ["", "not-a-jwt", &expired, &forged] {
            let resp = app.clone().oneshot(get(uri, bad)).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} with {bad:?}"
            );
        }
        let resp = app.clone().oneshot(get(uri, &token)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri} with valid token");
    }

    // push (POST) is guarded too.
    let resp = app
        .clone()
        .oneshot(post_json(
            "/api/v1/sync/push",
            "",
            &PushRequest {
                submissions: vec![],
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn submissions_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("collecta.db").to_str().unwrap().to_string();

    let mut form = Form::new("Survey");
    form.add_field(FormField::text("site_name", "Site Name").set_required());
    let form_id = form.id;

    let sub_id;
    {
        let store = Store::connect(&db).await.unwrap();
        store.insert_form(&form).await.unwrap();
        let mut sub = Submission::new(form_id, form.version);
        sub.set_value("site_name", FieldValue::Text("Alpha".into()));
        sub_id = sub.id;
        store.insert_submission(&sub).await.unwrap();
    } // store dropped: pool closed, simulating shutdown.

    // reopen the same file: a fresh process would see committed data.
    let store = Store::connect(&db).await.unwrap();
    assert_eq!(store.list_forms().await.unwrap().len(), 1);
    let subs = store.list_submissions(form_id).await.unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].id, sub_id);

    let counts = store.sync_counts().await.unwrap();
    assert_eq!(counts.pending, 1);
    assert_eq!(counts.total, 1);
}

#[tokio::test]
async fn http_submit_list_and_sync_status() {
    let (app, token) = test_app().await;

    let mut form = Form::new("Site");
    form.add_field(FormField::text("site_name", "Site Name").set_required());
    let form_id = form.id;

    let resp = app
        .clone()
        .oneshot(post_json("/api/v1/forms", &token, &form))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let subs_uri = format!("/api/v1/forms/{form_id}/submissions");

    let mut good = Submission::new(form_id, form.version);
    good.set_value("site_name", FieldValue::Text("Alpha".into()));
    let resp = app
        .clone()
        .oneshot(post_json(&subs_uri, &token, &good))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // missing required field: rejected by the real validation engine.
    let bad = Submission::new(form_id, form.version);
    let resp = app
        .clone()
        .oneshot(post_json(&subs_uri, &token, &bad))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let resp = app.clone().oneshot(get(&subs_uri, &token)).await.unwrap();
    let stored: Vec<Submission> = json_body(resp).await;
    assert_eq!(stored.len(), 1);

    let resp = app
        .clone()
        .oneshot(get("/api/v1/sync/status", &token))
        .await
        .unwrap();
    let status = body_string(resp).await;
    assert!(status.contains("\"pending\":1"), "got {status}");
    assert!(status.contains("\"total\":1"), "got {status}");
}

#[tokio::test]
async fn http_import_xlsform() {
    let (app, token) = test_app().await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/forms/import")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(tiny_xlsform()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(get("/api/v1/forms", &token))
        .await
        .unwrap();
    let forms: Vec<serde_json::Value> = json_body(resp).await;
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0]["title"], "Imported");
}

#[tokio::test]
async fn sync_push_is_idempotent() {
    let (app, token) = test_app().await;

    let mut form = Form::new("Survey");
    form.add_field(FormField::text("site_name", "Site Name").set_required());
    let resp = app
        .clone()
        .oneshot(post_json("/api/v1/forms", &token, &form))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let mut a = Submission::new(form.id, form.version);
    a.set_value("site_name", FieldValue::Text("Alpha".into()));
    let mut b = Submission::new(form.id, form.version);
    b.set_value("site_name", FieldValue::Text("Beta".into()));
    let invalid = Submission::new(form.id, form.version); // missing required field
    let unknown_form = Submission::new(Uuid::new_v4(), 1);
    let batch = PushRequest {
        submissions: vec![a.clone(), b.clone(), invalid.clone(), unknown_form.clone()],
    };

    let resp = app
        .clone()
        .oneshot(post_json("/api/v1/sync/push", &token, &batch))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let first: PushResponse = json_body(resp).await;
    let statuses: Vec<_> = first.results.iter().map(|r| (r.id, r.status)).collect();
    assert_eq!(
        statuses,
        vec![
            (a.id, PushItemStatus::Accepted),
            (b.id, PushItemStatus::Accepted),
            (invalid.id, PushItemStatus::Error),
            (unknown_form.id, PushItemStatus::Error),
        ]
    );

    // same batch again: valid items are duplicates, nothing new is stored.
    let resp = app
        .clone()
        .oneshot(post_json("/api/v1/sync/push", &token, &batch))
        .await
        .unwrap();
    let second: PushResponse = json_body(resp).await;
    assert_eq!(second.results[0].status, PushItemStatus::Duplicate);
    assert_eq!(second.results[1].status, PushItemStatus::Duplicate);
    assert_eq!(second.results[2].status, PushItemStatus::Error);
    assert_eq!(second.results[3].status, PushItemStatus::Error);

    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/forms/{}/submissions", form.id),
            &token,
        ))
        .await
        .unwrap();
    let stored: Vec<Submission> = json_body(resp).await;
    assert_eq!(stored.len(), 2, "re-push must not duplicate rows");
}

#[tokio::test]
async fn sync_forms_since_cursor() {
    let (app, token) = test_app().await;

    let form_a = Form::new("First");
    let resp = app
        .clone()
        .oneshot(post_json("/api/v1/forms", &token, &form_a))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // initial pull (no cursor) sees everything.
    let resp = app
        .clone()
        .oneshot(get("/api/v1/sync/forms", &token))
        .await
        .unwrap();
    let pull: FormsPullResponse = json_body(resp).await;
    assert_eq!(pull.forms.len(), 1);
    assert_eq!(pull.forms[0].id, form_a.id);
    assert!(!pull.cursor.is_empty());

    let form_b = Form::new("Second");
    let resp = app
        .clone()
        .oneshot(post_json("/api/v1/forms", &token, &form_b))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // incremental pull from the cursor sees only the newer form.
    let uri = format!("/api/v1/sync/forms?since={}", urlencode(&pull.cursor));
    let resp = app.clone().oneshot(get(&uri, &token)).await.unwrap();
    let pull2: FormsPullResponse = json_body(resp).await;
    assert_eq!(pull2.forms.len(), 1);
    assert_eq!(pull2.forms[0].id, form_b.id);

    // nothing changed since the newest cursor: empty pull, cursor echoed.
    let uri = format!("/api/v1/sync/forms?since={}", urlencode(&pull2.cursor));
    let resp = app.clone().oneshot(get(&uri, &token)).await.unwrap();
    let pull3: FormsPullResponse = json_body(resp).await;
    assert!(pull3.forms.is_empty());
    assert_eq!(pull3.cursor, pull2.cursor);
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

fn tiny_xlsform() -> Vec<u8> {
    let mut workbook = Workbook::new();
    let survey = workbook.add_worksheet();
    survey.set_name("survey").unwrap();
    for (c, val) in ["type", "name", "label", "required"].iter().enumerate() {
        survey.write_string(0, c as u16, *val).unwrap();
    }
    for (c, val) in ["text", "q1", "Question 1", "yes"].iter().enumerate() {
        survey.write_string(1, c as u16, *val).unwrap();
    }
    let settings = workbook.add_worksheet();
    settings.set_name("settings").unwrap();
    settings.write_string(0, 0, "form_title").unwrap();
    settings.write_string(1, 0, "Imported").unwrap();
    workbook.save_to_buffer().unwrap()
}

fn post_json<T: Serialize>(uri: &str, token: &str, value: &T) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    builder
        .body(Body::from(serde_json::to_vec(value).unwrap()))
        .unwrap()
}

fn get(uri: &str, token: &str) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn body_string(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json_body<T: DeserializeOwned>(resp: Response) -> T {
    serde_json::from_str(&body_string(resp).await).unwrap()
}
