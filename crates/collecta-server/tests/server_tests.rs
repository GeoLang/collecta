// End-to-end tests through the real router: auth (login, token rejection),
// persistence across restart, sync push idempotency, and the forms cursor.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use collecta_core::form::{Form, FormField};
use collecta_core::submission::{FieldValue, Submission};
use collecta_core::sync_protocol::{FormsPullResponse, PushItemStatus, PushRequest, PushResponse};
use collecta_server::auth::{Claims, TokenResponse, hash_password};
use collecta_server::store::{AttachmentRow, FormOwner, FormWriter, Store, UserRecord};
use collecta_server::{Config, router};
use rust_xlsxwriter::Workbook;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
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

/// Config for the json-api tests, whose handlers never touch the data dir.
fn test_config() -> Config {
    Config::new(
        TEST_SECRET,
        std::env::temp_dir().join("collecta-tests-unused"),
    )
}

/// In-memory app with one seeded admin, plus a token from a real login.
async fn test_app() -> (axum::Router, String) {
    let app = router(seeded_store(":memory:").await, test_config());
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
        store
            .insert_form(&form, FormWriter::system())
            .await
            .unwrap();
        let mut sub = Submission::new(form_id, form.version);
        sub.set_value("site_name", FieldValue::Text("Alpha".into()));
        sub_id = sub.id;
        assert!(store.insert_submission(&sub).await.unwrap());
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

// two forms sharing an updated_at straddle any timestamp-only cursor: one of
// them is on the wrong side of `>` and never comes back. The rowid tiebreak is
// what makes the pair reachable.
#[tokio::test]
async fn sync_forms_cursor_tiebreaks_identical_timestamps() {
    const SAME_MICROSECOND: &str = "2026-01-01T00:00:00.000000Z";

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("collecta.db").to_str().unwrap().to_string();
    let store = seeded_store(&db).await;

    let form_a = Form::new("First");
    let form_b = Form::new("Second");
    store
        .insert_form(&form_a, FormWriter::system())
        .await
        .unwrap();
    store
        .insert_form(&form_b, FormWriter::system())
        .await
        .unwrap();

    // the clock makes this collision rare, not impossible: force it.
    let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&db))
        .await
        .unwrap();
    sqlx::query("UPDATE forms SET updated_at = ?")
        .bind(SAME_MICROSECOND)
        .execute(&pool)
        .await
        .unwrap();
    let rowid_a: i64 = sqlx::query_scalar("SELECT rowid FROM forms WHERE id = ?")
        .bind(form_a.id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();

    let app = router(store, test_config());
    let token = login(&app, TEST_EMAIL, TEST_PASSWORD).await;

    // a client holding the cursor for form_a must still be given form_b.
    let cursor = format!("{SAME_MICROSECOND}@{rowid_a}");
    let uri = format!("/api/v1/sync/forms?since={}", urlencode(&cursor));
    let resp = app.clone().oneshot(get(&uri, &token)).await.unwrap();
    let pull: FormsPullResponse = json_body(resp).await;
    assert_eq!(pull.forms.len(), 1, "form written in the same microsecond");
    assert_eq!(pull.forms[0].id, form_b.id);

    // and the cursor it returns is exhausted, so form_b is not resent forever.
    let uri = format!("/api/v1/sync/forms?since={}", urlencode(&pull.cursor));
    let resp = app.clone().oneshot(get(&uri, &token)).await.unwrap();
    let pull2: FormsPullResponse = json_body(resp).await;
    assert!(pull2.forms.is_empty());

    // a bare timestamp from a pre-compound client re-delivers its microsecond
    // rather than skipping past it.
    let uri = format!("/api/v1/sync/forms?since={}", urlencode(SAME_MICROSECOND));
    let resp = app.clone().oneshot(get(&uri, &token)).await.unwrap();
    let pull3: FormsPullResponse = json_body(resp).await;
    assert_eq!(pull3.forms.len(), 2);
}

// a database written before forms had a creator must still open, and the forms
// already in it must come back unowned rather than attributed to anyone.
#[tokio::test]
async fn a_database_without_creator_id_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db").to_str().unwrap().to_string();
    let form = one_field_form("Old");

    let pool = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(&db)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE forms (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            version INTEGER NOT NULL,
            data TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO forms (id, title, version, data) VALUES (?, ?, 1, ?)")
        .bind(form.id.to_string())
        .bind(&form.title)
        .bind(serde_json::to_string(&form).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let store = Store::connect(&db).await.unwrap();
    assert_eq!(store.list_forms().await.unwrap().len(), 1);
    assert_eq!(
        store.form_owner(form.id).await.unwrap(),
        Some(FormOwner::Legacy)
    );
}

// ---- authorization -----------------------------------------------------

const EDITOR_A: &str = "editor-a@example.com";
const EDITOR_B: &str = "editor-b@example.com";
const VIEWER: &str = "viewer@example.com";
const UNKNOWN_ROLE: &str = "collector@example.com";

/// Store with the seeded admin plus two editors, a viewer, and an account whose
/// stored role is not one this server knows. All share `TEST_PASSWORD`.
async fn authz_store() -> Store {
    let store = seeded_store(":memory:").await;
    for (email, role) in [
        (EDITOR_A, "editor"),
        (EDITOR_B, "editor"),
        (VIEWER, "viewer"),
        (UNKNOWN_ROLE, "collector"),
    ] {
        store
            .create_user(&UserRecord {
                id: Uuid::new_v4(),
                email: email.to_string(),
                password_hash: hash_password(TEST_PASSWORD),
                role: role.to_string(),
            })
            .await
            .unwrap();
    }
    store
}

async fn authz_app() -> (axum::Router, Store) {
    let store = authz_store().await;
    (router(store.clone(), test_config()), store)
}

/// The same fixture over a real data directory, for the routes that read or
/// remove attachment bytes.
async fn authz_app_with_dir() -> (axum::Router, Store, tempfile::TempDir) {
    let store = authz_store().await;
    let dir = tempfile::tempdir().unwrap();
    let app = router(store.clone(), Config::new(TEST_SECRET, dir.path()));
    (app, store, dir)
}

async fn user_id(store: &Store, email: &str) -> Uuid {
    store.get_user_by_email(email).await.unwrap().unwrap().id
}

fn one_field_form(title: &str) -> Form {
    let mut form = Form::new(title);
    form.add_field(FormField::text("site_name", "Site Name").set_required());
    form
}

fn filled_submission(form: &Form) -> Submission {
    let mut submission = Submission::new(form.id, form.version);
    submission.set_value("site_name", FieldValue::Text("Alpha".into()));
    submission
}

async fn create_form(app: &axum::Router, token: &str, form: &Form) -> StatusCode {
    app.clone()
        .oneshot(post_json("/api/v1/forms", token, form))
        .await
        .unwrap()
        .status()
}

async fn submit(app: &axum::Router, token: &str, form: &Form) -> StatusCode {
    app.clone()
        .oneshot(post_json(
            &format!("/api/v1/forms/{}/submissions", form.id),
            token,
            &filled_submission(form),
        ))
        .await
        .unwrap()
        .status()
}

async fn list_submissions(app: &axum::Router, token: &str, form: &Form) -> Response {
    app.clone()
        .oneshot(get(
            &format!("/api/v1/forms/{}/submissions", form.id),
            token,
        ))
        .await
        .unwrap()
}

#[tokio::test]
async fn viewers_can_read_but_cannot_write() {
    let (app, _store) = authz_app().await;
    let editor = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let viewer = login(&app, VIEWER, TEST_PASSWORD).await;

    let form = one_field_form("Site");
    assert_eq!(create_form(&app, &editor, &form).await, StatusCode::CREATED);

    // discovery is open: a collector has to be able to find a form.
    for uri in [
        "/api/v1/forms".to_string(),
        format!("/api/v1/forms/{}", form.id),
    ] {
        let resp = app.clone().oneshot(get(&uri, &viewer)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    }

    assert_eq!(
        create_form(&app, &viewer, &one_field_form("Mine")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(submit(&app, &viewer, &form).await, StatusCode::FORBIDDEN);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/forms/import")
                .header("Authorization", format!("Bearer {viewer}"))
                .body(Body::from(tiny_xlsform()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = app
        .clone()
        .oneshot(post_json(
            "/api/v1/sync/push",
            &viewer,
            &PushRequest {
                submissions: vec![filled_submission(&form)],
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // nothing a viewer sent got through.
    let resp = list_submissions(&app, &editor, &form).await;
    let stored: Vec<Submission> = json_body(resp).await;
    assert!(stored.is_empty());
}

#[tokio::test]
async fn an_editor_can_submit_to_another_editors_form_but_not_read_it() {
    let (app, _store) = authz_app().await;
    let admin = login(&app, TEST_EMAIL, TEST_PASSWORD).await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;

    let form = one_field_form("Site");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);

    assert_eq!(submit(&app, &a, &form).await, StatusCode::CREATED);
    assert_eq!(submit(&app, &b, &form).await, StatusCode::CREATED);

    // collected data belongs to whoever created the form.
    assert_eq!(
        list_submissions(&app, &b, &form).await.status(),
        StatusCode::FORBIDDEN
    );

    for token in [&a, &admin] {
        let resp = list_submissions(&app, token, &form).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let stored: Vec<Submission> = json_body(resp).await;
        assert_eq!(stored.len(), 2);
    }
}

#[tokio::test]
async fn a_form_id_cannot_be_taken_over() {
    let (app, _store) = authz_app().await;
    let admin = login(&app, TEST_EMAIL, TEST_PASSWORD).await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;

    let form = one_field_form("Site");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);

    // posting a form is an upsert, so reusing an id would otherwise rewrite
    // someone else's form and pull its submissions across with it.
    let mut hijack = one_field_form("Hijacked");
    hijack.id = form.id;
    assert_eq!(create_form(&app, &b, &hijack).await, StatusCode::FORBIDDEN);

    let resp = app
        .clone()
        .oneshot(get(&format!("/api/v1/forms/{}", form.id), &b))
        .await
        .unwrap();
    let stored: Form = json_body(resp).await;
    assert_eq!(stored.title, "Site");

    // an admin may overwrite it, and that does not transfer ownership.
    let mut edited = one_field_form("Edited");
    edited.id = form.id;
    assert_eq!(
        create_form(&app, &admin, &edited).await,
        StatusCode::CREATED
    );
    assert_eq!(
        list_submissions(&app, &a, &form).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        list_submissions(&app, &b, &form).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn forms_with_no_recorded_creator_are_admin_only() {
    let (app, store) = authz_app().await;
    let admin = login(&app, TEST_EMAIL, TEST_PASSWORD).await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;

    // a form from a database written before creators were recorded.
    let form = one_field_form("Legacy");
    store
        .insert_form(&form, FormWriter::system())
        .await
        .unwrap();

    assert_eq!(submit(&app, &a, &form).await, StatusCode::CREATED);
    assert_eq!(
        list_submissions(&app, &a, &form).await.status(),
        StatusCode::FORBIDDEN
    );
    let resp = list_submissions(&app, &admin, &form).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let stored: Vec<Submission> = json_body(resp).await;
    assert_eq!(stored.len(), 1);
}

#[tokio::test]
async fn sync_status_is_admin_only_and_the_form_pull_is_not() {
    let (app, _store) = authz_app().await;
    let admin = login(&app, TEST_EMAIL, TEST_PASSWORD).await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;
    let viewer = login(&app, VIEWER, TEST_PASSWORD).await;

    let form_a = one_field_form("A");
    let form_b = one_field_form("B");
    assert_eq!(create_form(&app, &a, &form_a).await, StatusCode::CREATED);
    assert_eq!(create_form(&app, &b, &form_b).await, StatusCode::CREATED);

    // queue counts cover the whole instance, so they are admin-only.
    let resp = app
        .clone()
        .oneshot(get("/api/v1/sync/status", &a))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = app
        .clone()
        .oneshot(get("/api/v1/sync/status", &admin))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // the form pull is discovery: an editor gets forms they did not create, so
    // they can collect against them offline. A viewer sees them too.
    for token in [&a, &b, &viewer, &admin] {
        let resp = app
            .clone()
            .oneshot(get("/api/v1/sync/forms", token))
            .await
            .unwrap();
        let pull: FormsPullResponse = json_body(resp).await;
        let mut ids: Vec<_> = pull.forms.iter().map(|f| f.id).collect();
        ids.sort();
        let mut expected = vec![form_a.id, form_b.id];
        expected.sort();
        assert_eq!(ids, expected);
    }
}

// a role string this server cannot map to a permission set is not treated as
// the weakest role: it gets nothing at all.
#[tokio::test]
async fn an_unknown_role_is_refused_everywhere() {
    let (app, _store) = authz_app().await;
    let editor = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let form = one_field_form("Site");
    assert_eq!(create_form(&app, &editor, &form).await, StatusCode::CREATED);

    // the credentials are good: authentication is not what fails.
    let token = login(&app, UNKNOWN_ROLE, TEST_PASSWORD).await;

    for uri in [
        "/api/v1/forms".to_string(),
        format!("/api/v1/forms/{}", form.id),
        format!("/api/v1/forms/{}/submissions", form.id),
        "/api/v1/sync/forms".to_string(),
        "/api/v1/sync/status".to_string(),
    ] {
        let resp = app.clone().oneshot(get(&uri, &token)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    assert_eq!(
        create_form(&app, &token, &one_field_form("Mine")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(submit(&app, &token, &form).await, StatusCode::FORBIDDEN);
}

// ---- attachment download -----------------------------------------------

#[tokio::test]
async fn an_attachment_is_readable_by_exactly_who_may_read_its_submission() {
    let (app, store, dir) = authz_app_with_dir().await;
    let admin = login(&app, TEST_EMAIL, TEST_PASSWORD).await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;

    let form = one_field_form("Site");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);
    let submission = filled_submission(&form);
    assert_eq!(
        post_submission(&app, &a, &submission).await,
        StatusCode::CREATED
    );
    let attachment = store_attachment(&store, &dir, submission.id, b"JPEGBYTES").await;
    let uri = format!("/api/v1/attachments/{attachment}");

    // the form's creator gets the bytes back, unchanged, under the type they
    // were stored with.
    let resp = app.clone().oneshot(get(&uri, &a)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "content-type"), "image/jpeg");
    // never inline: the type and name came off a field device.
    assert_eq!(header(&resp, "content-disposition"), "attachment");
    assert_eq!(header(&resp, "x-content-type-options"), "nosniff");
    assert_eq!(body_bytes(resp).await, b"JPEGBYTES");

    // an admin too, an unrelated editor not. A refusal is a 404, the same as an
    // id nobody stored, so holding a guessed id cannot confirm it exists.
    assert_eq!(
        app.clone()
            .oneshot(get(&uri, &admin))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone().oneshot(get(&uri, &b)).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.clone().oneshot(get(&uri, "")).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    // an id nobody stored is a 404, not a disk read.
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/v1/attachments/{}", Uuid::new_v4()), &a))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- deletes -----------------------------------------------------------

#[tokio::test]
async fn deleting_a_form_tombstones_it_and_takes_its_data_with_it() {
    let (app, store, dir) = authz_app_with_dir().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;
    let viewer = login(&app, VIEWER, TEST_PASSWORD).await;

    let form = one_field_form("Site");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);
    let submission = filled_submission(&form);
    assert_eq!(
        post_submission(&app, &a, &submission).await,
        StatusCode::CREATED
    );
    let attachment = store_attachment(&store, &dir, submission.id, b"JPEGBYTES").await;
    let file = store.list_attachments(submission.id).await.unwrap()[0]
        .storage_path
        .clone();

    // a cursor from before the delete, so the pull below has something to be
    // measured against.
    let resp = app
        .clone()
        .oneshot(get("/api/v1/sync/forms", &a))
        .await
        .unwrap();
    let before: FormsPullResponse = json_body(resp).await;
    assert!(before.deleted.is_empty());

    let uri = format!("/api/v1/forms/{}", form.id);
    assert_eq!(
        app.clone().oneshot(del(&uri, &b)).await.unwrap().status(),
        StatusCode::FORBIDDEN,
        "another editor must not be able to delete the form"
    );
    assert_eq!(
        app.clone()
            .oneshot(del(&uri, &viewer))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(!store.list_submissions(form.id).await.unwrap().is_empty());

    assert_eq!(
        app.clone().oneshot(del(&uri, &a)).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    // the form is gone from every read path, and so is everything under it.
    assert_eq!(
        app.clone().oneshot(get(&uri, &a)).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let resp = app.clone().oneshot(get("/api/v1/forms", &a)).await.unwrap();
    let forms: Vec<serde_json::Value> = json_body(resp).await;
    assert!(forms.is_empty());
    assert_eq!(
        list_submissions(&app, &a, &form).await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(store.list_submissions(form.id).await.unwrap().is_empty());
    assert!(
        store
            .list_attachments(submission.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        app.clone()
            .oneshot(get(&format!("/api/v1/attachments/{attachment}"), &a))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        !std::path::Path::new(&file).exists(),
        "{file} still on disk"
    );
    let counts = store.sync_counts().await.unwrap();
    assert_eq!(counts.total, 0, "the queue entry went with the submission");

    // and the delete reaches a client that already pulled the form.
    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/sync/forms?since={}", urlencode(&before.cursor)),
            &a,
        ))
        .await
        .unwrap();
    let pull: FormsPullResponse = json_body(resp).await;
    assert!(pull.forms.is_empty());
    assert_eq!(pull.deleted, vec![form.id], "the tombstone must sync");

    // deleting it again finds nothing left to delete.
    assert_eq!(
        app.clone().oneshot(del(&uri, &a)).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_a_submission_leaves_the_form_and_the_other_submissions() {
    let (app, store, dir) = authz_app_with_dir().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;

    let form = one_field_form("Site");
    let other = one_field_form("Other");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);
    assert_eq!(create_form(&app, &a, &other).await, StatusCode::CREATED);

    let doomed = filled_submission(&form);
    let kept = filled_submission(&form);
    for submission in [&doomed, &kept] {
        assert_eq!(
            post_submission(&app, &a, submission).await,
            StatusCode::CREATED
        );
    }
    let attachment = store_attachment(&store, &dir, doomed.id, b"JPEGBYTES").await;
    let file = store.list_attachments(doomed.id).await.unwrap()[0]
        .storage_path
        .clone();

    let uri = format!("/api/v1/forms/{}/submissions/{}", form.id, doomed.id);
    assert_eq!(
        app.clone().oneshot(del(&uri, &b)).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );

    // naming a form the caller does own does not reach a submission filed
    // under a different one.
    let crossed = format!("/api/v1/forms/{}/submissions/{}", other.id, doomed.id);
    assert_eq!(
        app.clone()
            .oneshot(del(&crossed, &a))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(store.list_submissions(form.id).await.unwrap().len(), 2);

    assert_eq!(
        app.clone().oneshot(del(&uri, &a)).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let stored = store.list_submissions(form.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, kept.id, "the wrong submission was deleted");
    assert!(store.list_attachments(doomed.id).await.unwrap().is_empty());
    assert!(!std::path::Path::new(&file).exists());
    assert_eq!(
        app.clone()
            .oneshot(get(&format!("/api/v1/attachments/{attachment}"), &a))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(store.sync_counts().await.unwrap().total, 1);

    // the form itself survived, and so did the second delete's 404.
    assert_eq!(
        list_submissions(&app, &a, &form).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone().oneshot(del(&uri, &a)).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

// ---- per-form grants ---------------------------------------------------

#[tokio::test]
async fn a_grant_opens_one_form_to_one_account_and_nothing_more() {
    let (app, store, dir) = authz_app_with_dir().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;
    let b_id = user_id(&store, EDITOR_B).await;

    let shared = one_field_form("Shared");
    let private = one_field_form("Private");
    assert_eq!(create_form(&app, &a, &shared).await, StatusCode::CREATED);
    assert_eq!(create_form(&app, &a, &private).await, StatusCode::CREATED);
    let submission = filled_submission(&shared);
    assert_eq!(
        post_submission(&app, &a, &submission).await,
        StatusCode::CREATED
    );
    let attachment = store_attachment(&store, &dir, submission.id, b"JPEGBYTES").await;

    let grants = format!("/api/v1/forms/{}/grants", shared.id);
    let attachment_uri = format!("/api/v1/attachments/{attachment}");

    // nothing is shared to begin with, and the grantee cannot share to itself.
    assert_eq!(
        list_submissions(&app, &b, &shared).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post_json_status(&app, &grants, &b, &serde_json::json!({ "user_id": b_id })).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(get(&grants, &b))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    // a grant to an id nobody holds is refused rather than stored.
    assert_eq!(
        post_json_status(
            &app,
            &grants,
            &a,
            &serde_json::json!({ "user_id": Uuid::new_v4() })
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    assert_eq!(
        post_json_status(&app, &grants, &a, &serde_json::json!({ "user_id": b_id })).await,
        StatusCode::CREATED
    );

    // the grantee now reads that form's submissions and their attachments.
    let resp = list_submissions(&app, &b, &shared).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let stored: Vec<Submission> = json_body(resp).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, submission.id);
    assert_eq!(
        app.clone()
            .oneshot(get(&attachment_uri, &b))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // and nothing else: not the owner's other form, not the delete, not the
    // sharing itself.
    assert_eq!(
        list_submissions(&app, &b, &private).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(del(&format!("/api/v1/forms/{}", shared.id), &b))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(del(
                &format!("/api/v1/forms/{}/submissions/{}", shared.id, submission.id),
                &b,
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(get(&grants, &b))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    // the owner sees who holds it.
    let resp = app.clone().oneshot(get(&grants, &a)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed: Vec<serde_json::Value> = json_body(resp).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["user_id"], b_id.to_string());
    assert_eq!(listed[0]["email"], EDITOR_B);

    // revoking closes it again.
    let revoke = format!("/api/v1/forms/{}/grants/{}", shared.id, b_id);
    assert_eq!(
        app.clone()
            .oneshot(del(&revoke, &b))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(del(&revoke, &a))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        list_submissions(&app, &b, &shared).await.status(),
        StatusCode::FORBIDDEN,
        "a revoked grant must not still read"
    );
    // the attachment closes as a 404, so a revoked grantee holding the id from
    // when they could read cannot even confirm it is still there.
    assert_eq!(
        app.clone()
            .oneshot(get(&attachment_uri, &b))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.clone()
            .oneshot(del(&revoke, &a))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_admin_can_share_a_form_it_does_not_own() {
    let (app, store) = authz_app().await;
    let admin = login(&app, TEST_EMAIL, TEST_PASSWORD).await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let viewer = login(&app, VIEWER, TEST_PASSWORD).await;
    let viewer_id = user_id(&store, VIEWER).await;

    let form = one_field_form("Site");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);
    assert_eq!(submit(&app, &a, &form).await, StatusCode::CREATED);

    let grants = format!("/api/v1/forms/{}/grants", form.id);
    assert_eq!(
        list_submissions(&app, &viewer, &form).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post_json_status(
            &app,
            &grants,
            &admin,
            &serde_json::json!({ "user_id": viewer_id })
        )
        .await,
        StatusCode::CREATED
    );

    // a granted viewer reads that form's data and still cannot write anywhere.
    let resp = list_submissions(&app, &viewer, &form).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let stored: Vec<Submission> = json_body(resp).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(submit(&app, &viewer, &form).await, StatusCode::FORBIDDEN);
}

// ---- submission ownership ----------------------------------------------
//
// `submissions.id` comes from the client and is unique across every form, so a
// write that replaced an existing row would move that row, and the attachments
// hanging off it, under a form the writer controls. Every per-form gate reads
// authority out of that row, so these three cover the whole escape.

#[tokio::test]
async fn a_submission_id_already_on_file_cannot_be_taken_over() {
    let (app, store, dir) = authz_app_with_dir().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;

    let form_a = one_field_form("A's survey");
    assert_eq!(create_form(&app, &a, &form_a).await, StatusCode::CREATED);
    let victim = filled_submission(&form_a);
    assert_eq!(
        post_submission(&app, &a, &victim).await,
        StatusCode::CREATED
    );
    let attachment = store_attachment(&store, &dir, victim.id, b"SECRET-PHOTO-BYTES").await;
    let attachment_uri = format!("/api/v1/attachments/{attachment}");

    // B owns a form of their own and files under it, reusing A's submission id.
    let form_b = one_field_form("B's own form");
    assert_eq!(create_form(&app, &b, &form_b).await, StatusCode::CREATED);
    let mut hijack = filled_submission(&form_b);
    hijack.id = victim.id;
    assert_eq!(
        post_submission(&app, &b, &hijack).await,
        StatusCode::CONFLICT,
        "an id already on file must be refused, not replaced"
    );

    // the attachment stayed with A's form, and its bytes never reached B.
    assert_eq!(
        app.clone()
            .oneshot(get(&attachment_uri, &b))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let resp = app.clone().oneshot(get(&attachment_uri, &a)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"SECRET-PHOTO-BYTES");

    // and A's own row is untouched.
    let resp = list_submissions(&app, &a, &form_a).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rows: Vec<Submission> = json_body(resp).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, victim.id);
    assert_eq!(rows[0].form_id, form_a.id);
    // B's form never received the row either, so the refusal dropped it.
    let resp = list_submissions(&app, &b, &form_b).await;
    let rows: Vec<Submission> = json_body(resp).await;
    assert!(rows.is_empty());
}

#[tokio::test]
async fn a_submission_body_cannot_name_a_form_other_than_the_one_in_the_path() {
    let (app, _store) = authz_app().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;

    let form_a = one_field_form("A's survey");
    let form_b = one_field_form("B's own form");
    assert_eq!(create_form(&app, &a, &form_a).await, StatusCode::CREATED);
    assert_eq!(create_form(&app, &b, &form_b).await, StatusCode::CREATED);

    // posted to B's own form, but the body claims A's. Validating against one
    // form and filing under the other is refused rather than quietly corrected.
    let mut planted = filled_submission(&form_b);
    planted.form_id = form_a.id;
    assert_eq!(
        post_json_status(
            &app,
            &format!("/api/v1/forms/{}/submissions", form_b.id),
            &b,
            &planted
        )
        .await,
        StatusCode::BAD_REQUEST
    );

    let resp = list_submissions(&app, &a, &form_a).await;
    let rows: Vec<Submission> = json_body(resp).await;
    assert!(rows.is_empty(), "B's row must not land in A's form");
}

#[tokio::test]
async fn a_submission_records_the_caller_not_what_the_body_claims() {
    let (app, store) = authz_app().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;
    let a_id = user_id(&store, EDITOR_A).await;
    let b_id = user_id(&store, EDITOR_B).await;

    let form = one_field_form("A's survey");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);

    // B files against A's form, on both ingest paths, claiming to be A.
    let mut posted = filled_submission(&form);
    posted.collector_id = Some(a_id.to_string());
    assert_eq!(
        post_submission(&app, &b, &posted).await,
        StatusCode::CREATED
    );

    let mut pushed = filled_submission(&form);
    pushed.collector_id = Some(a_id.to_string());
    let resp = app
        .clone()
        .oneshot(post_json(
            "/api/v1/sync/push",
            &b,
            &PushRequest {
                submissions: vec![pushed.clone()],
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // and nothing can be filed without a caller to record.
    assert_eq!(
        post_submission(&app, "", &filled_submission(&form)).await,
        StatusCode::UNAUTHORIZED
    );

    let resp = list_submissions(&app, &a, &form).await;
    let rows: Vec<Submission> = json_body(resp).await;
    let submitters: Vec<_> = rows
        .iter()
        .map(|r| (r.id, r.collector_id.clone()))
        .collect();
    assert_eq!(
        submitters,
        vec![
            (posted.id, Some(b_id.to_string())),
            (pushed.id, Some(b_id.to_string())),
        ]
    );
}

#[tokio::test]
async fn a_row_from_before_submitters_were_recorded_reads_as_no_submitter() {
    let (app, store) = authz_app().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;

    let form = one_field_form("A's survey");
    assert_eq!(create_form(&app, &a, &form).await, StatusCode::CREATED);
    // what the old handler stored: the posted body, submitter field and all.
    let legacy = filled_submission(&form);
    assert!(store.insert_submission(&legacy).await.unwrap());

    let resp = list_submissions(&app, &a, &form).await;
    let rows: Vec<serde_json::Value> = json_body(resp).await;
    assert_eq!(rows.len(), 1);
    // null, never an account: a reader can tell "nobody recorded one" from a
    // row that names someone, and no backfill invents an owner for it.
    assert_eq!(rows[0]["collector_id"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_revoked_grantee_cannot_reach_the_shared_forms_attachments_again() {
    let (app, store, dir) = authz_app_with_dir().await;
    let a = login(&app, EDITOR_A, TEST_PASSWORD).await;
    let b = login(&app, EDITOR_B, TEST_PASSWORD).await;
    let b_id = user_id(&store, EDITOR_B).await;

    let shared = one_field_form("Shared");
    assert_eq!(create_form(&app, &a, &shared).await, StatusCode::CREATED);
    let submission = filled_submission(&shared);
    assert_eq!(
        post_submission(&app, &a, &submission).await,
        StatusCode::CREATED
    );
    let attachment = store_attachment(&store, &dir, submission.id, b"SECRET-PHOTO-BYTES").await;
    let attachment_uri = format!("/api/v1/attachments/{attachment}");

    // A shares read, B harvests the ids the share hands over, A takes it back.
    assert_eq!(
        post_json_status(
            &app,
            &format!("/api/v1/forms/{}/grants", shared.id),
            &a,
            &serde_json::json!({ "user_id": b_id })
        )
        .await,
        StatusCode::CREATED
    );
    let resp = list_submissions(&app, &b, &shared).await;
    let seen: Vec<Submission> = json_body(resp).await;
    let harvested = seen[0].id;
    assert_eq!(
        app.clone()
            .oneshot(del(
                &format!("/api/v1/forms/{}/grants/{}", shared.id, b_id),
                &a,
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );

    // re-filing the harvested id under a form of B's own buys nothing back.
    let form_b = one_field_form("B's own form");
    assert_eq!(create_form(&app, &b, &form_b).await, StatusCode::CREATED);
    let mut hijack = filled_submission(&form_b);
    hijack.id = harvested;
    assert_eq!(
        post_submission(&app, &b, &hijack).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.clone()
            .oneshot(get(&attachment_uri, &b))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND,
        "a revoked grant must not leave the attachment bytes reachable"
    );

    // and a read-only grant never became a delete on the grantor's data.
    assert_eq!(
        app.clone()
            .oneshot(del(
                &format!("/api/v1/forms/{}/submissions/{harvested}", form_b.id),
                &b,
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let resp = list_submissions(&app, &a, &shared).await;
    let rows: Vec<Submission> = json_body(resp).await;
    assert_eq!(rows.len(), 1, "A's submission must survive");
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

async fn post_json_status<T: Serialize>(
    app: &axum::Router,
    uri: &str,
    token: &str,
    value: &T,
) -> StatusCode {
    app.clone()
        .oneshot(post_json(uri, token, value))
        .await
        .unwrap()
        .status()
}

async fn post_submission(app: &axum::Router, token: &str, submission: &Submission) -> StatusCode {
    post_json_status(
        app,
        &format!("/api/v1/forms/{}/submissions", submission.form_id),
        token,
        submission,
    )
    .await
}

fn get(uri: &str, token: &str) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn del(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// Store an attachment the way the OpenRosa route does, since the JSON API has
/// no upload of its own: bytes under `<data dir>/attachments/<submission>/`,
/// metadata in the table.
async fn store_attachment(
    store: &Store,
    dir: &tempfile::TempDir,
    submission_id: Uuid,
    bytes: &[u8],
) -> Uuid {
    let id = Uuid::new_v4();
    let directory =
        collecta_server::openrosa::attachments_dir(dir.path()).join(submission_id.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(id.to_string());
    std::fs::write(&path, bytes).unwrap();
    store
        .add_attachment(&AttachmentRow {
            id,
            submission_id,
            field_name: "photo".to_string(),
            filename: "photo1.jpg".to_string(),
            content_type: "image/jpeg".to_string(),
            size_bytes: bytes.len() as u64,
            storage_path: path.to_string_lossy().into_owned(),
        })
        .await
        .unwrap();
    id
}

fn header(resp: &Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("response carries {name}"))
        .to_str()
        .unwrap()
        .to_string()
}

async fn body_bytes(resp: Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

async fn body_string(resp: Response) -> String {
    String::from_utf8(body_bytes(resp).await).unwrap()
}

async fn json_body<T: DeserializeOwned>(resp: Response) -> T {
    serde_json::from_str(&body_string(resp).await).unwrap()
}
