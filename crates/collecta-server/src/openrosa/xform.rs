//! Rendering a stored [`Form`] as an ODK XForm.
//!
//! Field names become XML element names, so they must be NCNames; rendering
//! fails rather than mangling a name, because the submission parser maps
//! incoming elements back by the same name.
//!
//! The raw `relevant`/`constraint`/`calculation` xpath the xlsform importer
//! preserved in [`FormField::metadata`] is copied into the binds verbatim.
//! Collecta never evaluates it; ODK Collect does, on the device.

use std::io::Cursor;

use collecta_core::form::{FieldType, Form, FormField};
use md5::{Digest, Md5};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

/// Root element of the generated instance. Every nodeset path is rooted here.
pub const INSTANCE_ROOT: &str = "data";

const NS_XFORMS: &str = "http://www.w3.org/2002/xforms";
const NS_XHTML: &str = "http://www.w3.org/1999/xhtml";
const NS_JAVAROSA: &str = "http://openrosa.org/javarosa";
/// The OpenRosa metadata namespace, where `meta/instanceID` lives.
pub const NS_OPENROSA: &str = "http://openrosa.org/xforms";
const NS_XSD: &str = "http://www.w3.org/2001/XMLSchema";

#[derive(Debug)]
pub enum XFormError {
    /// A field name is not usable as an XML element name.
    InvalidName(String),
    /// Unreachable in practice: the writer targets an in-memory buffer.
    Write(std::io::Error),
}

impl std::fmt::Display for XFormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XFormError::InvalidName(name) => {
                write!(f, "field name is not a valid xml name: {name}")
            }
            XFormError::Write(e) => write!(f, "xform serialization failed: {e}"),
        }
    }
}

impl std::error::Error for XFormError {}

impl From<std::io::Error> for XFormError {
    fn from(e: std::io::Error) -> Self {
        XFormError::Write(e)
    }
}

/// The protocol-mandated cache-busting hash for a form list entry.
///
/// MD5 is what the OpenRosa spec fixes for this field. It is a change detector
/// for the client, never an integrity control.
pub fn form_hash(xml: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(xml.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(4 + digest.len() * 2);
    out.push_str("md5:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Render `form` as an ODK XForm document.
pub fn render(form: &Form) -> Result<String, XFormError> {
    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    start(
        &mut writer,
        "h:html",
        &[
            ("xmlns", NS_XFORMS),
            ("xmlns:h", NS_XHTML),
            ("xmlns:jr", NS_JAVAROSA),
            ("xmlns:orx", NS_OPENROSA),
            ("xmlns:xsd", NS_XSD),
        ],
    )?;

    start(&mut writer, "h:head", &[])?;
    text_element(&mut writer, "h:title", &form.title, &[])?;
    start(&mut writer, "model", &[])?;
    write_instance(&mut writer, form)?;
    write_binds(&mut writer, form)?;
    end(&mut writer, "model")?;
    end(&mut writer, "h:head")?;

    start(&mut writer, "h:body", &[])?;
    for field in &form.fields {
        write_control(&mut writer, field, &node_path(&[], field)?)?;
    }
    end(&mut writer, "h:body")?;

    end(&mut writer, "h:html")?;

    let bytes = writer.into_inner().into_inner();
    // every byte written came from utf-8 &str input.
    Ok(String::from_utf8(bytes).expect("xml writer emits utf-8"))
}

// ---- instance ----------------------------------------------------------

fn write_instance<W: std::io::Write>(w: &mut Writer<W>, form: &Form) -> Result<(), XFormError> {
    start(w, "instance", &[])?;
    let version = form.version.to_string();
    let id = form.id.to_string();
    start(
        w,
        INSTANCE_ROOT,
        &[("id", id.as_str()), ("version", version.as_str())],
    )?;
    for field in &form.fields {
        write_instance_node(w, field)?;
    }
    // collect fills instanceID in via the jr:preload="uid" bind below.
    start(w, "orx:meta", &[])?;
    empty(w, "orx:instanceID", &[])?;
    end(w, "orx:meta")?;
    end(w, INSTANCE_ROOT)?;
    end(w, "instance")?;
    Ok(())
}

fn write_instance_node<W: std::io::Write>(
    w: &mut Writer<W>,
    field: &FormField,
) -> Result<(), XFormError> {
    let name = xml_name(&field.name)?;
    if field.field_type == FieldType::Repeat {
        start(w, name, &[])?;
        for child in field.children.iter().flatten() {
            write_instance_node(w, child)?;
        }
        end(w, name)?;
    } else {
        empty(w, name, &[])?;
    }
    Ok(())
}

// ---- binds -------------------------------------------------------------

fn write_binds<W: std::io::Write>(w: &mut Writer<W>, form: &Form) -> Result<(), XFormError> {
    for field in &form.fields {
        write_bind(w, field, &[])?;
    }
    empty(
        w,
        "bind",
        &[
            (
                "nodeset",
                &format!("/{INSTANCE_ROOT}/orx:meta/orx:instanceID"),
            ),
            ("type", "xsd:string"),
            ("jr:preload", "uid"),
        ],
    )?;
    Ok(())
}

fn write_bind<W: std::io::Write>(
    w: &mut Writer<W>,
    field: &FormField,
    ancestors: &[&str],
) -> Result<(), XFormError> {
    let path = node_path(ancestors, field)?;
    let mut attrs: Vec<(&str, String)> = vec![("nodeset", path.clone())];

    if field.field_type != FieldType::Repeat {
        attrs.push(("type", bind_type(&field.field_type).to_string()));
    }
    if field.required {
        attrs.push(("required", "true()".to_string()));
    }
    if field.field_type == FieldType::Note {
        attrs.push(("readonly", "true()".to_string()));
    }
    // raw xpath from the xlsform, evaluated by collect and not by us.
    for (column, attribute) in [
        ("relevant", "relevant"),
        ("constraint", "constraint"),
        ("calculation", "calculate"),
        ("constraint_message", "jr:constraintMsg"),
    ] {
        if let Some(expr) = field.metadata.get(column) {
            attrs.push((attribute, expr.clone()));
        }
    }

    let borrowed: Vec<(&str, &str)> = attrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    empty(w, "bind", &borrowed)?;

    if field.field_type == FieldType::Repeat {
        let name = xml_name(&field.name)?;
        let mut nested: Vec<&str> = ancestors.to_vec();
        nested.push(name);
        for child in field.children.iter().flatten() {
            write_bind(w, child, &nested)?;
        }
    }
    Ok(())
}

/// XForm data type for a field.
///
/// XML Schema types keep the `xsd:` prefix the ODK spec example uses; the
/// ODK-specific types (geo, binary, barcode) have no schema equivalent.
fn bind_type(field_type: &FieldType) -> &'static str {
    match field_type {
        FieldType::Text | FieldType::TextArea | FieldType::Note | FieldType::Select => "xsd:string",
        FieldType::Integer => "xsd:int",
        FieldType::Decimal => "xsd:decimal",
        FieldType::Date => "xsd:date",
        FieldType::DateTime => "xsd:dateTime",
        FieldType::Time => "xsd:time",
        FieldType::Boolean => "xsd:boolean",
        // a multi-select node holds the space-separated value list as a string.
        FieldType::MultiSelect => "xsd:string",
        FieldType::GeoPoint => "geopoint",
        FieldType::GeoTrace => "geotrace",
        FieldType::GeoShape => "geoshape",
        FieldType::Photo
        | FieldType::Audio
        | FieldType::Video
        | FieldType::File
        | FieldType::Signature => "binary",
        FieldType::Barcode => "barcode",
        // repeats carry no value; callers skip the type attribute entirely.
        FieldType::Repeat => "xsd:string",
    }
}

// ---- body --------------------------------------------------------------

fn write_control<W: std::io::Write>(
    w: &mut Writer<W>,
    field: &FormField,
    path: &str,
) -> Result<(), XFormError> {
    let appearance = field.metadata.get("appearance").map(String::as_str);

    match &field.field_type {
        FieldType::Repeat => {
            let mut attrs = vec![("ref", path)];
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, "group", &attrs)?;
            write_label_hint(w, field)?;
            start(w, "repeat", &[("nodeset", path)])?;
            let name = xml_name(&field.name)?;
            for child in field.children.iter().flatten() {
                let child_path = node_path(&[name], child)?;
                write_control(w, child, &child_path)?;
            }
            end(w, "repeat")?;
            end(w, "group")?;
        }
        FieldType::Select | FieldType::MultiSelect => {
            let element = if field.field_type == FieldType::Select {
                "select1"
            } else {
                "select"
            };
            let mut attrs = vec![("ref", path)];
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, element, &attrs)?;
            write_label_hint(w, field)?;
            for choice in field.choices.iter().flatten() {
                start(w, "item", &[])?;
                text_element(w, "label", &choice.label, &[])?;
                text_element(w, "value", &choice.value, &[])?;
                end(w, "item")?;
            }
            end(w, element)?;
        }
        FieldType::Photo
        | FieldType::Audio
        | FieldType::Video
        | FieldType::File
        | FieldType::Signature => {
            let mut attrs = vec![("ref", path), ("mediatype", mediatype(&field.field_type))];
            // signature is an image capture with a drawing appearance.
            let appearance = appearance.or(match field.field_type {
                FieldType::Signature => Some("signature"),
                _ => None,
            });
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, "upload", &attrs)?;
            write_label_hint(w, field)?;
            end(w, "upload")?;
        }
        _ => {
            let mut attrs = vec![("ref", path)];
            let appearance = appearance.or(match field.field_type {
                FieldType::TextArea => Some("multiline"),
                _ => None,
            });
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, "input", &attrs)?;
            write_label_hint(w, field)?;
            end(w, "input")?;
        }
    }
    Ok(())
}

fn mediatype(field_type: &FieldType) -> &'static str {
    match field_type {
        FieldType::Audio => "audio/*",
        FieldType::Video => "video/*",
        FieldType::File => "application/*",
        // photo and signature both capture an image.
        _ => "image/*",
    }
}

fn write_label_hint<W: std::io::Write>(
    w: &mut Writer<W>,
    field: &FormField,
) -> Result<(), XFormError> {
    text_element(w, "label", &field.label, &[])?;
    if let Some(hint) = &field.hint {
        text_element(w, "hint", hint, &[])?;
    }
    Ok(())
}

// ---- names and paths ---------------------------------------------------

/// `/data/<ancestors...>/<field>`, rejecting names XML cannot express.
fn node_path(ancestors: &[&str], field: &FormField) -> Result<String, XFormError> {
    let mut path = format!("/{INSTANCE_ROOT}");
    for ancestor in ancestors {
        path.push('/');
        path.push_str(ancestor);
    }
    path.push('/');
    path.push_str(xml_name(&field.name)?);
    Ok(path)
}

/// Accept a field name only if it is an ASCII XML NCName.
///
/// Deliberately stricter than the XML grammar: names reach us from uploaded
/// spreadsheets, and anything outside this set is far more likely to be a
/// mistake than a legitimate identifier. Rejecting keeps the generated
/// document well-formed without silently rewriting a name the submission
/// parser would then fail to match.
pub fn xml_name(name: &str) -> Result<&str, XFormError> {
    let invalid = || XFormError::InvalidName(name.to_string());
    let mut chars = name.chars();
    let first = chars.next().ok_or_else(invalid)?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(invalid());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        return Err(invalid());
    }
    // "xml" in any casing is reserved by the XML spec.
    if name.len() >= 3 && name[..3].eq_ignore_ascii_case("xml") {
        return Err(invalid());
    }
    Ok(name)
}

// ---- writer helpers ----------------------------------------------------

fn start<W: std::io::Write>(
    w: &mut Writer<W>,
    name: &str,
    attrs: &[(&str, &str)],
) -> Result<(), XFormError> {
    let mut elem = BytesStart::new(name.to_string());
    for (key, value) in attrs {
        elem.push_attribute((*key, *value));
    }
    w.write_event(Event::Start(elem))?;
    Ok(())
}

fn empty<W: std::io::Write>(
    w: &mut Writer<W>,
    name: &str,
    attrs: &[(&str, &str)],
) -> Result<(), XFormError> {
    let mut elem = BytesStart::new(name.to_string());
    for (key, value) in attrs {
        elem.push_attribute((*key, *value));
    }
    w.write_event(Event::Empty(elem))?;
    Ok(())
}

fn end<W: std::io::Write>(w: &mut Writer<W>, name: &str) -> Result<(), XFormError> {
    w.write_event(Event::End(BytesEnd::new(name.to_string())))?;
    Ok(())
}

fn text_element<W: std::io::Write>(
    w: &mut Writer<W>,
    name: &str,
    text: &str,
    attrs: &[(&str, &str)],
) -> Result<(), XFormError> {
    start(w, name, attrs)?;
    w.write_event(Event::Text(BytesText::new(text)))?;
    end(w, name)?;
    Ok(())
}
