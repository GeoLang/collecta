//! Parsing an ODK Collect submission instance into a typed [`Submission`].
//!
//! # Entity expansion
//!
//! The parser is the only place untrusted XML enters the server, so it is
//! deliberately narrow. quick-xml never processes a DTD internal subset: it
//! reports `<!DOCTYPE ...>` as an opaque event and surfaces every `&name;` as a
//! separate [`Event::GeneralRef`] instead of substituting it. We resolve only
//! the five predefined XML entities and numeric character references, and error
//! on anything else, so a declared entity can never expand, recursively or
//! otherwise. A DOCTYPE is rejected outright rather than ignored.

use std::collections::HashMap;

use collecta_core::form::{FieldType, Form, FormField};
use collecta_core::submission::{FieldValue, GeoPoint, Submission};
use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use uuid::Uuid;

/// Deepest element nesting accepted. Forms nest one repeat level plus the meta
/// block; anything beyond this is malformed or hostile.
const MAX_DEPTH: usize = 32;

/// Cap on elements in one instance, so a small body cannot allocate unboundedly.
const MAX_ELEMENTS: usize = 100_000;

#[derive(Debug, PartialEq, Eq)]
pub enum InstanceError {
    Malformed(String),
    /// A DTD was present. Never accepted, however harmless it looks.
    DocTypeRejected,
    /// An entity reference that is not one of the five predefined ones.
    UnknownEntity(String),
    TooDeep,
    TooLarge,
    /// The root element carries no parseable form `id` attribute.
    MissingFormId,
    /// No `meta/instanceID`; the protocol has no idempotency key without it.
    MissingInstanceId,
}

impl std::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceError::Malformed(e) => write!(f, "submission xml is malformed: {e}"),
            InstanceError::DocTypeRejected => write!(f, "submission xml must not declare a dtd"),
            InstanceError::UnknownEntity(name) => {
                write!(f, "submission xml references undefined entity &{name};")
            }
            InstanceError::TooDeep => write!(f, "submission xml is nested too deeply"),
            InstanceError::TooLarge => write!(f, "submission xml has too many elements"),
            InstanceError::MissingFormId => write!(f, "submission xml has no form id attribute"),
            InstanceError::MissingInstanceId => {
                write!(f, "submission xml has no meta/instanceID")
            }
        }
    }
}

impl std::error::Error for InstanceError {}

/// A generic element tree; the shape is checked against the form afterwards.
#[derive(Debug, Default)]
pub struct Node {
    /// Element name with any namespace prefix stripped.
    pub name: String,
    pub text: String,
    pub children: Vec<Node>,
    attrs: Vec<(String, String)>,
}

impl Node {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
}

/// What the server needs out of one submission instance.
pub struct Instance {
    pub form_id: Uuid,
    pub instance_id: String,
    pub root: Node,
}

/// Parse the `xml_submission_file` part.
pub fn parse(xml: &[u8]) -> Result<Instance, InstanceError> {
    let root = parse_tree(xml)?;

    let form_id = root
        .attr("id")
        .and_then(|id| Uuid::parse_str(id.trim()).ok())
        .ok_or(InstanceError::MissingFormId)?;

    // the meta block is namespaced (orx:), and prefixes are already stripped.
    let instance_id = root
        .child("meta")
        .and_then(|meta| meta.child("instanceID"))
        .map(|node| node.text.trim().to_string())
        .filter(|id| !id.is_empty())
        .ok_or(InstanceError::MissingInstanceId)?;

    Ok(Instance {
        form_id,
        instance_id,
        root,
    })
}

fn parse_tree(xml: &[u8]) -> Result<Node, InstanceError> {
    let mut reader = Reader::from_reader(xml);
    let config = reader.config_mut();
    config.check_end_names = true;
    config.allow_unmatched_ends = false;
    // a lone '&' is a malformed reference, not text to wave through.
    config.allow_dangling_amp = false;

    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut elements = 0usize;
    let mut buf = Vec::new();

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| InstanceError::Malformed(e.to_string()))?;
        match event {
            Event::Eof => break,
            // no dtd, so no place to declare an entity in the first place.
            Event::DocType(_) => return Err(InstanceError::DocTypeRejected),
            Event::Start(e) => {
                elements += 1;
                if elements > MAX_ELEMENTS {
                    return Err(InstanceError::TooLarge);
                }
                stack.push(node_from(&e)?);
                if stack.len() > MAX_DEPTH {
                    return Err(InstanceError::TooDeep);
                }
            }
            // self-closing: complete on sight, it has no End event.
            Event::Empty(e) => {
                elements += 1;
                if elements > MAX_ELEMENTS {
                    return Err(InstanceError::TooLarge);
                }
                let node = node_from(&e)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::End(_) => {
                let node = stack.pop().ok_or_else(|| {
                    InstanceError::Malformed("unbalanced closing tag".to_string())
                })?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::Text(t) => {
                if let Some(node) = stack.last_mut() {
                    let text = t
                        .xml10_content()
                        .map_err(|e| InstanceError::Malformed(e.to_string()))?;
                    node.text.push_str(&text);
                }
            }
            Event::CData(c) => {
                if let Some(node) = stack.last_mut() {
                    let text = c
                        .decode()
                        .map_err(|e| InstanceError::Malformed(e.to_string()))?;
                    node.text.push_str(&text);
                }
            }
            // the entity-expansion gate: predefined names and char refs only.
            Event::GeneralRef(r) => {
                let name = r
                    .decode()
                    .map_err(|e| InstanceError::Malformed(e.to_string()))?
                    .into_owned();
                let resolved = match resolve_xml_entity(&name) {
                    Some(text) => text.to_string(),
                    None => r
                        .resolve_char_ref()
                        .map_err(|e| InstanceError::Malformed(e.to_string()))?
                        .ok_or(InstanceError::UnknownEntity(name))?
                        .to_string(),
                };
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&resolved);
                }
            }
            _ => {}
        }
        buf.clear();
    }

    if !stack.is_empty() {
        return Err(InstanceError::Malformed("unclosed element".to_string()));
    }
    root.ok_or_else(|| InstanceError::Malformed("no root element".to_string()))
}

fn node_from(e: &quick_xml::events::BytesStart) -> Result<Node, InstanceError> {
    let mut node = Node {
        name: local_name(e.name().as_ref()),
        ..Node::default()
    };
    let attributes = e.attributes();
    let decoder = attributes.decoder();
    for attribute in attributes {
        let attribute = attribute.map_err(|e| InstanceError::Malformed(e.to_string()))?;
        let value = attribute
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
            .map_err(|e| InstanceError::Malformed(e.to_string()))?;
        node.attrs
            .push((local_name(attribute.key.as_ref()), value.into_owned()));
    }
    Ok(node)
}

/// Drop any namespace prefix: the instance uses `orx:` for meta, and callers
/// only ever match on local names.
fn local_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    match name.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => name.into_owned(),
    }
}

// ---- mapping onto the form ---------------------------------------------

/// Build a [`Submission`] by matching instance elements to form fields.
///
/// Elements with no matching field are ignored rather than rejected: ODK adds
/// metadata nodes of its own, and the validation engine would flag them as
/// unknown fields. Values that do not fit their declared type are reported.
pub fn to_submission(
    instance: &Instance,
    form: &Form,
    collector_id: &str,
) -> (Submission, Vec<String>) {
    let mut submission = Submission::new(form.id, form.version);
    submission.collector_id = Some(collector_id.to_string());
    let mut errors = Vec::new();

    for field in &form.fields {
        if field.field_type == FieldType::Repeat {
            let rows: Vec<HashMap<String, FieldValue>> = instance
                .root
                .children
                .iter()
                .filter(|node| node.name == field.name)
                .map(|node| repeat_row(node, field, &mut errors))
                .collect();
            if !rows.is_empty() {
                submission.set_value(field.name.clone(), FieldValue::Repeat(rows));
            }
            continue;
        }
        let Some(node) = instance.root.child(&field.name) else {
            continue;
        };
        match coerce(&node.text, field) {
            Ok(value) => submission.set_value(field.name.clone(), value),
            Err(e) => errors.push(e),
        }
    }

    submission.complete();
    (submission, errors)
}

fn repeat_row(
    node: &Node,
    field: &FormField,
    errors: &mut Vec<String>,
) -> HashMap<String, FieldValue> {
    let mut row = HashMap::new();
    for child_field in field.children.iter().flatten() {
        let Some(child) = node.child(&child_field.name) else {
            continue;
        };
        match coerce(&child.text, child_field) {
            Ok(value) => {
                row.insert(child_field.name.clone(), value);
            }
            Err(e) => errors.push(e),
        }
    }
    row
}

/// Turn instance text into a typed value, per the field's declared type.
fn coerce(raw: &str, field: &FormField) -> Result<FieldValue, String> {
    let text = raw.trim();
    if text.is_empty() {
        // an empty node is an unanswered question; the validation engine
        // decides whether that is allowed.
        return Ok(FieldValue::Null);
    }
    let bad = |expected: &str| format!("field '{}' is not a valid {expected}", field.name);

    let value = match field.field_type {
        FieldType::Text | FieldType::TextArea | FieldType::Note => FieldValue::Text(text.into()),
        FieldType::Integer => FieldValue::Integer(text.parse().map_err(|_| bad("integer"))?),
        FieldType::Decimal => FieldValue::Decimal(text.parse().map_err(|_| bad("decimal"))?),
        FieldType::Boolean => match text {
            "true" | "1" => FieldValue::Boolean(true),
            "false" | "0" => FieldValue::Boolean(false),
            _ => return Err(bad("boolean")),
        },
        FieldType::Date => FieldValue::Date(text.into()),
        FieldType::DateTime => FieldValue::DateTime(text.into()),
        FieldType::Time => FieldValue::Time(text.into()),
        FieldType::Select => FieldValue::Choice(text.into()),
        // odk joins selected values with spaces.
        FieldType::MultiSelect => {
            FieldValue::MultiChoice(text.split_whitespace().map(String::from).collect())
        }
        FieldType::GeoPoint => FieldValue::GeoPoint(geopoint(text).ok_or_else(|| bad("geopoint"))?),
        FieldType::GeoTrace => {
            FieldValue::GeoTrace(geopoints(text).ok_or_else(|| bad("geotrace"))?)
        }
        FieldType::GeoShape => {
            FieldValue::GeoShape(geopoints(text).ok_or_else(|| bad("geoshape"))?)
        }
        FieldType::Barcode => FieldValue::Barcode(text.into()),
        // a binary node holds the attachment's file name; the bytes arrive as
        // their own multipart part and are linked through the attachments table.
        FieldType::Photo
        | FieldType::Audio
        | FieldType::Video
        | FieldType::File
        | FieldType::Signature => FieldValue::Text(text.into()),
        FieldType::Repeat => FieldValue::Null,
    };
    Ok(value)
}

/// `"lat lon alt acc"`, per the ODK geopoint type. Altitude and accuracy are
/// optional in practice even though Collect always sends four values.
fn geopoint(text: &str) -> Option<GeoPoint> {
    let mut parts = text.split_whitespace();
    let latitude: f64 = parts.next()?.parse().ok()?;
    let longitude: f64 = parts.next()?.parse().ok()?;
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return None;
    }
    Some(GeoPoint {
        latitude,
        longitude,
        altitude: parts.next().and_then(|v| v.parse().ok()),
        accuracy: parts.next().and_then(|v| v.parse().ok()),
    })
}

/// Semicolon-separated geopoints, as ODK encodes traces and shapes.
fn geopoints(text: &str) -> Option<Vec<GeoPoint>> {
    text.split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(geopoint)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctype_is_rejected_by_name() {
        let xml = br#"<?xml version="1.0"?><!DOCTYPE data [<!ENTITY lol "lol">]><data id="x"><q>&lol;</q></data>"#;
        assert_eq!(parse_tree(xml).unwrap_err(), InstanceError::DocTypeRejected);
    }

    #[test]
    fn undeclared_entity_is_rejected_by_name() {
        let xml = br#"<data id="x"><q>&xxe;</q></data>"#;
        assert_eq!(
            parse_tree(xml).unwrap_err(),
            InstanceError::UnknownEntity("xxe".to_string())
        );
    }

    #[test]
    fn predefined_entities_and_char_refs_resolve() {
        let xml = br#"<data id="x"><q>a &amp; b &lt;c&gt; &#65;</q></data>"#;
        let root = parse_tree(xml).unwrap();
        assert_eq!(root.children[0].text, "a & b <c> A");
    }

    #[test]
    fn nesting_past_the_limit_is_rejected() {
        let mut xml = String::from("<data>");
        xml.push_str(&"<a>".repeat(MAX_DEPTH + 5));
        xml.push_str(&"</a>".repeat(MAX_DEPTH + 5));
        xml.push_str("</data>");
        assert_eq!(
            parse_tree(xml.as_bytes()).unwrap_err(),
            InstanceError::TooDeep
        );
    }
}
