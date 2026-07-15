//! Form schema definitions — the structure of a data collection form.
//!
//! Forms are schema-driven: each form has a list of typed fields with
//! validation rules, conditional visibility, and grouping.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A complete form definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Form {
    /// Unique form identifier.
    pub id: Uuid,
    /// Human-readable form title.
    pub title: String,
    /// Optional description.
    pub description: Option<String>,
    /// Version number (increment on schema changes).
    pub version: u32,
    /// Ordered list of fields.
    pub fields: Vec<FormField>,
}

impl Form {
    /// Create a new empty form with a title.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            description: None,
            version: 1,
            fields: Vec::new(),
        }
    }

    /// Add a field to the form.
    pub fn add_field(&mut self, field: FormField) {
        self.fields.push(field);
    }

    /// Find a field by name.
    pub fn field_by_name(&self, name: &str) -> Option<&FormField> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// A single field in a form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    /// Machine-readable field name (unique within form).
    pub name: String,
    /// Human-readable label.
    pub label: String,
    /// Field data type.
    pub field_type: FieldType,
    /// Whether this field must be filled.
    pub required: bool,
    /// Optional help text displayed below the field.
    pub hint: Option<String>,
    /// Default value (JSON-encoded).
    pub default: Option<serde_json::Value>,
    /// Conditional visibility: if set, field only shows when this condition is met.
    pub relevant: Option<Condition>,
    /// For choice fields: the list of options.
    pub choices: Option<Vec<Choice>>,
    /// Validation constraints.
    pub constraints: Vec<Constraint>,
    /// For repeat groups: nested fields.
    pub children: Option<Vec<FormField>>,
    /// xlsform attributes the engine does not model yet, preserved verbatim
    /// (raw constraint/relevant expressions, choice_filter, enclosing group).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl FormField {
    /// Create a simple text field.
    pub fn text(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            field_type: FieldType::Text,
            required: false,
            hint: None,
            default: None,
            relevant: None,
            choices: None,
            constraints: Vec::new(),
            children: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Create a GPS location field.
    pub fn geopoint(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            field_type: FieldType::GeoPoint,
            required: false,
            hint: None,
            default: None,
            relevant: None,
            choices: None,
            constraints: Vec::new(),
            children: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Mark field as required.
    pub fn set_required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Add a constraint.
    pub fn with_constraint(mut self, constraint: Constraint) -> Self {
        self.constraints.push(constraint);
        self
    }
}

/// Supported field types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldType {
    /// Single-line text.
    Text,
    /// Multi-line text.
    TextArea,
    /// Integer number.
    Integer,
    /// Decimal number.
    Decimal,
    /// Date (YYYY-MM-DD).
    Date,
    /// Date and time.
    DateTime,
    /// Time only.
    Time,
    /// Single-select from choices.
    Select,
    /// Multi-select from choices.
    MultiSelect,
    /// GPS coordinate (lat, lon, altitude, accuracy).
    GeoPoint,
    /// GPS trace (line geometry).
    GeoTrace,
    /// GPS shape (polygon geometry).
    GeoShape,
    /// Photo attachment.
    Photo,
    /// Audio recording.
    Audio,
    /// Video recording.
    Video,
    /// File attachment.
    File,
    /// Barcode/QR code scan.
    Barcode,
    /// Handwritten signature.
    Signature,
    /// Boolean yes/no.
    Boolean,
    /// Repeating group of fields.
    Repeat,
    /// Display-only note (no input).
    Note,
}

/// A choice option for Select/MultiSelect fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    /// Machine-readable value.
    pub value: String,
    /// Human-readable label.
    pub label: String,
}

/// Conditional visibility expression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    /// Field name to evaluate.
    pub field: String,
    /// Operator.
    pub op: ConditionOp,
    /// Value to compare against.
    pub value: serde_json::Value,
}

/// Condition operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConditionOp {
    Equals,
    NotEquals,
    GreaterThan,
    LessThan,
    Contains,
    IsNotEmpty,
}

/// Validation constraint on a field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraint {
    /// Constraint type.
    pub kind: ConstraintKind,
    /// Error message if constraint fails.
    pub message: String,
}

/// Types of validation constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConstraintKind {
    /// Minimum numeric value.
    Min(f64),
    /// Maximum numeric value.
    Max(f64),
    /// Minimum text length.
    MinLength(usize),
    /// Maximum text length.
    MaxLength(usize),
    /// Regex pattern match.
    Pattern(String),
    /// Value must be in a set.
    OneOf(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_form() {
        let mut form = Form::new("Site Inspection");
        form.add_field(FormField::text("site_name", "Site Name").set_required());
        form.add_field(FormField::geopoint("location", "GPS Location").set_required());

        assert_eq!(form.title, "Site Inspection");
        assert_eq!(form.fields.len(), 2);
        assert!(form.fields[0].required);
    }

    #[test]
    fn test_field_by_name() {
        let mut form = Form::new("Test");
        form.add_field(FormField::text("name", "Name"));
        form.add_field(FormField::text("email", "Email"));

        assert!(form.field_by_name("name").is_some());
        assert!(form.field_by_name("missing").is_none());
    }

    #[test]
    fn test_field_with_constraint() {
        let field = FormField::text("age", "Age").with_constraint(Constraint {
            kind: ConstraintKind::Min(0.0),
            message: "Age must be positive".to_string(),
        });
        assert_eq!(field.constraints.len(), 1);
    }

    #[test]
    fn test_form_serialization() {
        let mut form = Form::new("Test Form");
        form.add_field(FormField::text("q1", "Question 1"));
        let json = serde_json::to_string(&form).unwrap();
        let parsed: Form = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, "Test Form");
        assert_eq!(parsed.fields.len(), 1);
    }
}
