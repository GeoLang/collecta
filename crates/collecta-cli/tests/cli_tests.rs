//! End-to-end tests for the CLI binary against a stub push endpoint.
//!
//! The stub replaces the server, never the queue: every assertion reads the
//! queue file the binary actually wrote.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::response::Json;
use axum::routing::post;
use chrono::{Duration, Utc};
use collecta_core::submission::FieldValue;
use collecta_core::sync_protocol::{PushItemResult, PushItemStatus, PushRequest, PushResponse};
use collecta_core::{Submission, SyncQueue, SyncStatus};
use tempfile::TempDir;
use uuid::Uuid;

fn run_cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_collecta-cli"))
        .args(arguments)
        .output()
        .expect("failed to run collecta-cli")
}

fn write_submission(directory: &Path, name: &str) -> Submission {
    let mut submission = Submission::new(Uuid::new_v4(), 1);
    submission.set_value("site_name", FieldValue::Text(name.to_string()));
    submission.complete();
    std::fs::write(
        directory.join(format!("{name}.json")),
        serde_json::to_vec(&submission).unwrap(),
    )
    .unwrap();
    submission
}

fn read_queue(path: &Path) -> SyncQueue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn item_status(queue: &SyncQueue, submission_id: Uuid) -> SyncStatus {
    queue
        .items()
        .iter()
        .find(|item| item.submission.id == submission_id)
        .expect("submission missing from queue")
        .status
}

/// Move every recorded attempt back an hour, which is how a test gets past the
/// backoff window without waiting it out.
fn rewind_attempts(path: &Path) {
    let mut queue: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let earlier = (Utc::now() - Duration::hours(1)).to_rfc3339();
    for item in queue["items"].as_array_mut().unwrap() {
        if !item["last_attempt"].is_null() {
            item["last_attempt"] = serde_json::Value::String(earlier.clone());
        }
    }
    std::fs::write(path, serde_json::to_vec(&queue).unwrap()).unwrap();
}

#[derive(Clone)]
struct StubState {
    received: Arc<Mutex<Vec<PushRequest>>>,
    authorizations: Arc<Mutex<Vec<String>>>,
    rejected: Arc<Vec<Uuid>>,
}

struct StubServer {
    base_url: String,
    received: Arc<Mutex<Vec<PushRequest>>>,
    authorizations: Arc<Mutex<Vec<String>>>,
}

impl StubServer {
    fn pushed_ids(&self) -> Vec<Uuid> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .flat_map(|request| request.submissions.iter().map(|submission| submission.id))
            .collect()
    }

    fn request_count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

async fn stub_push(
    State(state): State<StubState>,
    headers: HeaderMap,
    Json(request): Json<PushRequest>,
) -> Json<PushResponse> {
    if let Some(authorization) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        state
            .authorizations
            .lock()
            .unwrap()
            .push(authorization.to_string());
    }
    let results = request
        .submissions
        .iter()
        .map(|submission| {
            if state.rejected.contains(&submission.id) {
                PushItemResult {
                    id: submission.id,
                    status: PushItemStatus::Error,
                    message: Some("missing required field: site_name".to_string()),
                }
            } else {
                PushItemResult {
                    id: submission.id,
                    status: PushItemStatus::Accepted,
                    message: None,
                }
            }
        })
        .collect();
    state.received.lock().unwrap().push(request);
    Json(PushResponse { results })
}

fn start_stub_server(rejected: Vec<Uuid>) -> StubServer {
    let state = StubState {
        received: Arc::new(Mutex::new(Vec::new())),
        authorizations: Arc::new(Mutex::new(Vec::new())),
        rejected: Arc::new(rejected),
    };
    let router = Router::new()
        .route("/api/v1/sync/push", post(stub_push))
        .with_state(state.clone());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener =
        runtime.block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap() });
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        runtime.block_on(async { axum::serve(listener, router).await.unwrap() });
    });

    StubServer {
        base_url: format!("http://{address}"),
        received: state.received,
        authorizations: state.authorizations,
    }
}

/// An address nothing is listening on, standing in for a server that is down.
fn closed_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

fn queue_path(directory: &TempDir) -> PathBuf {
    directory.path().join("queue.json")
}

#[test]
fn submit_queues_offline_and_survives_across_runs() {
    let directory = TempDir::new().unwrap();
    let path = queue_path(&directory);
    let first = write_submission(directory.path(), "alpha");
    let second = write_submission(directory.path(), "beta");

    for name in ["alpha", "beta"] {
        let output = run_cli(&[
            "submit",
            directory
                .path()
                .join(format!("{name}.json"))
                .to_str()
                .unwrap(),
            "--queue",
            path.to_str().unwrap(),
        ]);
        assert!(output.status.success(), "submit failed: {output:?}");
    }

    let queue = read_queue(&path);
    assert_eq!(queue.len(), 2);
    assert_eq!(item_status(&queue, first.id), SyncStatus::Pending);
    assert_eq!(item_status(&queue, second.id), SyncStatus::Pending);

    let status = run_cli(&["status", "--queue", path.to_str().unwrap()]);
    let listing = String::from_utf8(status.stdout).unwrap();
    assert!(status.status.success());
    assert!(listing.contains(&first.id.to_string()), "{listing}");
    assert!(listing.contains(&second.id.to_string()), "{listing}");
    assert!(listing.contains("2 queued: 2 pending"), "{listing}");
}

#[test]
fn push_after_the_server_returns_marks_the_queued_submission_synced() {
    let directory = TempDir::new().unwrap();
    let path = queue_path(&directory);
    let submission = write_submission(directory.path(), "alpha");

    let submit = run_cli(&[
        "submit",
        directory.path().join("alpha.json").to_str().unwrap(),
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(submit.status.success());

    let down = run_cli(&[
        "push",
        "--server",
        &closed_address(),
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(!down.status.success(), "push should fail with no server");
    let queue = read_queue(&path);
    assert_eq!(item_status(&queue, submission.id), SyncStatus::Failed);
    assert_eq!(queue.items()[0].retry_count, 1);
    assert!(queue.items()[0].last_error.is_some());

    // still inside the 5s backoff, so the retry is held back and nothing is sent.
    let server = start_stub_server(Vec::new());
    let too_soon = run_cli(&[
        "push",
        "--server",
        &server.base_url,
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(too_soon.status.success());
    assert_eq!(server.request_count(), 0);
    assert_eq!(
        item_status(&read_queue(&path), submission.id),
        SyncStatus::Failed
    );

    rewind_attempts(&path);
    let retry = run_cli(&[
        "push",
        "--server",
        &server.base_url,
        "--token",
        "test-token",
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(retry.status.success(), "retry failed: {retry:?}");
    assert_eq!(server.pushed_ids(), vec![submission.id]);
    assert_eq!(
        server.authorizations.lock().unwrap().as_slice(),
        ["Bearer test-token".to_string()]
    );
    assert_eq!(
        item_status(&read_queue(&path), submission.id),
        SyncStatus::Synced
    );

    // a synced item is done, so a later push has nothing to send.
    let after = run_cli(&[
        "push",
        "--server",
        &server.base_url,
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(after.status.success());
    assert_eq!(server.request_count(), 1);
}

#[test]
fn push_applies_per_item_results() {
    let directory = TempDir::new().unwrap();
    let path = queue_path(&directory);
    let accepted = write_submission(directory.path(), "alpha");
    let rejected = write_submission(directory.path(), "beta");
    for name in ["alpha", "beta"] {
        run_cli(&[
            "submit",
            directory
                .path()
                .join(format!("{name}.json"))
                .to_str()
                .unwrap(),
            "--queue",
            path.to_str().unwrap(),
        ]);
    }

    let server = start_stub_server(vec![rejected.id]);
    let output = run_cli(&[
        "push",
        "--server",
        &server.base_url,
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "push failed: {output:?}");
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(
        report.contains("missing required field: site_name"),
        "{report}"
    );

    let queue = read_queue(&path);
    assert_eq!(item_status(&queue, accepted.id), SyncStatus::Synced);
    assert_eq!(item_status(&queue, rejected.id), SyncStatus::Failed);

    // only the rejected item goes out again, once its backoff has passed.
    rewind_attempts(&path);
    let retry = run_cli(&[
        "push",
        "--server",
        &server.base_url,
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(retry.status.success());
    assert_eq!(server.request_count(), 2);
    assert_eq!(
        server.pushed_ids(),
        vec![accepted.id, rejected.id, rejected.id]
    );
}

#[test]
fn push_reports_a_rejecting_server_and_keeps_the_item() {
    let directory = TempDir::new().unwrap();
    let path = queue_path(&directory);
    let submission = write_submission(directory.path(), "alpha");
    run_cli(&[
        "submit",
        directory.path().join("alpha.json").to_str().unwrap(),
        "--queue",
        path.to_str().unwrap(),
    ]);

    let unauthorized = Router::new().route(
        "/api/v1/sync/push",
        post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "missing bearer token") }),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener =
        runtime.block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap() });
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        runtime.block_on(async { axum::serve(listener, unauthorized).await.unwrap() });
    });

    let output = run_cli(&[
        "push",
        "--server",
        &format!("http://{address}"),
        "--queue",
        path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("missing bearer token"), "{message}");

    let queue = read_queue(&path);
    assert_eq!(item_status(&queue, submission.id), SyncStatus::Failed);
    assert_eq!(queue.items()[0].retry_count, 1);
}

#[test]
fn status_on_a_missing_queue_file_reports_empty() {
    let directory = TempDir::new().unwrap();
    let path = queue_path(&directory);
    let output = run_cli(&["status", "--queue", path.to_str().unwrap()]);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("is empty"),
        "unexpected output"
    );
    assert!(!path.exists());
}
