// End-to-end tests through the real router: auth (login, token rejection),
// persistence across restart, sync push idempotency, and the forms cursor.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use collecta_core::form::{Form, FormField};
use collecta_core::submission::{FieldValue, Submission};
use collecta_core::sync_protocol::{FormsPullResponse, PushItemStatus, PushRequest, PushResponse};
use collecta_server::auth::{Claims, TokenResponse, hash_password};
use collecta_server::store::{FormOwner, FormWriter, Store, UserRecord};
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

/// App with the seeded admin plus two editors, a viewer, and an account whose
/// stored role is not one this server knows. All share `TEST_PASSWORD`.
async fn authz_app() -> (axum::Router, Store) {
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
    (router(store.clone(), test_config()), store)
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
