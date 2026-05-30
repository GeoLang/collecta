//! Sync queue — offline-first submission queue with retry logic.
//!
//! Submissions are stored locally and synced to the server when connectivity
//! is available. Failed syncs are retried with exponential backoff.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::submission::Submission;

/// Offline sync queue — stores submissions pending upload.
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
    fn test_backoff() {
        assert_eq!(SyncQueue::backoff_seconds(0), 5);
        assert_eq!(SyncQueue::backoff_seconds(1), 10);
        assert_eq!(SyncQueue::backoff_seconds(2), 20);
        assert_eq!(SyncQueue::backoff_seconds(6), 300); // capped
    }
}
