//! Draining the queue to `POST /api/v1/sync/push`.

use std::path::Path;

use chrono::Utc;
use collecta_core::sync_protocol::{PushItemStatus, PushResponse};
use collecta_core::{SyncQueue, SyncStatus};

use crate::Result;
use crate::http;
use crate::json_file;

const PUSH_PATH: &str = "/api/v1/sync/push";

/// Push everything due, then record the per-item results in the queue file.
///
/// A request that never reaches the server counts as one failed attempt for
/// every item in it, so those items are retried later rather than dropped.
pub fn run(queue_path: &Path, server: &str, token: Option<&str>) -> Result<()> {
    let mut queue: SyncQueue = json_file::load(queue_path)?;
    let request = queue.build_push_request(Utc::now());
    if request.submissions.is_empty() {
        println!("{}", nothing_due(&queue));
        return Ok(());
    }

    println!("pushing to {server}");
    let outcome: std::result::Result<PushResponse, String> =
        http::post(server, PUSH_PATH, token, &request);
    match outcome {
        Ok(response) => {
            report(&response);
            queue.apply_push_response(&response);
            json_file::save(queue_path, &queue)?;
            summarize(&queue);
            Ok(())
        }
        Err(message) => {
            for submission in &request.submissions {
                queue.mark_failed(submission.id, message.clone());
            }
            json_file::save(queue_path, &queue)?;
            summarize(&queue);
            Err(message.into())
        }
    }
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
