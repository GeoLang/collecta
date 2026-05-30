//! Error types for the collecta engine.

use std::fmt;

/// All possible errors in the collecta system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A required field was not provided.
    RequiredField(String),
    /// A field value failed validation.
    ValidationFailed { field: String, reason: String },
    /// A field reference does not exist in the form schema.
    UnknownField(String),
    /// Submission refers to a non-existent form.
    FormNotFound(String),
    /// Attachment exceeds size limit.
    AttachmentTooLarge { max_bytes: u64, actual_bytes: u64 },
    /// Sync failed with a remote error.
    SyncFailed(String),
    /// JSON serialization/deserialization error.
    SerdeError(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiredField(name) => write!(f, "required field missing: {name}"),
            Self::ValidationFailed { field, reason } => {
                write!(f, "validation failed for '{field}': {reason}")
            }
            Self::UnknownField(name) => write!(f, "unknown field: {name}"),
            Self::FormNotFound(id) => write!(f, "form not found: {id}"),
            Self::AttachmentTooLarge {
                max_bytes,
                actual_bytes,
            } => {
                write!(
                    f,
                    "attachment too large: {actual_bytes} > {max_bytes} bytes"
                )
            }
            Self::SyncFailed(msg) => write!(f, "sync failed: {msg}"),
            Self::SerdeError(msg) => write!(f, "serialization error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
