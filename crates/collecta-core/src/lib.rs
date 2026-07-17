//! # collecta-core
//!
//! Field data collection engine — schema-driven forms, offline-first
//! data capture, validation, and sync queue for the TileTopia ecosystem.

pub mod attachment;
pub mod error;
pub mod form;
pub mod submission;
pub mod sync_protocol;
pub mod sync_queue;
pub mod validation;

pub use error::Error;
pub use form::{FieldType, Form, FormField};
pub use submission::{FieldValue, Submission};
pub use sync_queue::{SyncQueue, SyncStatus};
