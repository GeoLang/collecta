//! Collecta command-line client.
//!
//! `submit` enqueues a submission file into a queue on disk, `push` drains that
//! queue to a server, `status` lists what is in it. Only `push` needs the
//! network: a submission is queued whether or not the server can be reached,
//! and stays queued until the server has taken it.

mod push;
mod queue_file;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use collecta_core::{Submission, SyncStatus};

/// Boxed so each command can return whichever io, json or http error it hit.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const DEFAULT_QUEUE_PATH: &str = "./collecta-queue.json";

const USAGE: &str = "usage:
  collecta-cli submit <submission.json> [--queue <path>]
  collecta-cli push --server <url> [--token <jwt>] [--queue <path>]
  collecta-cli status [--queue <path>]

The queue file defaults to ./collecta-queue.json, or $COLLECTA_QUEUE.
The bearer token defaults to $COLLECTA_TOKEN.";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let queue_path = flag(&arguments, "--queue")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("COLLECTA_QUEUE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_QUEUE_PATH));

    let outcome = match arguments.first().map(String::as_str) {
        Some("submit") => match arguments.get(1).filter(|path| !path.starts_with("--")) {
            Some(submission_path) => submit(&queue_path, Path::new(submission_path)),
            None => return usage(),
        },
        Some("push") => match flag(&arguments, "--server") {
            Some(server) => {
                let token =
                    flag(&arguments, "--token").or_else(|| std::env::var("COLLECTA_TOKEN").ok());
                push::run(&queue_path, &server, token.as_deref())
            }
            None => return usage(),
        },
        Some("status") => status(&queue_path),
        _ => return usage(),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn flag(arguments: &[String], name: &str) -> Option<String> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position + 1).cloned()
}

fn submit(queue_path: &Path, submission_path: &Path) -> Result<()> {
    let contents = std::fs::read_to_string(submission_path)?;
    let submission: Submission = serde_json::from_str(&contents)?;
    let submission_id = submission.id;

    let mut queue = queue_file::load(queue_path)?;
    queue.enqueue(submission);
    queue_file::save(queue_path, &queue)?;

    println!(
        "queued {submission_id}, {} awaiting push in {}",
        queue.pending().len(),
        queue_path.display()
    );
    Ok(())
}

fn status(queue_path: &Path) -> Result<()> {
    let queue = queue_file::load(queue_path)?;
    if queue.is_empty() {
        println!("{} is empty", queue_path.display());
        return Ok(());
    }

    let now = Utc::now();
    for item in queue.items() {
        let mut details = vec![format!("queued {}", item.queued_at.to_rfc3339())];
        if item.retry_count > 0 {
            details.push(format!("attempts {}", item.retry_count));
        }
        let next_attempt = queue
            .next_attempt_at(item)
            .filter(|_| retryable(item.status));
        if let Some(next_attempt) = next_attempt {
            details.push(if next_attempt <= now {
                "retry due now".to_string()
            } else {
                format!("retry due {}", next_attempt.to_rfc3339())
            });
        }
        if let Some(error) = &item.last_error {
            details.push(format!("last error: {error}"));
        }
        println!(
            "{} {} ({})",
            item.submission.id,
            label(item.status),
            details.join(", ")
        );
    }

    println!(
        "{} queued: {} pending, {} synced, {} to retry, {} abandoned",
        queue.len(),
        queue.count_by_status(SyncStatus::Pending),
        queue.count_by_status(SyncStatus::Synced),
        queue.count_by_status(SyncStatus::Failed),
        queue.count_by_status(SyncStatus::Abandoned),
    );
    Ok(())
}

fn retryable(status: SyncStatus) -> bool {
    matches!(status, SyncStatus::Pending | SyncStatus::Failed)
}

fn label(status: SyncStatus) -> &'static str {
    match status {
        SyncStatus::Pending => "pending",
        SyncStatus::InProgress => "in-progress",
        SyncStatus::Synced => "synced",
        SyncStatus::Failed => "failed",
        SyncStatus::Abandoned => "abandoned",
    }
}
