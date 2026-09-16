// Build a real .xlsx workbook in-memory, parse it, and validate submissions
// through the actual collecta-core validation engine.

use collecta_core::form::{ConditionOp, FieldType, FormField};
use collecta_core::submission::{FieldValue, GeoPoint, Submission};
use collecta_core::validation::validate;
use collecta_xlsform::parse_bytes;
use rust_xlsxwriter::Workbook;
use serde_json::json;

fn write_sheet(workbook: &mut Workbook, name: &str, rows: &[Vec<&str>]) {
    let sheet = workbook.add_worksheet();
    sheet.set_name(name).unwrap();
    for (r, row) in rows.iter().enumerate() {
        for (c, val) in row.iter().enumerate() {
            sheet.write_string(r as u32, c as u16, *val).unwrap();
        }
    }
}

fn sample_xlsx() -> Vec<u8> {
    let mut workbook = Workbook::new();

    let survey = vec![
        vec![
            "type",
            "name",
            "label",
            "required",
            "constraint",
            "relevant",
        ],
        vec!["text", "site_name", "Site Name", "yes", "", ""],
        vec!["integer", "count", "Count", "", "", ""],
        vec!["decimal", "depth", "Depth", "", "", ""],
        vec!["date", "visit_date", "Visit Date", "", "", ""],
        vec!["time", "visit_time", "Visit Time", "", "", ""],
        vec!["dateTime", "stamp", "Stamp", "", "", ""],
        vec!["note", "intro", "Welcome", "", "", ""],
        vec!["geopoint", "location", "Location", "yes", "", ""],
        vec!["geotrace", "path", "Path", "", "", ""],
        vec!["geoshape", "area", "Area", "", "", ""],
        vec![
            "select_one yesno",
            "accessible",
            "Accessible",
            "yes",
            "",
            "${count} > 0",
        ],
        vec!["select_multiple colors", "hazards", "Hazards", "", "", ""],
        vec![
            "text",
            "comment",
            "Comment",
            "",
            "string-length(.) < 500",
            "${accessible} = 'no'",
        ],
        vec!["begin_group", "site_details", "Site Details", "", "", ""],
        vec!["text", "inspector", "Inspector", "yes", "", ""],
        vec!["end_group", "", "", "", "", ""],
        vec!["begin_repeat", "samples", "Samples", "", "", ""],
        vec!["text", "sample_id", "Sample ID", "yes", "", ""],
        vec!["end_repeat", "", "", "", "", ""],
    ];
    write_sheet(&mut workbook, "survey", &survey);

    let choices = vec![
        vec!["list_name", "name", "label"],
        vec!["yesno", "yes", "Yes"],
        vec!["yesno", "no", "No"],
        vec!["colors", "red", "Red"],
        vec!["colors", "green", "Green"],
        vec!["colors", "blue", "Blue"],
    ];
    write_sheet(&mut workbook, "choices", &choices);

    let settings = vec![vec!["form_title", "version"], vec!["Field Survey", "7"]];
    write_sheet(&mut workbook, "settings", &settings);

    workbook.save_to_buffer().unwrap()
}

fn relevant_xlsx() -> Vec<u8> {
    let mut workbook = Workbook::new();

    let survey = vec![
        vec!["type", "name", "label", "relevant"],
        vec!["text", "trigger", "Trigger", ""],
        vec!["integer", "amount", "Amount", ""],
        vec!["select_multiple colors", "picks", "Picks", ""],
        vec!["text", "on_equals", "Equals", "${trigger} = 'yes'"],
        vec!["text", "on_not_equals", "Not equals", "${trigger} != 'yes'"],
        vec!["text", "on_greater", "Greater", "${amount} > 3"],
        vec!["text", "on_less", "Less", "${amount} < 10.5"],
        vec!["text", "on_answered", "Answered", "${trigger} != ''"],
        vec![
            "text",
            "on_selected",
            "Selected",
            "selected(${picks}, 'red')",
        ],
        vec![
            "text",
            "on_two_clauses",
            "Two clauses",
            "${trigger} = 'yes' and ${amount} > 3",
        ],
        vec!["text", "on_at_least", "At least", "${amount} >= 3"],
        vec![
            "text",
            "on_double_quotes",
            "Double quotes",
            "${trigger} = \"yes\"",
        ],
        vec!["text", "on_function", "Function", "count(${picks}) > 1"],
    ];
    write_sheet(&mut workbook, "survey", &survey);

    let choices = vec![
        vec!["list_name", "name", "label"],
        vec!["colors", "red", "Red"],
        vec!["colors", "green", "Green"],
    ];
    write_sheet(&mut workbook, "choices", &choices);

    workbook.save_to_buffer().unwrap()
}

#[test]
fn parses_simple_relevant_expressions_into_conditions() {
    let form = parse_bytes(&relevant_xlsx()).unwrap();

    for (name, field, op, value) in [
        ("on_equals", "trigger", ConditionOp::Equals, json!("yes")),
        (
            "on_not_equals",
            "trigger",
            ConditionOp::NotEquals,
            json!("yes"),
        ),
        ("on_greater", "amount", ConditionOp::GreaterThan, json!(3)),
        ("on_less", "amount", ConditionOp::LessThan, json!(10.5)),
        (
            "on_answered",
            "trigger",
            ConditionOp::IsNotEmpty,
            serde_json::Value::Null,
        ),
        ("on_selected", "picks", ConditionOp::Contains, json!("red")),
    ] {
        let condition = form
            .field_by_name(name)
            .unwrap()
            .relevant
            .as_ref()
            .unwrap_or_else(|| panic!("{name} should carry a condition"));
        assert_eq!(condition.field, field, "{name}");
        assert_eq!(condition.op, op, "{name}");
        assert_eq!(condition.value, value, "{name}");
    }
}

#[test]
fn keeps_richer_relevant_expressions_as_raw_metadata_only() {
    let form = parse_bytes(&relevant_xlsx()).unwrap();

    for (name, raw) in [
        ("on_two_clauses", "${trigger} = 'yes' and ${amount} > 3"),
        ("on_at_least", "${amount} >= 3"),
        ("on_double_quotes", "${trigger} = \"yes\""),
        ("on_function", "count(${picks}) > 1"),
    ] {
        let field = form.field_by_name(name).unwrap();
        assert!(field.relevant.is_none(), "{name} should not be modelled");
        assert_eq!(field.metadata.get("relevant").unwrap(), raw, "{name}");
    }

    // a modelled condition keeps the raw expression too: the xform still carries it.
    let modelled = form.field_by_name("on_equals").unwrap();
    assert_eq!(
        modelled.metadata.get("relevant").unwrap(),
        "${trigger} = 'yes'"
    );
}

#[test]
fn conditions_hide_fields_from_validation() {
    let form = parse_bytes(&relevant_xlsx()).unwrap();
    let mut answered = Submission::new(form.id, form.version);
    answered.set_value("trigger", FieldValue::Text("yes".into()));
    answered.set_value("amount", FieldValue::Integer(4));
    answered.set_value("picks", FieldValue::MultiChoice(vec!["red".into()]));

    // shown fields left blank are still not required, so this must be clean.
    assert!(validate(&form, &answered).is_empty());

    let shown = |name: &str, submission: &Submission| {
        form.field_by_name(name)
            .unwrap()
            .relevant
            .as_ref()
            .unwrap()
            .evaluate(&submission.values)
    };
    assert!(shown("on_equals", &answered));
    assert!(!shown("on_not_equals", &answered));
    assert!(shown("on_greater", &answered));
    assert!(shown("on_less", &answered));
    assert!(shown("on_answered", &answered));
    assert!(shown("on_selected", &answered));

    let blank = Submission::new(form.id, form.version);
    assert!(!shown("on_equals", &blank));
    assert!(shown("on_not_equals", &blank));
    assert!(!shown("on_greater", &blank));
    assert!(!shown("on_less", &blank));
    assert!(!shown("on_answered", &blank));
    assert!(!shown("on_selected", &blank));
}

#[test]
fn parses_settings_and_field_types() {
    let form = parse_bytes(&sample_xlsx()).unwrap();

    assert_eq!(form.title, "Field Survey");
    assert_eq!(form.version, 7);

    // grouped "inspector" is flattened to the top level; repeat stays nested.
    let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "site_name",
            "count",
            "depth",
            "visit_date",
            "visit_time",
            "stamp",
            "intro",
            "location",
            "path",
            "area",
            "accessible",
            "hazards",
            "comment",
            "inspector",
            "samples",
        ]
    );

    let ty = |name: &str| form.field_by_name(name).unwrap().field_type.clone();
    assert_eq!(ty("count"), FieldType::Integer);
    assert_eq!(ty("depth"), FieldType::Decimal);
    assert_eq!(ty("visit_date"), FieldType::Date);
    assert_eq!(ty("visit_time"), FieldType::Time);
    assert_eq!(ty("stamp"), FieldType::DateTime);
    assert_eq!(ty("intro"), FieldType::Note);
    assert_eq!(ty("location"), FieldType::GeoPoint);
    assert_eq!(ty("path"), FieldType::GeoTrace);
    assert_eq!(ty("area"), FieldType::GeoShape);
    assert_eq!(ty("accessible"), FieldType::Select);
    assert_eq!(ty("hazards"), FieldType::MultiSelect);
}

#[test]
fn maps_selects_and_preserves_unmodeled_attributes() {
    let form = parse_bytes(&sample_xlsx()).unwrap();

    // select_one gets choices and a OneOf constraint the engine can enforce.
    let accessible = form.field_by_name("accessible").unwrap();
    assert_eq!(accessible.choices.as_ref().unwrap().len(), 2);
    assert_eq!(accessible.constraints.len(), 1);
    assert_eq!(accessible.metadata.get("relevant").unwrap(), "${count} > 0");

    // select_multiple gets choices but no OneOf (engine does not check multichoice).
    let hazards = form.field_by_name("hazards").unwrap();
    assert_eq!(hazards.choices.as_ref().unwrap().len(), 3);
    assert!(hazards.constraints.is_empty());

    // raw constraint/relevant expressions preserved verbatim, not evaluated.
    let comment = form.field_by_name("comment").unwrap();
    assert_eq!(
        comment.metadata.get("constraint").unwrap(),
        "string-length(.) < 500"
    );
    assert_eq!(
        comment.metadata.get("relevant").unwrap(),
        "${accessible} = 'no'"
    );

    // group membership preserved after flattening.
    let inspector = form.field_by_name("inspector").unwrap();
    assert_eq!(inspector.metadata.get("group").unwrap(), "site_details");
    assert!(inspector.required);

    // repeat becomes a nested container.
    let samples = form.field_by_name("samples").unwrap();
    assert_eq!(samples.field_type, FieldType::Repeat);
    let children: &Vec<FormField> = samples.children.as_ref().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name, "sample_id");
}

#[test]
fn parsed_form_drives_the_validation_engine() {
    let form = parse_bytes(&sample_xlsx()).unwrap();

    // valid: all required top-level fields present, select value in the list.
    // count > 0 is what makes "accessible" relevant, so it is required here.
    let mut ok = Submission::new(form.id, form.version);
    ok.set_value("count", FieldValue::Integer(2));
    ok.set_value("site_name", FieldValue::Text("Alpha".into()));
    ok.set_value("location", FieldValue::GeoPoint(GeoPoint::new(51.5, -0.1)));
    ok.set_value("accessible", FieldValue::Choice("yes".into()));
    ok.set_value("inspector", FieldValue::Text("Dana".into()));
    let errors = validate(&form, &ok);
    assert!(errors.is_empty(), "expected valid, got: {errors:?}");

    // missing a required field flags it.
    let mut missing = Submission::new(form.id, form.version);
    missing.set_value("location", FieldValue::GeoPoint(GeoPoint::new(0.0, 0.0)));
    missing.set_value("accessible", FieldValue::Choice("yes".into()));
    missing.set_value("inspector", FieldValue::Text("Dana".into()));
    let errors = validate(&form, &missing);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, collecta_core::Error::RequiredField(f) if f == "site_name")),
        "expected site_name required error, got: {errors:?}"
    );

    // a select value outside the choice list fails the OneOf constraint.
    let mut bad_choice = Submission::new(form.id, form.version);
    bad_choice.set_value("count", FieldValue::Integer(2));
    bad_choice.set_value("site_name", FieldValue::Text("Alpha".into()));
    bad_choice.set_value("location", FieldValue::GeoPoint(GeoPoint::new(0.0, 0.0)));
    bad_choice.set_value("accessible", FieldValue::Choice("maybe".into()));
    bad_choice.set_value("inspector", FieldValue::Text("Dana".into()));
    let errors = validate(&form, &bad_choice);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, collecta_core::Error::ValidationFailed { field, .. } if field == "accessible")),
        "expected accessible OneOf error, got: {errors:?}"
    );
}
