//! Sync queue — offline-first submission queue with retry logic.
//!
//! Submissions are stored locally and synced to the server when connectivity
//! is available. Failed syncs are retried with exponential backoff.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::submission::Submission;
use crate::sync_protocol::{PushItemStatus, PushRequest, PushResponse};

/// Offline sync queue — stores submissions pending upload.
///
/// The whole queue serializes, so a client can keep it in a file between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncQueue {
    items: Vec<QueueItem>,
    max_retries: u32,
}

/// An item in the sync queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    /// Submission to sync.
    pub submission: Submission,
    /// Current sync status.
    pub status: SyncStatus,
    /// Number of failed attempts.
    pub retry_count: u32,
    /// When the item was queued.
    pub queued_at: DateTime<Utc>,
    /// When the last sync attempt was made.
    pub last_attempt: Option<DateTime<Utc>>,
    /// Error message from last failed attempt.
    pub last_error: Option<String>,
}

/// Sync status for a queued item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncStatus {
    /// Waiting to be synced.
    Pending,
    /// Currently being uploaded.
    InProgress,
    /// Successfully synced.
    Synced,
    /// Failed — will retry.
    Failed,
    /// Permanently failed (max retries exceeded).
    Abandoned,
}

impl SyncQueue {
    /// Create a new queue with default max retries (5).
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            max_retries: 5,
        }
    }

    /// Create a queue with custom max retries.
    pub fn with_max_retries(max_retries: u32) -> Self {
        Self {
            items: Vec::new(),
            max_retries,
        }
    }

    /// Enqueue a completed submission for sync.
    pub fn enqueue(&mut self, submission: Submission) {
        self.items.push(QueueItem {
            submission,
            status: SyncStatus::Pending,
            retry_count: 0,
            queued_at: Utc::now(),
            last_attempt: None,
            last_error: None,
        });
    }

    /// Get all items pending sync (Pending or Failed with retries remaining).
    pub fn pending(&self) -> Vec<&QueueItem> {
        self.items
            .iter()
            .filter(|item| {
                item.status == SyncStatus::Pending
                    || (item.status == SyncStatus::Failed && item.retry_count < self.max_retries)
            })
            .collect()
    }

    /// Mark an item as successfully synced.
    pub fn mark_synced(&mut self, submission_id: Uuid) {
        if let Some(item) = self.find_mut(submission_id) {
            item.status = SyncStatus::Synced;
            item.last_attempt = Some(Utc::now());
        }
    }

    /// Mark an item as failed (will retry if under max_retries).
    pub fn mark_failed(&mut self, submission_id: Uuid, error: String) {
        let max_retries = self.max_retries;
        if let Some(item) = self.find_mut(submission_id) {
            item.retry_count += 1;
            item.last_attempt = Some(Utc::now());
            item.last_error = Some(error);

            if item.retry_count >= max_retries {
                item.status = SyncStatus::Abandoned;
            } else {
                item.status = SyncStatus::Failed;
            }
        }
    }

    /// Every item in the queue, in the order they were enqueued.
    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    /// Items due for a push attempt at `now`: everything pending, minus the
    /// failed items still inside their backoff window.
    pub fn due(&self, now: DateTime<Utc>) -> Vec<&QueueItem> {
        self.pending()
            .into_iter()
            .filter(|item| self.next_attempt_at(item).is_none_or(|due| due <= now))
            .collect()
    }

    /// When `item` may next be attempted, or `None` when it may be attempted
    /// now because nothing has been tried yet.
    pub fn next_attempt_at(&self, item: &QueueItem) -> Option<DateTime<Utc>> {
        let last_attempt = item.last_attempt?;
        // the first retry waits the 5s base, and by then one attempt has
        // already been counted.
        let wait = Self::backoff_seconds(item.retry_count.saturating_sub(1));
        Some(last_attempt + Duration::seconds(wait as i64))
    }

    /// Get total number of items in queue.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Count of items by status.
    pub fn count_by_status(&self, status: SyncStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    /// Build a push batch from every item due for an attempt at `now`.
    pub fn build_push_request(&self, now: DateTime<Utc>) -> PushRequest {
        PushRequest {
            submissions: self
                .due(now)
                .iter()
                .map(|item| item.submission.clone())
                .collect(),
        }
    }

    /// Apply the server's per-item push results to the queue.
    ///
    /// `Accepted` and `Duplicate` both mean the server has the submission,
    /// so both mark the item synced; `Error` marks it failed for retry.
    pub fn apply_push_response(&mut self, response: &PushResponse) {
        for result in &response.results {
            match result.status {
                PushItemStatus::Accepted | PushItemStatus::Duplicate => {
                    self.mark_synced(result.id);
                }
                PushItemStatus::Error => {
                    let message = result
                        .message
                        .clone()
                        .unwrap_or_else(|| "server rejected submission".to_string());
                    self.mark_failed(result.id, message);
                }
            }
        }
    }

    /// Get backoff duration in seconds for a given retry count.
    pub fn backoff_seconds(retry_count: u32) -> u64 {
        // Exponential: 2^retry * 5 seconds, capped at 5 minutes
        let secs = 5u64 * 2u64.pow(retry_count);
        secs.min(300)
    }

    fn find_mut(&mut self, submission_id: Uuid) -> Option<&mut QueueItem> {
        self.items
            .iter_mut()
            .find(|item| item.submission.id == submission_id)
    }
}

impl Default for SyncQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enqueue_and_pending() {
        let mut queue = SyncQueue::new();
        let sub = Submission::new(Uuid::new_v4(), 1);
        queue.enqueue(sub);

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pending().len(), 1);
    }

    #[test]
    fn test_mark_synced() {
        let mut queue = SyncQueue::new();
        let sub = Submission::new(Uuid::new_v4(), 1);
        let sub_id = sub.id;
        queue.enqueue(sub);

        queue.mark_synced(sub_id);
        assert_eq!(queue.count_by_status(SyncStatus::Synced), 1);
        assert_eq!(queue.pending().len(), 0);
    }

    #[test]
    fn test_retry_and_abandon() {
        let mut queue = SyncQueue::with_max_retries(3);
        let sub = Submission::new(Uuid::new_v4(), 1);
        let sub_id = sub.id;
        queue.enqueue(sub);

        // Fail 3 times → abandoned
        queue.mark_failed(sub_id, "timeout".to_string());
        assert_eq!(queue.count_by_status(SyncStatus::Failed), 1);
        assert_eq!(queue.pending().len(), 1); // still retryable

        queue.mark_failed(sub_id, "timeout".to_string());
        assert_eq!(queue.pending().len(), 1);

        queue.mark_failed(sub_id, "timeout".to_string());
        assert_eq!(queue.count_by_status(SyncStatus::Abandoned), 1);
        assert_eq!(queue.pending().len(), 0); // no longer retryable
    }

    #[test]
    fn test_push_round_trip() {
        use crate::sync_protocol::{PushItemResult, PushItemStatus, PushResponse};

        let mut queue = SyncQueue::new();
        let ok = Submission::new(Uuid::new_v4(), 1);
        let dup = Submission::new(Uuid::new_v4(), 1);
        let bad = Submission::new(Uuid::new_v4(), 1);
        let (ok_id, dup_id, bad_id) = (ok.id, dup.id, bad.id);
        queue.enqueue(ok);
        queue.enqueue(dup);
        queue.enqueue(bad);

        let request = queue.build_push_request(Utc::now());
        assert_eq!(request.submissions.len(), 3);

        let response = PushResponse {
            results: vec![
                PushItemResult {
                    id: ok_id,
                    status: PushItemStatus::Accepted,
                    message: None,
                },
                PushItemResult {
                    id: dup_id,
                    status: PushItemStatus::Duplicate,
                    message: None,
                },
                PushItemResult {
                    id: bad_id,
                    status: PushItemStatus::Error,
                    message: Some("missing required field".to_string()),
                },
            ],
        };
        queue.apply_push_response(&response);

        assert_eq!(queue.count_by_status(SyncStatus::Synced), 2);
        assert_eq!(queue.count_by_status(SyncStatus::Failed), 1);
        // only the failed item is retried, and only once its backoff has passed.
        let after_backoff = Utc::now() + Duration::seconds(5);
        assert!(queue.build_push_request(Utc::now()).submissions.is_empty());
        assert_eq!(queue.build_push_request(after_backoff).submissions.len(), 1);
        assert_eq!(
            queue.build_push_request(after_backoff).submissions[0].id,
            bad_id
        );
    }

    #[test]
    fn test_backoff_gates_retry() {
        let mut queue = SyncQueue::new();
        let submission = Submission::new(Uuid::new_v4(), 1);
        let submission_id = submission.id;
        queue.enqueue(submission);
        assert_eq!(queue.due(Utc::now()).len(), 1);

        queue.mark_failed(submission_id, "connection refused".to_string());
        let attempted_at = queue.items()[0].last_attempt.unwrap();
        assert!(queue.due(attempted_at + Duration::seconds(4)).is_empty());
        assert_eq!(queue.due(attempted_at + Duration::seconds(5)).len(), 1);

        queue.mark_failed(submission_id, "connection refused".to_string());
        let attempted_at = queue.items()[0].last_attempt.unwrap();
        assert!(queue.due(attempted_at + Duration::seconds(9)).is_empty());
        assert_eq!(queue.due(attempted_at + Duration::seconds(10)).len(), 1);
    }

    #[test]
    fn test_queue_round_trips_through_json() {
        let mut queue = SyncQueue::with_max_retries(3);
        let submission = Submission::new(Uuid::new_v4(), 1);
        let submission_id = submission.id;
        queue.enqueue(submission);
        queue.enqueue(Submission::new(Uuid::new_v4(), 2));
        queue.mark_failed(submission_id, "connection refused".to_string());

        let json = serde_json::to_string(&queue).unwrap();
        let mut restored: SyncQueue = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored.items()[0].submission.id, submission_id);
        assert_eq!(restored.items()[0].status, SyncStatus::Failed);
        assert_eq!(restored.items()[0].retry_count, 1);
        assert_eq!(
            restored.items()[0].last_error.as_deref(),
            Some("connection refused")
        );
        // max_retries survives, so a restored queue abandons at the same point.
        restored.mark_failed(submission_id, "connection refused".to_string());
        restored.mark_failed(submission_id, "connection refused".to_string());
        assert_eq!(restored.count_by_status(SyncStatus::Abandoned), 1);
    }

    #[test]
    fn test_backoff() {
        assert_eq!(SyncQueue::backoff_seconds(0), 5);
        assert_eq!(SyncQueue::backoff_seconds(1), 10);
        assert_eq!(SyncQueue::backoff_seconds(2), 20);
        assert_eq!(SyncQueue::backoff_seconds(6), 300); // capped
    }
}
