//! Rendering a stored [`Form`] as an ODK XForm.
//!
//! Field names become XML element names, so they must be NCNames; rendering
//! fails rather than mangling a name, because the submission parser maps
//! incoming elements back by the same name.
//!
//! The `relevant`/`constraint`/`calculation` expressions the xlsform importer
//! preserved in [`FormField::metadata`] go into the binds with one change: the
//! XLSForm `${name}` shorthand is rewritten to the referenced field's path, the
//! way pyxform does it. `${name}` is not XPath, and JavaRosa rejects a form that
//! contains it. Everything around the references is passed through untouched.
//! Collecta never evaluates any of it; ODK Collect does, on the device.

use std::collections::BTreeMap;
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
    /// `${...}` naming a field the form does not define.
    UnknownReference { field: String, reference: String },
    /// `${...}` that is not a plain field name, `${last-saved#x}` included.
    MalformedReference { field: String, reference: String },
    /// `${...}` naming a field defined more than once, so the target is unclear.
    AmbiguousReference { field: String, reference: String },
    /// `${...}` in an attribute that is display text rather than an expression.
    /// pyxform moves these into an itext translation with an `<output>`, which
    /// this renderer does not emit.
    ReferenceInText { field: String, attribute: String },
    /// Unreachable in practice: the writer targets an in-memory buffer.
    Write(std::io::Error),
}

impl std::fmt::Display for XFormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XFormError::InvalidName(name) => {
                write!(f, "field name is not a valid xml name: {name}")
            }
            XFormError::UnknownReference { field, reference } => write!(
                f,
                "field '{field}' references ${{{reference}}}, which no field defines"
            ),
            XFormError::MalformedReference { field, reference } => write!(
                f,
                "field '{field}' has a malformed reference ${{{reference}}}: \
                 only ${{field_name}} is supported"
            ),
            XFormError::AmbiguousReference { field, reference } => write!(
                f,
                "field '{field}' references ${{{reference}}}, but more than one field \
                 has that name"
            ),
            XFormError::ReferenceInText { field, attribute } => write!(
                f,
                "field '{field}' uses ${{...}} in '{attribute}', which needs an itext \
                 output this renderer does not emit"
            ),
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

// ---- ${name} references ------------------------------------------------

/// Where every field sits in the instance, so `${name}` can be resolved.
///
/// XLSForm expressions use the `${name}` shorthand, which is not XPath. pyxform
/// rewrites each token to a path before emitting the bind, and JavaRosa rejects
/// the form outright if it does not (that is the "display condition with node"
/// error ODK Collect reports). This index is what makes that rewrite possible.
struct FieldIndex {
    /// Field name to its enclosing repeat, `None` at the top level. More than
    /// one entry means the name is defined twice and cannot be referenced.
    locations: BTreeMap<String, Vec<Option<String>>>,
}

impl FieldIndex {
    fn build(form: &Form) -> Self {
        let mut locations: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
        for field in &form.fields {
            locations.entry(field.name.clone()).or_default().push(None);
            if field.field_type == FieldType::Repeat {
                for child in field.children.iter().flatten() {
                    locations
                        .entry(child.name.clone())
                        .or_default()
                        .push(Some(field.name.clone()));
                }
            }
        }
        Self { locations }
    }

    /// The path `${reference}` becomes, as seen from a field in `scope`.
    ///
    /// pyxform emits a path relative to the lowest common ancestor repeat when
    /// the referring field and its target share one, and an absolute path
    /// otherwise. The xlsform importer flattens display groups, so the only
    /// shared-repeat case here is two children of the same repeat, one step up.
    /// Absolute would be wrong there: it resolves to the first repeat instance
    /// rather than the current one.
    fn resolve(
        &self,
        reference: &str,
        scope: Option<&str>,
        field: &str,
    ) -> Result<String, XFormError> {
        if xml_name(reference).is_err() {
            return Err(XFormError::MalformedReference {
                field: field.to_string(),
                reference: reference.to_string(),
            });
        }
        let entries =
            self.locations
                .get(reference)
                .ok_or_else(|| XFormError::UnknownReference {
                    field: field.to_string(),
                    reference: reference.to_string(),
                })?;
        if entries.len() > 1 {
            return Err(XFormError::AmbiguousReference {
                field: field.to_string(),
                reference: reference.to_string(),
            });
        }
        Ok(match (scope, entries[0].as_deref()) {
            (Some(source), Some(target)) if source == target => format!("../{reference}"),
            (_, Some(target)) => format!("/{INSTANCE_ROOT}/{target}/{reference}"),
            (_, None) => format!("/{INSTANCE_ROOT}/{reference}"),
        })
    }
}

/// Rewrite every `${name}` in an expression to its path.
///
/// Substitutions are padded with spaces, as pyxform pads them, so two adjacent
/// references can never run together into one bogus path.
fn rewrite_expression(
    expression: &str,
    scope: Option<&str>,
    index: &FieldIndex,
    field: &str,
) -> Result<String, XFormError> {
    let mut out = String::with_capacity(expression.len());
    let mut rest = expression;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| XFormError::MalformedReference {
                field: field.to_string(),
                reference: after.to_string(),
            })?;
        out.push(' ');
        out.push_str(&index.resolve(&after[..end], scope, field)?);
        out.push(' ');
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out.trim().to_string())
}

/// Attribute values that are shown to the user rather than evaluated.
///
/// A reference here would need an itext translation carrying an `<output>`, so
/// it is refused rather than emitted as a path the enumerator would have to read.
fn checked_text(value: &str, attribute: &str, field: &str) -> Result<String, XFormError> {
    if value.contains("${") {
        return Err(XFormError::ReferenceInText {
            field: field.to_string(),
            attribute: attribute.to_string(),
        });
    }
    Ok(value.to_string())
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
    let index = FieldIndex::build(form);
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
    write_binds(&mut writer, form, &index)?;
    end(&mut writer, "model")?;
    end(&mut writer, "h:head")?;

    start(&mut writer, "h:body", &[])?;
    for field in &form.fields {
        write_control(&mut writer, field, &[], &index)?;
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

fn write_binds<W: std::io::Write>(
    w: &mut Writer<W>,
    form: &Form,
    index: &FieldIndex,
) -> Result<(), XFormError> {
    for field in &form.fields {
        write_bind(w, field, &[], index)?;
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
    index: &FieldIndex,
) -> Result<(), XFormError> {
    let path = node_path(ancestors, field)?;
    // groups are flattened, so the only ancestor a field can have is its repeat.
    let scope = ancestors.first().copied();
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
    // xlsform expressions, with their ${name} shorthand resolved to paths.
    // The surrounding xpath is otherwise untouched: collect evaluates it, we
    // do not.
    for (column, attribute) in [
        ("relevant", "relevant"),
        ("constraint", "constraint"),
        ("calculation", "calculate"),
    ] {
        if let Some(expr) = field.metadata.get(column) {
            attrs.push((
                attribute,
                rewrite_expression(expr, scope, index, &field.name)?,
            ));
        }
    }
    // the constraint message is shown to the enumerator, not evaluated.
    if let Some(message) = field.metadata.get("constraint_message") {
        attrs.push((
            "jr:constraintMsg",
            checked_text(message, "constraint_message", &field.name)?,
        ));
    }

    let borrowed: Vec<(&str, &str)> = attrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    empty(w, "bind", &borrowed)?;

    if field.field_type == FieldType::Repeat {
        let name = xml_name(&field.name)?;
        let mut nested: Vec<&str> = ancestors.to_vec();
        nested.push(name);
        for child in field.children.iter().flatten() {
            write_bind(w, child, &nested, index)?;
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
    ancestors: &[&str],
    index: &FieldIndex,
) -> Result<(), XFormError> {
    let path = &node_path(ancestors, field)?;
    let scope = ancestors.first().copied();
    let appearance = match field.metadata.get("appearance") {
        Some(value) => Some(checked_text(value, "appearance", &field.name)?),
        None => None,
    };
    let appearance = appearance.as_deref();

    match &field.field_type {
        FieldType::Repeat => {
            let mut attrs = vec![("ref", path.as_str())];
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, "group", &attrs)?;
            write_label_hint(w, field, scope, index)?;
            start(w, "repeat", &[("nodeset", path)])?;
            let name = xml_name(&field.name)?;
            let mut nested: Vec<&str> = ancestors.to_vec();
            nested.push(name);
            for child in field.children.iter().flatten() {
                write_control(w, child, &nested, index)?;
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
            let mut attrs = vec![("ref", path.as_str())];
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, element, &attrs)?;
            write_label_hint(w, field, scope, index)?;
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
            let mut attrs = vec![
                ("ref", path.as_str()),
                ("mediatype", mediatype(&field.field_type)),
            ];
            // signature is an image capture with a drawing appearance.
            let appearance = appearance.or(match field.field_type {
                FieldType::Signature => Some("signature"),
                _ => None,
            });
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, "upload", &attrs)?;
            write_label_hint(w, field, scope, index)?;
            end(w, "upload")?;
        }
        _ => {
            let mut attrs = vec![("ref", path.as_str())];
            let appearance = appearance.or(match field.field_type {
                FieldType::TextArea => Some("multiline"),
                _ => None,
            });
            if let Some(appearance) = appearance {
                attrs.push(("appearance", appearance));
            }
            start(w, "input", &attrs)?;
            write_label_hint(w, field, scope, index)?;
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
    scope: Option<&str>,
    index: &FieldIndex,
) -> Result<(), XFormError> {
    write_display_text(w, "label", &field.label, scope, index, &field.name)?;
    if let Some(hint) = &field.hint {
        write_display_text(w, "hint", hint, scope, index, &field.name)?;
    }
    Ok(())
}

/// Write display text, turning each `${name}` into an inline `<output>`.
///
/// `<output>` is the XForms way to show another node's value inside a label or
/// hint, and it takes a literal xpath, so a single-language form needs no itext
/// translation block. Substituting the path as plain text instead would print
/// the path to the enumerator.
fn write_display_text<W: std::io::Write>(
    w: &mut Writer<W>,
    element: &str,
    text: &str,
    scope: Option<&str>,
    index: &FieldIndex,
    field: &str,
) -> Result<(), XFormError> {
    start(w, element, &[])?;
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        if at > 0 {
            w.write_event(Event::Text(BytesText::new(&rest[..at])))?;
        }
        let after = &rest[at + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| XFormError::MalformedReference {
                field: field.to_string(),
                reference: after.to_string(),
            })?;
        let path = index.resolve(&after[..end], scope, field)?;
        empty(w, "output", &[("value", path.as_str())])?;
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        w.write_event(Event::Text(BytesText::new(rest)))?;
    }
    end(w, element)?;
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
