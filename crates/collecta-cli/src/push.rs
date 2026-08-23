//! Draining the queue to `POST /api/v1/sync/push`.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use collecta_core::sync_protocol::{PushItemStatus, PushRequest, PushResponse};
use collecta_core::{SyncQueue, SyncStatus};

use crate::Result;
use crate::queue_file;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Push everything due, then record the per-item results in the queue file.
///
/// A request that never reaches the server counts as one failed attempt for
/// every item in it, so those items are retried later rather than dropped.
pub fn run(queue_path: &Path, server: &str, token: Option<&str>) -> Result<()> {
    let mut queue = queue_file::load(queue_path)?;
    let request = queue.build_push_request(Utc::now());
    if request.submissions.is_empty() {
        println!("{}", nothing_due(&queue));
        return Ok(());
    }

    println!("pushing to {server}");
    let outcome = post(server, token, &request);
    match outcome {
        Ok(response) => {
            report(&response);
            queue.apply_push_response(&response);
            queue_file::save(queue_path, &queue)?;
            summarize(&queue);
            Ok(())
        }
        Err(message) => {
            for submission in &request.submissions {
                queue.mark_failed(submission.id, message.clone());
            }
            queue_file::save(queue_path, &queue)?;
            summarize(&queue);
            Err(message.into())
        }
    }
}

fn post(
    server: &str,
    token: Option<&str>,
    request: &PushRequest,
) -> std::result::Result<PushResponse, String> {
    let url = format!("{}/api/v1/sync/push", server.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let mut builder = client.post(&url).json(request);
    if let Some(token) = token {
        builder = builder.bearer_auth(token);
    }
    let response = builder.send().map_err(|error| error.to_string())?;

    let status = response.status();
    if status.is_success() {
        return response.json().map_err(|error| error.to_string());
    }
    let body = response.text().unwrap_or_default();
    let detail = body.trim();
    if detail.is_empty() {
        return Err(format!("server returned {status}"));
    }
    Err(format!("server returned {status}: {detail}"))
}

fn report(response: &PushResponse) {
    for result in &response.results {
        let label = match result.status {
            PushItemStatus::Accepted => "accepted",
            PushItemStatus::Duplicate => "duplicate",
            PushItemStatus::Error => "error",
        };
        match &result.message {
            Some(message) => println!("  {label} {}: {message}", result.id),
            None => println!("  {label} {}", result.id),
        }
    }
}

fn nothing_due(queue: &SyncQueue) -> String {
    let waiting = queue.pending().len();
    if waiting == 0 {
        return format!("nothing to push, {} queued", queue.len());
    }
    format!("nothing due yet, {waiting} still inside the retry backoff")
}

fn summarize(queue: &SyncQueue) {
    println!(
        "{} synced, {} to retry, {} abandoned",
        queue.count_by_status(SyncStatus::Synced),
        queue.count_by_status(SyncStatus::Failed),
        queue.count_by_status(SyncStatus::Abandoned),
    );
}
