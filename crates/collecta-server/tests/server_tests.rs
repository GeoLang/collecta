// Persistence and endpoint tests: submissions survive a restart, and the HTTP
// surface (submit, list, sync status, xlsform import) works end to end.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use collecta_core::form::{Form, FormField};
use collecta_core::submission::{FieldValue, Submission};
use collecta_server::router;
use collecta_server::store::Store;
use rust_xlsxwriter::Workbook;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tower::ServiceExt;

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
    let app = router(Store::connect(":memory:").await.unwrap());

    let mut form = Form::new("Site");
    form.add_field(FormField::text("site_name", "Site Name").set_required());
    let form_id = form.id;

    let resp = app
        .clone()
        .oneshot(post_json("/api/v1/forms", &form))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let subs_uri = format!("/api/v1/forms/{form_id}/submissions");

    let mut good = Submission::new(form_id, form.version);
    good.set_value("site_name", FieldValue::Text("Alpha".into()));
    let resp = app
        .clone()
        .oneshot(post_json(&subs_uri, &good))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // missing required field: rejected by the real validation engine.
    let bad = Submission::new(form_id, form.version);
    let resp = app
        .clone()
        .oneshot(post_json(&subs_uri, &bad))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let resp = app.clone().oneshot(get(&subs_uri)).await.unwrap();
    let stored: Vec<Submission> = json_body(resp).await;
    assert_eq!(stored.len(), 1);

    let resp = app
        .clone()
        .oneshot(get("/api/v1/sync/status"))
        .await
        .unwrap();
    let status = body_string(resp).await;
    assert!(status.contains("\"pending\":1"), "got {status}");
    assert!(status.contains("\"total\":1"), "got {status}");
}

#[tokio::test]
async fn http_import_xlsform() {
    let app = router(Store::connect(":memory:").await.unwrap());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/forms/import")
                .body(Body::from(tiny_xlsform()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app.clone().oneshot(get("/api/v1/forms")).await.unwrap();
    let forms: Vec<serde_json::Value> = json_body(resp).await;
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0]["title"], "Imported");
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

fn post_json<T: Serialize>(uri: &str, value: &T) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(value).unwrap()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
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
