//! Form validation — enforce constraints and required fields.

use crate::error::Error;
use crate::form::{ConstraintKind, Form, FormField};
use crate::submission::{FieldValue, Submission};

/// Validate a submission against its form schema.
///
/// Returns a list of all validation errors (empty = valid).
pub fn validate(form: &Form, submission: &Submission) -> Vec<Error> {
    let mut errors = Vec::new();

    for field in &form.fields {
        validate_field(field, submission, &mut errors);
    }

    // Check for unknown fields
    for key in submission.values.keys() {
        if form.field_by_name(key).is_none() {
            errors.push(Error::UnknownField(key.clone()));
        }
    }

    errors
}

fn validate_field(field: &FormField, submission: &Submission, errors: &mut Vec<Error>) {
    let value = submission.values.get(&field.name);

    // Check required
    if field.required {
        let is_empty = match value {
            None => true,
            Some(FieldValue::Null) => true,
            Some(FieldValue::Text(s)) => s.is_empty(),
            _ => false,
        };
        if is_empty {
            errors.push(Error::RequiredField(field.name.clone()));
            return;
        }
    }

    // Skip constraint checks if value is absent/null
    let Some(value) = value else { return };
    if *value == FieldValue::Null {
        return;
    }

    // Apply constraints
    for constraint in &field.constraints {
        if let Some(err) = check_constraint(field, value, constraint) {
            errors.push(err);
        }
    }
}

fn check_constraint(
    field: &FormField,
    value: &FieldValue,
    constraint: &crate::form::Constraint,
) -> Option<Error> {
    match &constraint.kind {
        ConstraintKind::Min(min) => {
            let num = extract_number(value)?;
            if num < *min {
                return Some(Error::ValidationFailed {
                    field: field.name.clone(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::Max(max) => {
            let num = extract_number(value)?;
            if num > *max {
                return Some(Error::ValidationFailed {
                    field: field.name.clone(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::MinLength(min) => {
            let len = extract_length(value)?;
            if len < *min {
                return Some(Error::ValidationFailed {
                    field: field.name.clone(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::MaxLength(max) => {
            let len = extract_length(value)?;
            if len > *max {
                return Some(Error::ValidationFailed {
                    field: field.name.clone(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::Pattern(pattern) => {
            if let FieldValue::Text(text) = value {
                // Simple glob-style match (not full regex to avoid dependency)
                if !simple_pattern_match(pattern, text) {
                    return Some(Error::ValidationFailed {
                        field: field.name.clone(),
                        reason: constraint.message.clone(),
                    });
                }
            }
        }
        ConstraintKind::OneOf(allowed) => {
            let text = match value {
                FieldValue::Text(s) => s.as_str(),
                FieldValue::Choice(s) => s.as_str(),
                _ => return None,
            };
            if !allowed.iter().any(|a| a == text) {
                return Some(Error::ValidationFailed {
                    field: field.name.clone(),
                    reason: constraint.message.clone(),
                });
            }
        }
    }
    None
}

fn extract_number(value: &FieldValue) -> Option<f64> {
    match value {
        FieldValue::Integer(n) => Some(*n as f64),
        FieldValue::Decimal(n) => Some(*n),
        _ => None,
    }
}

fn extract_length(value: &FieldValue) -> Option<usize> {
    match value {
        FieldValue::Text(s) => Some(s.len()),
        _ => None,
    }
}

/// Simple pattern matching (supports * as wildcard).
fn simple_pattern_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return text.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    pattern == text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::{Constraint, ConstraintKind, FieldType, FormField};

    fn test_form() -> Form {
        let mut form = Form::new("Test");
        form.add_field(FormField::text("name", "Name").set_required());
        form.add_field(FormField {
            name: "age".to_string(),
            label: "Age".to_string(),
            field_type: FieldType::Integer,
            required: false,
            hint: None,
            default: None,
            relevant: None,
            choices: None,
            constraints: vec![
                Constraint {
                    kind: ConstraintKind::Min(0.0),
                    message: "Must be >= 0".to_string(),
                },
                Constraint {
                    kind: ConstraintKind::Max(150.0),
                    message: "Must be <= 150".to_string(),
                },
            ],
            children: None,
        });
        form
    }

    #[test]
    fn test_valid_submission() {
        let form = test_form();
        let mut sub = Submission::new(form.id, 1);
        sub.set_value("name", FieldValue::Text("Alice".to_string()));
        sub.set_value("age", FieldValue::Integer(30));

        let errors = validate(&form, &sub);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_missing_required() {
        let form = test_form();
        let sub = Submission::new(form.id, 1);

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], Error::RequiredField(name) if name == "name"));
    }

    #[test]
    fn test_constraint_violation() {
        let form = test_form();
        let mut sub = Submission::new(form.id, 1);
        sub.set_value("name", FieldValue::Text("Bob".to_string()));
        sub.set_value("age", FieldValue::Integer(-5));

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], Error::ValidationFailed { field, .. } if field == "age"));
    }

    #[test]
    fn test_unknown_field() {
        let form = test_form();
        let mut sub = Submission::new(form.id, 1);
        sub.set_value("name", FieldValue::Text("Test".to_string()));
        sub.set_value("bogus", FieldValue::Text("wat".to_string()));

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], Error::UnknownField(name) if name == "bogus"));
    }

    #[test]
    fn test_pattern_match() {
        assert!(simple_pattern_match("*.pdf", "report.pdf"));
        assert!(!simple_pattern_match("*.pdf", "report.doc"));
        assert!(simple_pattern_match("hello*", "hello world"));
        assert!(simple_pattern_match("*", "anything"));
    }
}
