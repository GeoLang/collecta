//! Submissions — captured field data for a form instance.
//!
//! Each submission is a filled-out form: field values, GPS location,
//! timestamps, and metadata. Stored locally offline and synced when connected.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A completed form submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    /// Unique submission ID.
    pub id: Uuid,
    /// Form ID this submission belongs to.
    pub form_id: Uuid,
    /// Form version at time of submission.
    pub form_version: u32,
    /// Field values keyed by field name.
    pub values: HashMap<String, FieldValue>,
    /// When the submission was started.
    pub started_at: DateTime<Utc>,
    /// When the submission was completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Device GPS location at time of submission.
    pub device_location: Option<GeoPoint>,
    /// Collecting user/device identifier.
    pub collector_id: Option<String>,
    /// Submission status.
    pub status: SubmissionStatus,
    /// Attached file references.
    pub attachments: Vec<AttachmentRef>,
}

impl Submission {
    /// Create a new in-progress submission for a form.
    pub fn new(form_id: Uuid, form_version: u32) -> Self {
        Self {
            id: Uuid::new_v4(),
            form_id,
            form_version,
            values: HashMap::new(),
            started_at: Utc::now(),
            completed_at: None,
            device_location: None,
            collector_id: None,
            status: SubmissionStatus::Draft,
            attachments: Vec::new(),
        }
    }

    /// Set a field value.
    pub fn set_value(&mut self, field_name: impl Into<String>, value: FieldValue) {
        self.values.insert(field_name.into(), value);
    }

    /// Mark the submission as complete.
    pub fn complete(&mut self) {
        self.completed_at = Some(Utc::now());
        self.status = SubmissionStatus::Complete;
    }

    /// Check if a field has a value.
    pub fn has_value(&self, field_name: &str) -> bool {
        self.values.contains_key(field_name)
    }
}

/// Value for a single field in a submission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FieldValue {
    /// Text/textarea value.
    Text(String),
    /// Integer value.
    Integer(i64),
    /// Decimal value.
    Decimal(f64),
    /// Boolean value.
    Boolean(bool),
    /// Date string (YYYY-MM-DD).
    Date(String),
    /// DateTime string (ISO 8601).
    DateTime(String),
    /// Time string (HH:MM:SS).
    Time(String),
    /// Single selected choice value.
    Choice(String),
    /// Multiple selected choice values.
    MultiChoice(Vec<String>),
    /// GPS point.
    GeoPoint(GeoPoint),
    /// GPS trace (line).
    GeoTrace(Vec<GeoPoint>),
    /// GPS shape (polygon).
    GeoShape(Vec<GeoPoint>),
    /// Barcode/QR code value.
    Barcode(String),
    /// File attachment reference (UUID).
    Attachment(Uuid),
    /// Repeat group: list of sub-submissions.
    Repeat(Vec<HashMap<String, FieldValue>>),
    /// Null/empty value.
    Null,
}

/// A geographic point (GPS reading).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: Option<f64>,
    pub accuracy: Option<f64>,
}

impl GeoPoint {
    pub fn new(latitude: f64, longitude: f64) -> Self {
        Self {
            latitude,
            longitude,
            altitude: None,
            accuracy: None,
        }
    }
}

/// Submission lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmissionStatus {
    /// Still being filled out.
    Draft,
    /// Complete and ready for sync.
    Complete,
    /// Successfully synced to server.
    Synced,
    /// Sync failed — will retry.
    SyncFailed,
}

/// Reference to an attached file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: Uuid,
    pub field_name: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_submission() {
        let form_id = Uuid::new_v4();
        let mut sub = Submission::new(form_id, 1);
        sub.set_value("site_name", FieldValue::Text("Alpha Site".to_string()));
        sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(51.5, -0.1)));

        assert_eq!(sub.form_id, form_id);
        assert_eq!(sub.status, SubmissionStatus::Draft);
        assert!(sub.has_value("site_name"));
        assert!(!sub.has_value("missing"));
    }

    #[test]
    fn test_complete_submission() {
        let mut sub = Submission::new(Uuid::new_v4(), 1);
        sub.set_value("q1", FieldValue::Text("answer".to_string()));
        sub.complete();

        assert_eq!(sub.status, SubmissionStatus::Complete);
        assert!(sub.completed_at.is_some());
    }

    #[test]
    fn test_repeat_group() {
        let mut sub = Submission::new(Uuid::new_v4(), 1);
        let rows = vec![
            HashMap::from([("item".to_string(), FieldValue::Text("A".to_string()))]),
            HashMap::from([("item".to_string(), FieldValue::Text("B".to_string()))]),
        ];
        sub.set_value("items", FieldValue::Repeat(rows));

        if let Some(FieldValue::Repeat(rows)) = sub.values.get("items") {
            assert_eq!(rows.len(), 2);
        } else {
            panic!("expected repeat value");
        }
    }

    #[test]
    fn test_serialization() {
        let mut sub = Submission::new(Uuid::new_v4(), 1);
        sub.set_value("name", FieldValue::Text("Test".to_string()));
        sub.set_value("count", FieldValue::Integer(42));

        let json = serde_json::to_string(&sub).unwrap();
        let parsed: Submission = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.values.get("name"),
            Some(&FieldValue::Text("Test".to_string()))
        );
    }
}
