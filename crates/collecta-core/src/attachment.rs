//! Attachment handling — photos, audio, signatures, files.
//!
//! Manages binary attachments associated with form submissions.
//! Attachments are stored locally and synced separately from submission data.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Maximum attachment size (50 MB).
pub const MAX_ATTACHMENT_SIZE: u64 = 50 * 1024 * 1024;

/// An attachment stored in the local device store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Unique attachment ID.
    pub id: Uuid,
    /// Original filename.
    pub filename: String,
    /// MIME type.
    pub mime_type: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// The binary content.
    #[serde(skip)]
    pub data: Vec<u8>,
    /// Whether this attachment has been synced to server.
    pub synced: bool,
}

impl Attachment {
    /// Create a new attachment from raw bytes.
    pub fn new(filename: impl Into<String>, mime_type: impl Into<String>, data: Vec<u8>) -> Self {
        let size_bytes = data.len() as u64;
        Self {
            id: Uuid::new_v4(),
            filename: filename.into(),
            mime_type: mime_type.into(),
            size_bytes,
            data,
            synced: false,
        }
    }

    /// Check if the attachment exceeds the size limit.
    pub fn exceeds_limit(&self) -> bool {
        self.size_bytes > MAX_ATTACHMENT_SIZE
    }
}

/// Local attachment store (in-memory for now, disk-backed in production).
#[derive(Default)]
pub struct AttachmentStore {
    attachments: Vec<Attachment>,
}

impl AttachmentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store an attachment. Returns the attachment ID.
    pub fn store(&mut self, attachment: Attachment) -> Result<Uuid, crate::error::Error> {
        if attachment.exceeds_limit() {
            return Err(crate::error::Error::AttachmentTooLarge {
                max_bytes: MAX_ATTACHMENT_SIZE,
                actual_bytes: attachment.size_bytes,
            });
        }
        let id = attachment.id;
        self.attachments.push(attachment);
        Ok(id)
    }

    /// Get an attachment by ID.
    pub fn get(&self, id: Uuid) -> Option<&Attachment> {
        self.attachments.iter().find(|a| a.id == id)
    }

    /// Get all unsynced attachments.
    pub fn unsynced(&self) -> Vec<&Attachment> {
        self.attachments.iter().filter(|a| !a.synced).collect()
    }

    /// Mark an attachment as synced.
    pub fn mark_synced(&mut self, id: Uuid) {
        if let Some(a) = self.attachments.iter_mut().find(|a| a.id == id) {
            a.synced = true;
        }
    }

    /// Total storage used in bytes.
    pub fn total_size(&self) -> u64 {
        self.attachments.iter().map(|a| a.size_bytes).sum()
    }

    /// Number of stored attachments.
    pub fn len(&self) -> usize {
        self.attachments.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_attachment() {
        let mut store = AttachmentStore::new();
        let data = vec![0u8; 1024];
        let attachment = Attachment::new("photo.jpg", "image/jpeg", data);
        let id = store.store(attachment).unwrap();

        assert_eq!(store.len(), 1);
        assert!(store.get(id).is_some());
        assert_eq!(store.total_size(), 1024);
    }

    #[test]
    fn test_reject_oversized() {
        let mut store = AttachmentStore::new();
        let data = vec![0u8; (MAX_ATTACHMENT_SIZE + 1) as usize];
        let attachment = Attachment::new("huge.bin", "application/octet-stream", data);
        let result = store.store(attachment);

        assert!(result.is_err());
    }

    #[test]
    fn test_sync_tracking() {
        let mut store = AttachmentStore::new();
        let attachment = Attachment::new("a.jpg", "image/jpeg", vec![1, 2, 3]);
        let id = store.store(attachment).unwrap();

        assert_eq!(store.unsynced().len(), 1);
        store.mark_synced(id);
        assert_eq!(store.unsynced().len(), 0);
    }
}
