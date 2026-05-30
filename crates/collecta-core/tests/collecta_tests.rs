// Comprehensive integration tests for collecta-core.

use collecta_core::error::Error;
use collecta_core::form::*;
use collecta_core::submission::*;
use collecta_core::validation::validate;
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════════════
// Form builder tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_form_builder_basic() {
    let mut form = Form::new("Inspection");
    assert_eq!(form.title, "Inspection");
    assert_eq!(form.version, 1);
    assert!(form.fields.is_empty());

    form.add_field(FormField::text("name", "Name").set_required());
    assert_eq!(form.fields.len(), 1);
}

#[test]
fn test_form_field_by_name_found() {
    let mut form = Form::new("Test");
    form.add_field(FormField::text("alpha", "Alpha"));
    form.add_field(FormField::text("beta", "Beta"));
    let f = form.field_by_name("beta").unwrap();
    assert_eq!(f.label, "Beta");
}

#[test]
fn test_form_field_by_name_not_found() {
    let form = Form::new("Test");
    assert!(form.field_by_name("missing").is_none());
}

#[test]
fn test_form_field_geopoint() {
    let f = FormField::geopoint("loc", "Location");
    assert_eq!(f.field_type, FieldType::GeoPoint);
    assert!(!f.required);
}

#[test]
fn test_form_field_with_constraint() {
    let f = FormField::text("age", "Age").with_constraint(Constraint {
        kind: ConstraintKind::Min(0.0),
        message: "Age must be non-negative".into(),
    });
    assert_eq!(f.constraints.len(), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
// Submission tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_submission_creation() {
    let form_id = Uuid::new_v4();
    let sub = Submission::new(form_id, 1);
    assert_eq!(sub.form_id, form_id);
    assert_eq!(sub.form_version, 1);
    assert_eq!(sub.status, SubmissionStatus::Draft);
    assert!(sub.completed_at.is_none());
    assert!(sub.values.is_empty());
}

#[test]
fn test_submission_set_value() {
    let mut sub = Submission::new(Uuid::new_v4(), 1);
    sub.set_value("name", FieldValue::Text("Test".into()));
    assert!(sub.has_value("name"));
    assert!(!sub.has_value("other"));
}

#[test]
fn test_submission_complete() {
    let mut sub = Submission::new(Uuid::new_v4(), 1);
    assert_eq!(sub.status, SubmissionStatus::Draft);
    sub.complete();
    assert_eq!(sub.status, SubmissionStatus::Complete);
    assert!(sub.completed_at.is_some());
}

#[test]
fn test_submission_geopoint() {
    let gp = GeoPoint::new(51.5074, -0.1278);
    assert_eq!(gp.latitude, 51.5074);
    assert_eq!(gp.longitude, -0.1278);
    assert!(gp.altitude.is_none());
    assert!(gp.accuracy.is_none());
}

#[test]
fn test_submission_serialization_roundtrip() {
    let mut sub = Submission::new(Uuid::new_v4(), 2);
    sub.set_value("name", FieldValue::Text("Site A".into()));
    sub.set_value("count", FieldValue::Integer(42));
    sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(40.7, -74.0)));

    let json = serde_json::to_string(&sub).unwrap();
    let back: Submission = serde_json::from_str(&json).unwrap();
    assert_eq!(back.form_version, 2);
    assert_eq!(
        back.values.get("name"),
        Some(&FieldValue::Text("Site A".into()))
    );
    assert_eq!(back.values.get("count"), Some(&FieldValue::Integer(42)));
}

// ═══════════════════════════════════════════════════════════════════════════
// Validation tests
// ═══════════════════════════════════════════════════════════════════════════

fn sample_form() -> Form {
    let mut form = Form::new("Field Survey");
    form.add_field(FormField::text("site_name", "Site Name").set_required());
    form.add_field(
        FormField::text("notes", "Notes").with_constraint(Constraint {
            kind: ConstraintKind::MaxLength(500),
            message: "Notes too long".into(),
        }),
    );
    form.add_field(FormField::geopoint("location", "GPS").set_required());
    form
}

#[test]
fn test_validate_valid_submission() {
    let form = sample_form();
    let mut sub = Submission::new(form.id, 1);
    sub.set_value("site_name", FieldValue::Text("Alpha Site".into()));
    sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(51.5, -0.1)));

    let errors = validate(&form, &sub);
    assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
}

#[test]
fn test_validate_missing_required() {
    let form = sample_form();
    let sub = Submission::new(form.id, 1);
    // No values set at all

    let errors = validate(&form, &sub);
    // Should flag site_name and location as required
    let required_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e, Error::RequiredField(_)))
        .collect();
    assert_eq!(required_errors.len(), 2);
}

#[test]
fn test_validate_empty_text_is_missing() {
    let form = sample_form();
    let mut sub = Submission::new(form.id, 1);
    sub.set_value("site_name", FieldValue::Text("".into()));
    sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(0.0, 0.0)));

    let errors = validate(&form, &sub);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, Error::RequiredField(f) if f == "site_name"))
    );
}

#[test]
fn test_validate_null_required_field() {
    let form = sample_form();
    let mut sub = Submission::new(form.id, 1);
    sub.set_value("site_name", FieldValue::Null);
    sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(0.0, 0.0)));

    let errors = validate(&form, &sub);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, Error::RequiredField(f) if f == "site_name"))
    );
}

#[test]
fn test_validate_max_length_constraint() {
    let form = sample_form();
    let mut sub = Submission::new(form.id, 1);
    sub.set_value("site_name", FieldValue::Text("X".into()));
    sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(0.0, 0.0)));
    // Notes exceeding 500 chars
    sub.set_value("notes", FieldValue::Text("x".repeat(600)));

    let errors = validate(&form, &sub);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, Error::ValidationFailed { field, .. } if field == "notes"))
    );
}

#[test]
fn test_validate_unknown_field() {
    let form = sample_form();
    let mut sub = Submission::new(form.id, 1);
    sub.set_value("site_name", FieldValue::Text("Test".into()));
    sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(0.0, 0.0)));
    sub.set_value("nonexistent_field", FieldValue::Text("oops".into()));

    let errors = validate(&form, &sub);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, Error::UnknownField(f) if f == "nonexistent_field"))
    );
}

#[test]
fn test_validate_min_constraint() {
    let mut form = Form::new("Numeric Test");
    form.add_field(
        FormField::text("count", "Count")
            .set_required()
            .with_constraint(Constraint {
                kind: ConstraintKind::Min(1.0),
                message: "must be at least 1".into(),
            }),
    );

    let mut sub = Submission::new(form.id, 1);
    sub.set_value("count", FieldValue::Integer(0));

    let errors = validate(&form, &sub);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, Error::ValidationFailed { field, .. } if field == "count"))
    );
}

#[test]
fn test_validate_max_constraint_passes() {
    let mut form = Form::new("Numeric Test");
    form.add_field(
        FormField::text("score", "Score").with_constraint(Constraint {
            kind: ConstraintKind::Max(100.0),
            message: "max 100".into(),
        }),
    );

    let mut sub = Submission::new(form.id, 1);
    sub.set_value("score", FieldValue::Decimal(99.5));

    let errors = validate(&form, &sub);
    assert!(errors.is_empty());
}

#[test]
fn test_validate_one_of_constraint() {
    let mut form = Form::new("Choice Test");
    form.add_field(
        FormField::text("status", "Status")
            .set_required()
            .with_constraint(Constraint {
                kind: ConstraintKind::OneOf(vec![
                    "active".into(),
                    "inactive".into(),
                    "pending".into(),
                ]),
                message: "invalid status".into(),
            }),
    );

    // Valid
    let mut sub = Submission::new(form.id, 1);
    sub.set_value("status", FieldValue::Text("active".into()));
    assert!(validate(&form, &sub).is_empty());

    // Invalid
    let mut sub2 = Submission::new(form.id, 1);
    sub2.set_value("status", FieldValue::Text("deleted".into()));
    let errors = validate(&form, &sub2);
    assert!(!errors.is_empty());
}

#[test]
fn test_validate_optional_field_absent_ok() {
    let form = sample_form();
    let mut sub = Submission::new(form.id, 1);
    sub.set_value("site_name", FieldValue::Text("Test".into()));
    sub.set_value("location", FieldValue::GeoPoint(GeoPoint::new(0.0, 0.0)));
    // "notes" is optional — not setting it should be fine

    let errors = validate(&form, &sub);
    assert!(errors.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// Field value type tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_field_value_types_serialization() {
    let values = vec![
        FieldValue::Text("hello".into()),
        FieldValue::Integer(42),
        FieldValue::Decimal(3.15),
        FieldValue::Boolean(true),
        FieldValue::Date("2024-01-15".into()),
        FieldValue::Choice("option_a".into()),
        FieldValue::MultiChoice(vec!["a".into(), "b".into()]),
        FieldValue::Barcode("ABC123".into()),
        FieldValue::Null,
    ];
    for v in &values {
        let json = serde_json::to_string(v).unwrap();
        let back: FieldValue = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, v);
    }
}

#[test]
fn test_geo_point_with_altitude() {
    let gp = GeoPoint {
        latitude: 45.0,
        longitude: 90.0,
        altitude: Some(1500.0),
        accuracy: Some(3.5),
    };
    let json = serde_json::to_string(&gp).unwrap();
    let back: GeoPoint = serde_json::from_str(&json).unwrap();
    assert_eq!(back.altitude, Some(1500.0));
    assert_eq!(back.accuracy, Some(3.5));
}

#[test]
fn test_submission_status_transitions() {
    let statuses = vec![
        SubmissionStatus::Draft,
        SubmissionStatus::Complete,
        SubmissionStatus::Synced,
        SubmissionStatus::SyncFailed,
    ];
    for s in &statuses {
        let json = serde_json::to_string(s).unwrap();
        let back: SubmissionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, s);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Integration: form → submission → validate workflow
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_full_workflow_field_survey() {
    // Build form
    let mut form = Form::new("Tree Survey");
    form.add_field(FormField::text("species", "Species").set_required());
    form.add_field(
        FormField::text("diameter_cm", "Diameter (cm)")
            .set_required()
            .with_constraint(Constraint {
                kind: ConstraintKind::Min(1.0),
                message: "diameter must be positive".into(),
            })
            .with_constraint(Constraint {
                kind: ConstraintKind::Max(500.0),
                message: "diameter too large".into(),
            }),
    );
    form.add_field(FormField::geopoint("tree_location", "Tree GPS").set_required());
    form.add_field(FormField::text("health", "Health Notes"));

    // Valid submission
    let mut sub = Submission::new(form.id, form.version);
    sub.set_value("species", FieldValue::Text("Oak".into()));
    sub.set_value("diameter_cm", FieldValue::Decimal(45.5));
    sub.set_value(
        "tree_location",
        FieldValue::GeoPoint(GeoPoint::new(48.8566, 2.3522)),
    );
    sub.set_value("health", FieldValue::Text("Good condition".into()));
    sub.complete();

    let errors = validate(&form, &sub);
    assert!(errors.is_empty(), "valid submission failed: {errors:?}");
    assert_eq!(sub.status, SubmissionStatus::Complete);
}

#[test]
fn test_full_workflow_invalid_submission() {
    let mut form = Form::new("Pipe Inspection");
    form.add_field(FormField::text("pipe_id", "Pipe ID").set_required());
    form.add_field(
        FormField::text("pressure_psi", "Pressure (PSI)")
            .set_required()
            .with_constraint(Constraint {
                kind: ConstraintKind::Min(0.0),
                message: "pressure cannot be negative".into(),
            }),
    );

    let mut sub = Submission::new(form.id, form.version);
    // Missing pipe_id (required)
    sub.set_value("pressure_psi", FieldValue::Decimal(-5.0)); // negative

    let errors = validate(&form, &sub);
    // Should have: missing pipe_id + negative pressure
    assert!(errors.len() >= 2, "expected ≥2 errors, got: {errors:?}");
}
