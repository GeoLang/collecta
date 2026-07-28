// OpenRosa compatibility surface, driven through the real router.
//
// Every XML assertion goes through a parser rather than substring matching, so
// a malformed document fails the test instead of passing on a lucky match.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use collecta_core::form::{Choice, FieldType, Form, FormField};
use collecta_server::openrosa::xform;
use collecta_server::store::{Store, UserRecord};
use collecta_server::{Config, router};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use tower::ServiceExt;
use uuid::Uuid;

const TEST_SECRET: &str = "test-secret-0123456789abcdef0123456789abcdef";
const TEST_EMAIL: &str = "collector@example.com";
const TEST_PASSWORD: &str = "correct horse battery staple";

// ---- xform rendering ---------------------------------------------------

#[test]
fn xform_renders_every_field_type() {
    let form = kitchen_sink_form();
    let xml = xform::render(&form).expect("form renders");
    let doc = parse(&xml);

    // the skeleton collect needs in order to load the form at all.
    let root = doc.first().expect("document has a root");
    assert_eq!(root.name, "h:html");
    assert_eq!(
        root.attr("xmlns").as_deref(),
        Some("http://www.w3.org/2002/xforms")
    );
    assert_eq!(
        root.attr("xmlns:orx").as_deref(),
        Some("http://openrosa.org/xforms")
    );
    assert_eq!(doc.find("h:title").unwrap().text, "Kitchen Sink");

    // instance root carries the form identity collect echoes back on submit.
    let instance_root = doc.find("data").expect("instance root");
    assert_eq!(instance_root.attr("id"), Some(form.id.to_string()));
    assert_eq!(instance_root.attr("version").as_deref(), Some("1"));

    // the meta block collect fills the instanceID into.
    assert!(doc.find("orx:instanceID").is_some());
    let meta_bind = doc
        .all("bind")
        .into_iter()
        .find(|e| e.attr("nodeset").as_deref() == Some("/data/orx:meta/orx:instanceID"))
        .expect("instanceID bind");
    assert_eq!(meta_bind.attr("jr:preload").as_deref(), Some("uid"));

    for (field, expected_type) in [
        ("q_text", "xsd:string"),
        ("q_integer", "xsd:int"),
        ("q_decimal", "xsd:decimal"),
        ("q_date", "xsd:date"),
        ("q_datetime", "xsd:dateTime"),
        ("q_time", "xsd:time"),
        ("q_note", "xsd:string"),
        ("q_geopoint", "geopoint"),
        ("q_geotrace", "geotrace"),
        ("q_geoshape", "geoshape"),
        ("q_photo", "binary"),
        ("q_audio", "binary"),
        ("q_video", "binary"),
        ("q_file", "binary"),
        ("q_signature", "binary"),
        ("q_barcode", "barcode"),
        ("q_select", "xsd:string"),
        ("q_multiselect", "xsd:string"),
    ] {
        let bind = bind_for(&doc, &format!("/data/{field}"));
        assert_eq!(
            bind.attr("type").as_deref(),
            Some(expected_type),
            "bind type for {field}"
        );
    }

    // notes are readonly, and required carries through as an xpath literal.
    assert_eq!(
        bind_for(&doc, "/data/q_note").attr("readonly").as_deref(),
        Some("true()")
    );
    assert_eq!(
        bind_for(&doc, "/data/q_text").attr("required").as_deref(),
        Some("true()")
    );
    assert_eq!(bind_for(&doc, "/data/q_integer").attr("required"), None);

    // uploads pick the right capture intent.
    for (field, mediatype) in [
        ("q_photo", "image/*"),
        ("q_audio", "audio/*"),
        ("q_video", "video/*"),
        ("q_file", "application/*"),
        ("q_signature", "image/*"),
    ] {
        let upload = control(&doc, "upload", &format!("/data/{field}"));
        assert_eq!(
            upload.attr("mediatype").as_deref(),
            Some(mediatype),
            "mediatype for {field}"
        );
    }
    assert_eq!(
        control(&doc, "upload", "/data/q_signature")
            .attr("appearance")
            .as_deref(),
        Some("signature")
    );

    // selects become select1/select with inline choices.
    assert!(doc.find("select1").is_some());
    assert!(doc.find("select").is_some());
    let values: Vec<String> = doc
        .all("value")
        .into_iter()
        .map(|e| e.text.clone())
        .collect();
    assert!(
        values.contains(&"a".to_string()),
        "inline choice values: {values:?}"
    );

    // barcode gets a plain input, no invented appearance.
    assert!(
        control(&doc, "input", "/data/q_barcode")
            .attr("appearance")
            .is_none()
    );
}

#[test]
fn xform_copies_raw_xpath_verbatim_into_binds() {
    let mut form = Form::new("Expressions");
    let mut field = FormField::text("age", "Age");
    // exactly what the xlsform importer preserved, xpath and all.
    field
        .metadata
        .insert("relevant".into(), "${consent} = 'yes'".into());
    field
        .metadata
        .insert("constraint".into(), ". > 0 and . < 120".into());
    field
        .metadata
        .insert("calculation".into(), "concat('a', 'b')".into());
    field
        .metadata
        .insert("constraint_message".into(), "Must be 1-119".into());
    form.add_field(field);

    let xml = xform::render(&form).unwrap();
    let doc = parse(&xml);
    let bind = bind_for(&doc, "/data/age");

    assert_eq!(bind.attr("relevant").as_deref(), Some("${consent} = 'yes'"));
    assert_eq!(
        bind.attr("constraint").as_deref(),
        Some(". > 0 and . < 120")
    );
    assert_eq!(bind.attr("calculate").as_deref(), Some("concat('a', 'b')"));
    assert_eq!(
        bind.attr("jr:constraintMsg").as_deref(),
        Some("Must be 1-119")
    );
}

#[test]
fn xform_nests_repeat_children() {
    let mut form = Form::new("Repeats");
    let mut repeat = FormField::text("household", "Household");
    repeat.field_type = FieldType::Repeat;
    repeat.children = Some(vec![
        FormField::text("member_name", "Member name"),
        FormField::text("member_age", "Member age"),
    ]);
    form.add_field(repeat);

    let xml = xform::render(&form).unwrap();
    let doc = parse(&xml);

    // child nodesets are rooted inside the repeat.
    assert!(doc.find("repeat").is_some());
    assert_eq!(
        doc.find("repeat").unwrap().attr("nodeset").as_deref(),
        Some("/data/household")
    );
    let child = bind_for(&doc, "/data/household/member_name");
    assert_eq!(child.attr("type").as_deref(), Some("xsd:string"));
    // the repeat itself carries no data type.
    assert!(bind_for(&doc, "/data/household").attr("type").is_none());
}

#[test]
fn xform_escapes_markup_in_labels() {
    let mut form = Form::new("Title & <danger>");
    form.add_field(FormField::text("q1", "Pick <b>one</b> & go"));

    let xml = xform::render(&form).unwrap();
    // parsing is the assertion: unescaped markup would produce stray elements.
    let doc = parse(&xml);
    assert_eq!(doc.find("h:title").unwrap().text, "Title & <danger>");
    assert_eq!(doc.find("label").unwrap().text, "Pick <b>one</b> & go");
    assert!(
        doc.all("b").is_empty(),
        "label markup must not become elements"
    );
}

#[test]
fn xform_rejects_field_names_xml_cannot_express() {
    for bad in ["has space", "1leading", "sneaky<tag>", "", "xmlreserved"] {
        let mut form = Form::new("Bad");
        form.add_field(FormField::text(bad, "Label"));
        assert!(
            xform::render(&form).is_err(),
            "field name {bad:?} must be rejected, not mangled"
        );
    }
}

#[test]
fn form_hash_is_md5_of_the_rendered_bytes() {
    // md5("abc"), the protocol's cache-busting value.
    assert_eq!(
        xform::form_hash("abc"),
        "md5:900150983cd24fb0d6963f7d28e17f72"
    );

    let form = kitchen_sink_form();
    let xml = xform::render(&form).unwrap();
    assert_eq!(xform::form_hash(&xml), xform::form_hash(&xml));
    assert_ne!(xform::form_hash(&xml), xform::form_hash("different"));
}

// ---- routes ------------------------------------------------------------

#[tokio::test]
async fn form_list_advertises_hash_and_download_url() {
    let (app, form) = app_with_form().await;

    let resp = app
        .oneshot(authed("GET", "/formList", TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("x-openrosa-version").unwrap(),
        "1.0",
        "collect uses this header to recognise an openrosa server"
    );
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/xml; charset=utf-8"
    );
    assert!(resp.headers().get("date").is_some());

    let body = body_string(resp).await;
    let doc = parse(&body);
    let root = doc.first().unwrap();
    assert_eq!(root.name, "xforms");
    assert_eq!(
        root.attr("xmlns").as_deref(),
        Some("http://openrosa.org/xforms/xformsList")
    );
    assert_eq!(doc.find("formID").unwrap().text, form.id.to_string());
    assert_eq!(doc.find("name").unwrap().text, "Kitchen Sink");
    assert_eq!(doc.find("version").unwrap().text, "1");

    let expected_hash = xform::form_hash(&xform::render(&form).unwrap());
    assert_eq!(doc.find("hash").unwrap().text, expected_hash);
    assert_eq!(
        doc.find("downloadUrl").unwrap().text,
        format!("http://collecta.test/forms/{}/form.xml", form.id)
    );
}

#[tokio::test]
async fn form_download_serves_the_listed_form() {
    let (app, form) = app_with_form().await;

    let uri = format!("/forms/{}/form.xml", form.id);
    let resp = app
        .clone()
        .oneshot(authed("GET", &uri, TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("x-openrosa-version").unwrap(), "1.0");

    let body = body_string(resp).await;
    let doc = parse(&body);
    assert_eq!(doc.first().unwrap().name, "h:html");
    // the bytes served are the bytes the list hashed.
    assert_eq!(
        xform::form_hash(&body),
        xform::form_hash(&xform::render(&form).unwrap())
    );

    let missing = format!("/forms/{}/form.xml", Uuid::new_v4());
    let resp = app
        .oneshot(authed("GET", &missing, TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn openrosa_routes_challenge_unauthenticated_clients() {
    let (app, form) = app_with_form().await;
    let download = format!("/forms/{}/form.xml", form.id);

    for uri in ["/formList", &download] {
        // collect's first request carries no credentials at all.
        let resp = app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} unauthenticated"
        );
        assert_eq!(
            resp.headers().get("www-authenticate").unwrap(),
            r#"Basic realm="collecta", charset="UTF-8""#,
            "{uri} must challenge, not just refuse"
        );
        // the challenge is still an openrosa response.
        assert_eq!(resp.headers().get("x-openrosa-version").unwrap(), "1.0");

        for (email, password) in [
            (TEST_EMAIL, "wrong password"),
            ("nobody@example.com", TEST_PASSWORD),
            (TEST_EMAIL, ""),
        ] {
            let resp = app
                .clone()
                .oneshot(authed("GET", uri, email, password))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} with {email}/{password}"
            );
        }
    }
}

#[tokio::test]
async fn openrosa_rejects_malformed_authorization_headers() {
    let (app, _form) = app_with_form().await;

    let good = BASE64.encode(format!("{TEST_EMAIL}:{TEST_PASSWORD}"));
    for header in [
        "".to_string(),
        "Basic".to_string(),
        "Basic ".to_string(),
        "Basic !!!not-base64!!!".to_string(),
        // valid base64, but no colon separator.
        format!("Basic {}", BASE64.encode("no-colon-here")),
        // a bearer token must not open a basic-auth route.
        format!("Bearer {good}"),
        // unpadded base64 is not canonical.
        format!("Basic {}", good.trim_end_matches('=')),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::get("/formList")
                    .header("Authorization", &header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "Authorization: {header:?}"
        );
    }

    // the scheme token is case-insensitive per rfc 7617.
    for scheme in ["Basic", "basic", "BASIC", "BaSiC"] {
        let resp = app
            .clone()
            .oneshot(
                Request::get("/formList")
                    .header("Authorization", format!("{scheme} {good}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "scheme {scheme}");
    }
}

#[tokio::test]
async fn jwt_api_is_unaffected_by_the_openrosa_routes() {
    let (app, _form) = app_with_form().await;

    // basic credentials must not open the bearer-token api.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/api/v1/forms", TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // and /health stays public.
    let resp = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---- fixtures ----------------------------------------------------------

fn kitchen_sink_form() -> Form {
    let mut form = Form::new("Kitchen Sink");
    form.add_field(FormField::text("q_text", "Text").set_required());
    for (name, field_type) in [
        ("q_integer", FieldType::Integer),
        ("q_decimal", FieldType::Decimal),
        ("q_date", FieldType::Date),
        ("q_datetime", FieldType::DateTime),
        ("q_time", FieldType::Time),
        ("q_note", FieldType::Note),
        ("q_geopoint", FieldType::GeoPoint),
        ("q_geotrace", FieldType::GeoTrace),
        ("q_geoshape", FieldType::GeoShape),
        ("q_photo", FieldType::Photo),
        ("q_audio", FieldType::Audio),
        ("q_video", FieldType::Video),
        ("q_file", FieldType::File),
        ("q_signature", FieldType::Signature),
        ("q_barcode", FieldType::Barcode),
    ] {
        let mut field = FormField::text(name, name);
        field.field_type = field_type;
        form.add_field(field);
    }
    for (name, field_type) in [
        ("q_select", FieldType::Select),
        ("q_multiselect", FieldType::MultiSelect),
    ] {
        let mut field = FormField::text(name, name);
        field.field_type = field_type;
        field.choices = Some(vec![
            Choice {
                value: "a".into(),
                label: "Option A".into(),
            },
            Choice {
                value: "b".into(),
                label: "Option B".into(),
            },
        ]);
        form.add_field(field);
    }
    form
}

async fn app_with_form() -> (axum::Router, Form) {
    let store = Store::connect(":memory:").await.unwrap();
    store
        .create_user(&UserRecord {
            id: Uuid::new_v4(),
            email: TEST_EMAIL.to_string(),
            password_hash: collecta_server::auth::hash_password(TEST_PASSWORD),
            role: "collector".to_string(),
        })
        .await
        .unwrap();
    let form = kitchen_sink_form();
    store.insert_form(&form).await.unwrap();

    let mut config = Config::new(TEST_SECRET, std::env::temp_dir().join("collecta-or-tests"));
    config.base_url = Some("http://collecta.test".to_string());
    (router(store, config), form)
}

fn authed(method: &str, uri: &str, email: &str, password: &str) -> Request<Body> {
    let credentials = BASE64.encode(format!("{email}:{password}"));
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Basic {credentials}"))
        .body(Body::empty())
        .unwrap()
}

async fn body_string(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---- a tiny xml model, so assertions run against parsed output ----------

struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
}

impl Element {
    fn attr(&self, key: &str) -> Option<String> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

trait Doc {
    fn find(&self, name: &str) -> Option<&Element>;
    fn all(&self, name: &str) -> Vec<&Element>;
}

impl Doc for Vec<Element> {
    fn find(&self, name: &str) -> Option<&Element> {
        self.iter().find(|e| e.name == name)
    }
    fn all(&self, name: &str) -> Vec<&Element> {
        self.iter().filter(|e| e.name == name).collect()
    }
}

/// Flatten a document into its elements. Panics on anything not well-formed,
/// which is the point: a broken XForm must fail the test.
fn parse(xml: &str) -> Vec<Element> {
    let mut reader = Reader::from_str(xml);
    // no trim_text: escaped text arrives split around GeneralRef events, and
    // trimming each fragment would silently drop spaces next to an entity.
    let mut elements: Vec<Element> = Vec::new();
    let mut open: Vec<usize> = Vec::new();

    loop {
        match reader.read_event().expect("document is well-formed xml") {
            Event::Eof => break,
            Event::Start(e) => {
                elements.push(element_from(&e));
                open.push(elements.len() - 1);
            }
            Event::Empty(e) => elements.push(element_from(&e)),
            Event::End(_) => {
                open.pop();
            }
            Event::Text(t) => {
                if let Some(&index) = open.last() {
                    let text = t.xml10_content().expect("text decodes");
                    elements[index].text.push_str(&text);
                }
            }
            // quick-xml surfaces entity references as their own events.
            Event::GeneralRef(r) => {
                if let Some(&index) = open.last() {
                    let name = r.decode().expect("reference decodes").into_owned();
                    let resolved = quick_xml::escape::resolve_xml_entity(&name)
                        .map(str::to_string)
                        .or_else(|| r.resolve_char_ref().ok().flatten().map(String::from))
                        .unwrap_or_else(|| panic!("unexpected entity &{name};"));
                    elements[index].text.push_str(&resolved);
                }
            }
            _ => {}
        }
    }
    elements
}

fn element_from(e: &quick_xml::events::BytesStart) -> Element {
    let attributes = e.attributes();
    let decoder = attributes.decoder();
    Element {
        name: String::from_utf8(e.name().as_ref().to_vec()).unwrap(),
        attrs: attributes
            .map(|a| {
                let a = a.expect("attribute parses");
                (
                    String::from_utf8(a.key.as_ref().to_vec()).unwrap(),
                    a.decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
                        .expect("attribute unescapes")
                        .into_owned(),
                )
            })
            .collect(),
        text: String::new(),
    }
}

fn bind_for<'a>(doc: &'a [Element], nodeset: &str) -> &'a Element {
    doc.iter()
        .find(|e| e.name == "bind" && e.attr("nodeset").as_deref() == Some(nodeset))
        .unwrap_or_else(|| panic!("no bind for {nodeset}"))
}

fn control<'a>(doc: &'a [Element], element: &str, reference: &str) -> &'a Element {
    doc.iter()
        .find(|e| e.name == element && e.attr("ref").as_deref() == Some(reference))
        .unwrap_or_else(|| panic!("no {element} for {reference}"))
}
