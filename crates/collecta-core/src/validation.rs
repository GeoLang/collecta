//! Form validation — enforce constraints and required fields.

use std::collections::HashMap;

use crate::error::Error;
use crate::form::{ConstraintKind, FieldType, Form, FormField};
use crate::submission::{FieldValue, Submission};

/// Validate a submission against its form schema.
///
/// Returns a list of all validation errors (empty = valid).
pub fn validate(form: &Form, submission: &Submission) -> Vec<Error> {
    let mut errors = Vec::new();

    for field in &form.fields {
        validate_field(field, &submission.values, &field.name, &mut errors);
    }

    // Check for unknown fields
    for key in submission.values.keys() {
        if form.field_by_name(key).is_none() {
            errors.push(Error::UnknownField(key.clone()));
        }
    }

    errors
}

fn validate_field(
    field: &FormField,
    values: &HashMap<String, FieldValue>,
    path: &str,
    errors: &mut Vec<Error>,
) {
    // a field the condition hides was never asked, so nothing about it is checked.
    if let Some(condition) = &field.relevant
        && !condition.evaluate(values)
    {
        return;
    }

    let value = values.get(&field.name);

    // Check required
    if field.required {
        let is_empty = match value {
            None => true,
            Some(FieldValue::Null) => true,
            Some(FieldValue::Text(s)) => s.is_empty(),
            Some(FieldValue::Repeat(instances)) => instances.is_empty(),
            _ => false,
        };
        if is_empty {
            errors.push(Error::RequiredField(path.to_string()));
            return;
        }
    }

    // a repeat holds instances rather than a value of its own.
    if field.field_type == FieldType::Repeat {
        if let Some(FieldValue::Repeat(instances)) = value {
            validate_instances(field, instances, values, path, errors);
        }
        return;
    }

    // Skip constraint checks if value is absent/null
    let Some(value) = value else { return };
    if *value == FieldValue::Null {
        return;
    }

    // Apply constraints
    for constraint in &field.constraints {
        if let Some(err) = check_constraint(path, value, constraint) {
            errors.push(err);
        }
    }
}

fn validate_instances(
    repeat: &FormField,
    instances: &[HashMap<String, FieldValue>],
    outer: &HashMap<String, FieldValue>,
    path: &str,
    errors: &mut Vec<Error>,
) {
    for (index, instance) in instances.iter().enumerate() {
        // a child reads its own instance first, so a name it does not answer
        // resolves to the field outside the repeat.
        let mut scope = outer.clone();
        scope.extend(
            instance
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        for child in repeat.children.iter().flatten() {
            let child_path = format!("{path}[{index}].{}", child.name);
            validate_field(child, &scope, &child_path, errors);
        }
        for key in instance.keys() {
            let known = repeat
                .children
                .iter()
                .flatten()
                .any(|child| &child.name == key);
            if !known {
                errors.push(Error::UnknownField(format!("{path}[{index}].{key}")));
            }
        }
    }
}

fn check_constraint(
    path: &str,
    value: &FieldValue,
    constraint: &crate::form::Constraint,
) -> Option<Error> {
    match &constraint.kind {
        ConstraintKind::Min(min) => {
            let num = extract_number(value)?;
            if num < *min {
                return Some(Error::ValidationFailed {
                    field: path.to_string(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::Max(max) => {
            let num = extract_number(value)?;
            if num > *max {
                return Some(Error::ValidationFailed {
                    field: path.to_string(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::MinLength(min) => {
            let len = extract_length(value)?;
            if len < *min {
                return Some(Error::ValidationFailed {
                    field: path.to_string(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::MaxLength(max) => {
            let len = extract_length(value)?;
            if len > *max {
                return Some(Error::ValidationFailed {
                    field: path.to_string(),
                    reason: constraint.message.clone(),
                });
            }
        }
        ConstraintKind::Pattern(pattern) => {
            if let FieldValue::Text(text) = value {
                // Simple glob-style match (not full regex to avoid dependency)
                if !simple_pattern_match(pattern, text) {
                    return Some(Error::ValidationFailed {
                        field: path.to_string(),
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
                    field: path.to_string(),
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
    use crate::form::{Condition, ConditionOp, Constraint, ConstraintKind, FieldType, FormField};

    fn gated_form(op: ConditionOp, value: serde_json::Value) -> Form {
        let mut form = Form::new("Gated");
        form.add_field(FormField::text("trigger", "Trigger"));
        let mut detail = FormField::text("detail", "Detail")
            .set_required()
            .with_constraint(Constraint {
                kind: ConstraintKind::MaxLength(3),
                message: "Too long".to_string(),
            });
        detail.relevant = Some(Condition {
            field: "trigger".to_string(),
            op,
            value,
        });
        form.add_field(detail);
        form
    }

    fn errors_with_trigger(form: &Form, trigger: Option<FieldValue>) -> Vec<Error> {
        let mut sub = Submission::new(form.id, 1);
        if let Some(value) = trigger {
            sub.set_value("trigger", value);
        }
        validate(form, &sub)
    }

    fn repeat_form() -> Form {
        let mut form = Form::new("Samples");
        form.add_field(FormField::text("audit", "Audit"));

        let mut lab_ref = FormField::text("lab_ref", "Lab reference").set_required();
        lab_ref.relevant = Some(Condition {
            field: "needs_lab".to_string(),
            op: ConditionOp::Equals,
            value: serde_json::json!("yes"),
        });

        let mut audit_note = FormField::text("audit_note", "Audit note").set_required();
        audit_note.relevant = Some(Condition {
            field: "audit".to_string(),
            op: ConditionOp::Equals,
            value: serde_json::json!("yes"),
        });

        let mut depth = FormField::text("depth", "Depth").with_constraint(Constraint {
            kind: ConstraintKind::Max(10.0),
            message: "Deeper than the corer".to_string(),
        });
        depth.field_type = FieldType::Integer;

        let mut samples = FormField::text("samples", "Samples");
        samples.field_type = FieldType::Repeat;
        samples.children = Some(vec![
            FormField::text("sample_id", "Sample id").set_required(),
            FormField::text("needs_lab", "Needs lab"),
            lab_ref,
            audit_note,
            depth,
        ]);
        form.add_field(samples);
        form
    }

    fn instance(answers: &[(&str, FieldValue)]) -> std::collections::HashMap<String, FieldValue> {
        answers
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    }

    fn text(value: &str) -> FieldValue {
        FieldValue::Text(value.to_string())
    }

    fn submission_with(
        form: &Form,
        instances: Vec<std::collections::HashMap<String, FieldValue>>,
    ) -> Submission {
        let mut sub = Submission::new(form.id, 1);
        sub.set_value("samples", FieldValue::Repeat(instances));
        sub
    }

    #[test]
    fn test_required_child_fails_only_the_instance_that_omits_it() {
        let form = repeat_form();
        let sub = submission_with(
            &form,
            vec![
                instance(&[("sample_id", text("A1"))]),
                instance(&[("needs_lab", text("no"))]),
            ],
        );

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(
            matches!(&errors[0], Error::RequiredField(path) if path == "samples[1].sample_id"),
            "got: {errors:?}"
        );
    }

    #[test]
    fn test_child_condition_reads_its_own_instance() {
        let form = repeat_form();
        let sub = submission_with(
            &form,
            vec![
                instance(&[("sample_id", text("A1")), ("needs_lab", text("yes"))]),
                instance(&[("sample_id", text("A2")), ("needs_lab", text("no"))]),
            ],
        );

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(
            matches!(&errors[0], Error::RequiredField(path) if path == "samples[0].lab_ref"),
            "got: {errors:?}"
        );
    }

    #[test]
    fn test_child_condition_reads_a_top_level_field() {
        let form = repeat_form();
        let instances = vec![
            instance(&[("sample_id", text("A1"))]),
            instance(&[("sample_id", text("A2"))]),
        ];

        let mut audited = submission_with(&form, instances.clone());
        audited.set_value("audit", text("yes"));
        let errors = validate(&form, &audited);
        let paths: Vec<String> = errors
            .iter()
            .map(|error| match error {
                Error::RequiredField(path) => path.clone(),
                other => panic!("unexpected error: {other:?}"),
            })
            .collect();
        assert_eq!(paths, ["samples[0].audit_note", "samples[1].audit_note"]);

        let mut unaudited = submission_with(&form, instances);
        unaudited.set_value("audit", text("no"));
        assert!(validate(&form, &unaudited).is_empty());
    }

    #[test]
    fn test_child_constraint_names_the_instance() {
        let form = repeat_form();
        let sub = submission_with(
            &form,
            vec![
                instance(&[("sample_id", text("A1")), ("depth", FieldValue::Integer(4))]),
                instance(&[
                    ("sample_id", text("A2")),
                    ("depth", FieldValue::Integer(40)),
                ]),
            ],
        );

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(
            matches!(&errors[0], Error::ValidationFailed { field, .. } if field == "samples[1].depth"),
            "got: {errors:?}"
        );
    }

    #[test]
    fn test_unknown_key_inside_an_instance_names_the_row() {
        let form = repeat_form();
        let sub = submission_with(
            &form,
            vec![
                instance(&[("sample_id", text("A1"))]),
                instance(&[("sample_id", text("A2")), ("bogus", text("x"))]),
            ],
        );

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(
            matches!(&errors[0], Error::UnknownField(path) if path == "samples[1].bogus"),
            "got: {errors:?}"
        );
    }

    #[test]
    fn test_required_repeat_with_no_instances_is_missing() {
        let mut form = repeat_form();
        let samples = form
            .fields
            .iter_mut()
            .find(|field| field.name == "samples")
            .unwrap();
        samples.required = true;

        let empty = submission_with(&form, Vec::new());
        let errors = validate(&form, &empty);
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(
            matches!(&errors[0], Error::RequiredField(path) if path == "samples"),
            "got: {errors:?}"
        );

        let filled = submission_with(&form, vec![instance(&[("sample_id", text("A1"))])]);
        assert!(validate(&form, &filled).is_empty());
    }

    #[test]
    fn test_sibling_answer_beats_a_top_level_field_of_the_same_name() {
        let mut form = repeat_form();
        form.add_field(FormField::text("needs_lab", "Needs lab"));
        let mut sub = submission_with(
            &form,
            vec![instance(&[
                ("sample_id", text("A1")),
                ("needs_lab", text("no")),
            ])],
        );
        sub.set_value("needs_lab", text("yes"));

        assert!(validate(&form, &sub).is_empty());
    }

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
            metadata: std::collections::BTreeMap::new(),
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
    fn test_relevant_field_is_checked_when_the_condition_holds() {
        let form = gated_form(ConditionOp::Equals, serde_json::json!("yes"));
        let errors = errors_with_trigger(&form, Some(FieldValue::Text("yes".to_string())));
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(matches!(&errors[0], Error::RequiredField(name) if name == "detail"));
    }

    #[test]
    fn test_irrelevant_field_skips_required_and_constraints() {
        let form = gated_form(ConditionOp::Equals, serde_json::json!("yes"));
        let mut sub = Submission::new(form.id, 1);
        sub.set_value("trigger", FieldValue::Text("no".to_string()));
        sub.set_value("detail", FieldValue::Text("far too long".to_string()));

        let errors = validate(&form, &sub);
        assert!(errors.is_empty(), "got: {errors:?}");
    }

    #[test]
    fn test_relevant_field_still_fails_its_constraint() {
        let form = gated_form(ConditionOp::Equals, serde_json::json!("yes"));
        let mut sub = Submission::new(form.id, 1);
        sub.set_value("trigger", FieldValue::Text("yes".to_string()));
        sub.set_value("detail", FieldValue::Text("far too long".to_string()));

        let errors = validate(&form, &sub);
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(matches!(&errors[0], Error::ValidationFailed { field, .. } if field == "detail"));
    }

    // an unanswered question is an empty string in odk, so only != is true of it.
    #[test]
    fn test_absent_controlling_field_hides_every_condition_but_not_equals() {
        for (op, value, shown) in [
            (ConditionOp::Equals, serde_json::json!("yes"), false),
            (ConditionOp::NotEquals, serde_json::json!("yes"), true),
            (ConditionOp::GreaterThan, serde_json::json!(0), false),
            (ConditionOp::LessThan, serde_json::json!(10), false),
            (ConditionOp::Contains, serde_json::json!("red"), false),
            (ConditionOp::IsNotEmpty, serde_json::Value::Null, false),
        ] {
            let form = gated_form(op.clone(), value);
            let errors = errors_with_trigger(&form, None);
            assert_eq!(
                !errors.is_empty(),
                shown,
                "{op:?} with an absent trigger should be shown: {shown}"
            );
        }
    }

    #[test]
    fn test_empty_answer_matches_an_absent_one() {
        let form = gated_form(ConditionOp::IsNotEmpty, serde_json::Value::Null);
        let errors = errors_with_trigger(&form, Some(FieldValue::Text(String::new())));
        assert!(errors.is_empty(), "got: {errors:?}");

        let answered = errors_with_trigger(&form, Some(FieldValue::Text("x".to_string())));
        assert_eq!(answered.len(), 1, "got: {answered:?}");
    }

    #[test]
    fn test_numeric_and_multichoice_conditions() {
        let over_ten = gated_form(ConditionOp::GreaterThan, serde_json::json!(10));
        assert!(errors_with_trigger(&over_ten, Some(FieldValue::Integer(4))).is_empty());
        assert_eq!(
            errors_with_trigger(&over_ten, Some(FieldValue::Integer(11))).len(),
            1
        );

        let picked_red = gated_form(ConditionOp::Contains, serde_json::json!("red"));
        let others = FieldValue::MultiChoice(vec!["green".to_string(), "blue".to_string()]);
        assert!(errors_with_trigger(&picked_red, Some(others)).is_empty());
        let with_red = FieldValue::MultiChoice(vec!["green".to_string(), "red".to_string()]);
        assert_eq!(errors_with_trigger(&picked_red, Some(with_red)).len(), 1);
    }

    #[test]
    fn test_pattern_match() {
        assert!(simple_pattern_match("*.pdf", "report.pdf"));
        assert!(!simple_pattern_match("*.pdf", "report.doc"));
        assert!(simple_pattern_match("hello*", "hello world"));
        assert!(simple_pattern_match("*", "anything"));
    }
}
