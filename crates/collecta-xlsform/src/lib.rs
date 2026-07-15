//! # collecta-xlsform
//!
//! Import [XLSForm](https://xlsform.org) `.xlsx` files into the collecta form
//! model. Reads the `survey`, `choices`, and `settings` sheets.
//!
//! The collecta engine models a subset of XLSForm. Attributes it does not model
//! (raw `constraint`/`relevant` expressions, `choice_filter`, `appearance`,
//! `calculation`, enclosing group) are preserved verbatim on
//! [`FormField::metadata`] rather than dropped. Groups are flattened: their
//! fields move to the top level tagged with a `group` metadata key, since the
//! model has no group container. Repeats map to `FieldType::Repeat` with nested
//! children.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Cursor, Read, Seek};

use calamine::{Data, Range, Reader, Xlsx};
use collecta_core::form::{Choice, Constraint, ConstraintKind, FieldType, Form, FormField};

/// Errors raised while importing an XLSForm.
#[derive(Debug)]
pub enum XlsformError {
    /// Underlying xlsx read failure.
    Workbook(String),
    /// No `survey` sheet was found.
    MissingSurveySheet,
    /// A survey row uses a `type` this importer does not support.
    UnsupportedType { row: usize, type_token: String },
    /// A survey row is missing a `name`.
    MissingName { row: usize },
    /// A select field references a choice list absent from the `choices` sheet.
    MissingChoiceList { row: usize, list_name: String },
    /// A `begin_group`/`begin_repeat` was not closed, or an `end_*` had no opener.
    UnbalancedGroup,
}

impl fmt::Display for XlsformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workbook(msg) => write!(f, "xlsx read error: {msg}"),
            Self::MissingSurveySheet => write!(f, "xlsform has no 'survey' sheet"),
            Self::UnsupportedType { row, type_token } => {
                write!(f, "unsupported type '{type_token}' at survey row {row}")
            }
            Self::MissingName { row } => write!(f, "missing name at survey row {row}"),
            Self::MissingChoiceList { row, list_name } => {
                write!(f, "choice list '{list_name}' at survey row {row} not found")
            }
            Self::UnbalancedGroup => write!(f, "unbalanced begin/end group or repeat"),
        }
    }
}

impl std::error::Error for XlsformError {}

/// Parse an XLSForm from raw `.xlsx` bytes.
pub fn parse_bytes(bytes: &[u8]) -> Result<Form, XlsformError> {
    parse_reader(Cursor::new(bytes.to_vec()))
}

/// Parse an XLSForm from a seekable reader.
pub fn parse_reader<R: Read + Seek>(reader: R) -> Result<Form, XlsformError> {
    let opened: Result<Xlsx<R>, _> = calamine::open_workbook_from_rs(reader);
    let mut workbook = opened.map_err(|e| XlsformError::Workbook(e.to_string()))?;

    let survey_range =
        sheet_range(&mut workbook, "survey").ok_or(XlsformError::MissingSurveySheet)?;
    let choices_range = sheet_range(&mut workbook, "choices");
    let settings_range = sheet_range(&mut workbook, "settings");

    let choices = choices_range
        .map(|r| read_choice_lists(&r))
        .unwrap_or_default();
    let settings = settings_range
        .map(|r| read_records(&r).into_iter().next().unwrap_or_default())
        .unwrap_or_default();

    let title = settings
        .get("form_title")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "Imported Form".to_string());

    let mut form = Form::new(title);
    if let Some(version) = settings.get("version").and_then(|v| v.parse::<u32>().ok()) {
        form.version = version;
    }

    form.fields = parse_survey(&survey_range, &choices)?;
    Ok(form)
}

/// Case-insensitive lookup of a worksheet by name.
fn sheet_range<R: Read + Seek>(workbook: &mut Xlsx<R>, name: &str) -> Option<Range<Data>> {
    let actual = workbook
        .sheet_names()
        .into_iter()
        .find(|s| s.eq_ignore_ascii_case(name))?;
    workbook.worksheet_range(&actual).ok()
}

/// A container being filled while walking survey rows.
struct Frame {
    fields: Vec<FormField>,
    kind: FrameKind,
}

enum FrameKind {
    Root,
    Group { name: String },
    Repeat { field: Box<FormField> },
}

fn parse_survey(
    range: &Range<Data>,
    choices: &ChoiceLists,
) -> Result<Vec<FormField>, XlsformError> {
    let records = read_records(range);
    let mut stack: Vec<Frame> = vec![Frame {
        fields: Vec::new(),
        kind: FrameKind::Root,
    }];

    for (i, rec) in records.iter().enumerate() {
        // +2: skip the header row and switch to 1-based numbering for humans.
        let row_no = i + 2;
        let type_token = rec.get("type").map(String::as_str).unwrap_or("").trim();
        if type_token.is_empty() {
            continue;
        }
        let mut parts = type_token.split_whitespace();
        let base = parts.next().unwrap_or("");
        let list_name = parts.next();

        match base {
            "begin_group" | "begin group" => {
                let name = field_name(rec, row_no)?;
                stack.push(Frame {
                    fields: Vec::new(),
                    kind: FrameKind::Group { name },
                });
            }
            "end_group" | "end group" => {
                let frame = stack.pop().ok_or(XlsformError::UnbalancedGroup)?;
                let FrameKind::Group { name } = frame.kind else {
                    return Err(XlsformError::UnbalancedGroup);
                };
                let parent = stack.last_mut().ok_or(XlsformError::UnbalancedGroup)?;
                for mut field in frame.fields {
                    field
                        .metadata
                        .entry("group".to_string())
                        .or_insert(name.clone());
                    parent.fields.push(field);
                }
            }
            "begin_repeat" | "begin repeat" => {
                let mut field = build_field(rec, row_no, FieldType::Repeat, list_name, choices)?;
                field.children = Some(Vec::new());
                stack.push(Frame {
                    fields: Vec::new(),
                    kind: FrameKind::Repeat {
                        field: Box::new(field),
                    },
                });
            }
            "end_repeat" | "end repeat" => {
                let frame = stack.pop().ok_or(XlsformError::UnbalancedGroup)?;
                let FrameKind::Repeat { mut field } = frame.kind else {
                    return Err(XlsformError::UnbalancedGroup);
                };
                field.children = Some(frame.fields);
                stack
                    .last_mut()
                    .ok_or(XlsformError::UnbalancedGroup)?
                    .fields
                    .push(*field);
            }
            _ => {
                let field_type = map_type(base).ok_or_else(|| XlsformError::UnsupportedType {
                    row: row_no,
                    type_token: base.to_string(),
                })?;
                let field = build_field(rec, row_no, field_type, list_name, choices)?;
                stack
                    .last_mut()
                    .ok_or(XlsformError::UnbalancedGroup)?
                    .fields
                    .push(field);
            }
        }
    }

    if stack.len() != 1 {
        return Err(XlsformError::UnbalancedGroup);
    }
    Ok(stack.pop().unwrap().fields)
}

fn field_name(rec: &Record, row_no: usize) -> Result<String, XlsformError> {
    rec.get("name")
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or(XlsformError::MissingName { row: row_no })
}

fn build_field(
    rec: &Record,
    row_no: usize,
    field_type: FieldType,
    list_name: Option<&str>,
    choices: &ChoiceLists,
) -> Result<FormField, XlsformError> {
    let name = field_name(rec, row_no)?;
    let label = rec
        .get("label")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| name.clone());

    let mut field = FormField {
        name,
        label,
        field_type: field_type.clone(),
        required: parse_bool(rec.get("required")),
        hint: rec.get("hint").filter(|s| !s.is_empty()).cloned(),
        default: rec
            .get("default")
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::Value::String(s.clone())),
        relevant: None,
        choices: None,
        constraints: Vec::new(),
        children: None,
        metadata: BTreeMap::new(),
    };

    // selects: attach the choice list; select_one also enforces membership.
    if matches!(field_type, FieldType::Select | FieldType::MultiSelect) {
        let list = list_name.unwrap_or("");
        let options = choices
            .get(list)
            .ok_or_else(|| XlsformError::MissingChoiceList {
                row: row_no,
                list_name: list.to_string(),
            })?;
        field
            .metadata
            .insert("list_name".to_string(), list.to_string());
        field.choices = Some(options.clone());
        if field_type == FieldType::Select {
            let values: Vec<String> = options.iter().map(|c| c.value.clone()).collect();
            field.constraints.push(Constraint {
                kind: ConstraintKind::OneOf(values),
                message: format!("invalid choice for {}", field.name),
            });
        }
    }

    // preserve xlsform attributes the engine does not model.
    for key in [
        "constraint",
        "constraint_message",
        "relevant",
        "choice_filter",
        "appearance",
        "calculation",
    ] {
        if let Some(val) = rec.get(key).filter(|s| !s.is_empty()) {
            field.metadata.insert(key.to_string(), val.clone());
        }
    }

    Ok(field)
}

fn map_type(base: &str) -> Option<FieldType> {
    let ty = match base {
        "text" | "string" => FieldType::Text,
        "integer" | "int" => FieldType::Integer,
        "decimal" => FieldType::Decimal,
        "date" => FieldType::Date,
        "time" => FieldType::Time,
        "datetime" | "dateTime" => FieldType::DateTime,
        "note" => FieldType::Note,
        "geopoint" => FieldType::GeoPoint,
        "geotrace" => FieldType::GeoTrace,
        "geoshape" => FieldType::GeoShape,
        "image" | "photo" => FieldType::Photo,
        "audio" => FieldType::Audio,
        "video" => FieldType::Video,
        "file" => FieldType::File,
        "barcode" => FieldType::Barcode,
        "signature" => FieldType::Signature,
        "select_one" => FieldType::Select,
        "select_multiple" | "select_multi" => FieldType::MultiSelect,
        _ => return None,
    };
    Some(ty)
}

fn parse_bool(value: Option<&String>) -> bool {
    matches!(
        value.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("yes" | "true" | "1")
    )
}

type Record = BTreeMap<String, String>;
type ChoiceLists = BTreeMap<String, Vec<Choice>>;

/// Read a sheet into records keyed by header cell.
fn read_records(range: &Range<Data>) -> Vec<Record> {
    let mut rows = range.rows();
    let header: Vec<String> = match rows.next() {
        Some(cells) => cells.iter().map(cell_text).collect(),
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for row in rows {
        let mut rec = Record::new();
        let mut any = false;
        for (i, cell) in row.iter().enumerate() {
            let Some(key) = header.get(i).filter(|k| !k.is_empty()) else {
                continue;
            };
            let val = cell_text(cell);
            if !val.is_empty() {
                any = true;
            }
            rec.insert(key.clone(), val);
        }
        if any {
            out.push(rec);
        }
    }
    out
}

/// Group the choices sheet by list name. Accepts `list_name` or `list name`.
fn read_choice_lists(range: &Range<Data>) -> ChoiceLists {
    let mut lists: ChoiceLists = BTreeMap::new();
    for rec in read_records(range) {
        let list = rec
            .get("list_name")
            .or_else(|| rec.get("list name"))
            .filter(|s| !s.is_empty());
        let value = rec.get("name").filter(|s| !s.is_empty());
        let (Some(list), Some(value)) = (list, value) else {
            continue;
        };
        let label = rec
            .get("label")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| value.clone());
        lists.entry(list.clone()).or_default().push(Choice {
            value: value.clone(),
            label,
        });
    }
    lists
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        other => other.to_string().trim().to_string(),
    }
}
