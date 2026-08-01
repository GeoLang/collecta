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
use collecta_core::submission::{FieldValue, GeoPoint};
use collecta_server::openrosa::xform;
use collecta_server::store::{FormWriter, Store, UserRecord};
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
fn xform_rewrites_field_references_to_xpath() {
    let mut form = Form::new("Expressions");
    form.add_field(FormField::text("consent", "Consent given"));
    let mut field = FormField::text("age", "Age");
    // exactly what the xlsform importer preserved, ${} shorthand and all.
    field
        .metadata
        .insert("relevant".into(), "${consent} = 'yes'".into());
    field
        .metadata
        .insert("constraint".into(), ". > 0 and . < 120".into());
    field
        .metadata
        .insert("calculation".into(), "concat(${consent}, 'x')".into());
    field
        .metadata
        .insert("constraint_message".into(), "Must be 1-119".into());
    form.add_field(field);

    let xml = xform::render(&form).unwrap();
    let doc = parse(&xml);
    let bind = bind_for(&doc, "/data/age");

    // ${consent} is xlsform shorthand, not xpath. JavaRosa rejects the form if
    // it survives into the bind, so it must become the field's path.
    let relevant = bind.attr("relevant").unwrap();
    assert!(
        relevant.contains("/data/consent") && relevant.contains("= 'yes'"),
        "relevant was {relevant:?}"
    );
    // expressions with no reference are passed through untouched.
    assert_eq!(
        bind.attr("constraint").as_deref(),
        Some(". > 0 and . < 120")
    );
    let calculate = bind.attr("calculate").unwrap();
    assert!(
        calculate.contains("concat(") && calculate.contains("/data/consent"),
        "calculate was {calculate:?}"
    );
    assert_eq!(
        bind.attr("jr:constraintMsg").as_deref(),
        Some("Must be 1-119")
    );

    assert!(
        !xml.contains("${"),
        "no xlsform shorthand may survive into the document"
    );
}

#[test]
fn xform_rewrites_references_inside_repeats_relative_to_the_instance() {
    let mut form = Form::new("Repeat references");
    form.add_field(FormField::text("survey_date", "Survey date"));

    let mut child_name = FormField::text("child_name", "Child name");
    // a reference to a top-level field from inside a repeat stays absolute.
    child_name
        .metadata
        .insert("relevant".into(), "${survey_date} != ''".into());

    let mut child_age = FormField::text("child_age", "Child age");
    // a reference to a sibling in the same repeat must be relative: an
    // absolute path would resolve to the first repeat instance every time.
    child_age
        .metadata
        .insert("relevant".into(), "${child_name} != ''".into());
    child_age
        .metadata
        .insert("constraint".into(), ". > 0".into());

    let mut repeat = FormField::text("child", "Children");
    repeat.field_type = FieldType::Repeat;
    repeat.children = Some(vec![child_name, child_age]);
    form.add_field(repeat);

    let xml = xform::render(&form).unwrap();
    let doc = parse(&xml);

    // pyxform pads substitutions with spaces, so compare on collapsed runs.
    assert_eq!(
        collapse(
            &bind_for(&doc, "/data/child/child_age")
                .attr("relevant")
                .unwrap()
        ),
        "../child_name != ''",
        "same-repeat sibling reference must be relative"
    );
    let cross = bind_for(&doc, "/data/child/child_name")
        .attr("relevant")
        .unwrap();
    assert!(
        cross.contains("/data/survey_date") && !cross.contains(".."),
        "reference out of the repeat stays absolute, was {cross:?}"
    );
    assert!(!xml.contains("${"));
}

#[test]
fn xform_renders_references_in_labels_as_outputs() {
    let mut form = Form::new("Labels");
    form.add_field(FormField::text("child_name", "Child name"));
    let mut age = FormField::text("child_age", "Age of ${child_name}");
    age.hint = Some("How old is ${child_name}?".into());
    form.add_field(age);

    let xml = xform::render(&form).unwrap();
    let doc = parse(&xml);

    // a label cannot hold an xpath as text, it needs an <output> child.
    let outputs = doc.all("output");
    assert_eq!(
        outputs.len(),
        2,
        "one output for the label, one for the hint"
    );
    for output in outputs {
        assert_eq!(output.attr("value").as_deref(), Some("/data/child_name"));
    }
    // the literal text around the reference survives as label text.
    assert!(
        doc.all("label").iter().any(|l| l.text.contains("Age of")),
        "labels were {:?}",
        doc.all("label").iter().map(|l| &l.text).collect::<Vec<_>>()
    );
    assert!(!xml.contains("${"));
}

#[test]
fn xform_refuses_references_it_cannot_resolve() {
    // a reference to a field that does not exist.
    let mut form = Form::new("Dangling");
    let mut field = FormField::text("age", "Age");
    field
        .metadata
        .insert("relevant".into(), "${nonexistent} = 'yes'".into());
    form.add_field(field);
    let error = xform::render(&form).unwrap_err().to_string();
    assert!(
        error.contains("nonexistent") && error.contains("age"),
        "the error must name both the reference and the field, was {error:?}"
    );

    // an unterminated or malformed reference.
    for expression in ["${unterminated", "${not a name}", "${last-saved#age}"] {
        let mut form = Form::new("Malformed");
        form.add_field(FormField::text("age", "Age"));
        let mut field = FormField::text("other", "Other");
        field.metadata.insert("relevant".into(), expression.into());
        form.add_field(field);
        assert!(
            xform::render(&form).is_err(),
            "{expression:?} must fail rendering, not emit broken xpath"
        );
    }

    // a reference in the constraint message, which is a plain attribute and
    // cannot carry an <output>.
    let mut form = Form::new("Message");
    form.add_field(FormField::text("limit", "Limit"));
    let mut field = FormField::text("age", "Age");
    field
        .metadata
        .insert("constraint_message".into(), "Must be under ${limit}".into());
    form.add_field(field);
    let error = xform::render(&form).unwrap_err().to_string();
    assert!(error.contains("constraint_message"), "was {error:?}");
}

#[test]
fn xform_refuses_ambiguous_references() {
    // the same name at the top level and inside a repeat: a reference to it
    // could mean either, so it must not be guessed.
    let mut form = Form::new("Ambiguous");
    form.add_field(FormField::text("name", "Name"));
    let mut repeat = FormField::text("group", "Group");
    repeat.field_type = FieldType::Repeat;
    repeat.children = Some(vec![FormField::text("name", "Name")]);
    form.add_field(repeat);
    let mut field = FormField::text("other", "Other");
    field
        .metadata
        .insert("relevant".into(), "${name} != ''".into());
    form.add_field(field);

    let error = xform::render(&form).unwrap_err().to_string();
    assert!(error.contains("name"), "was {error:?}");
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

// ---- submission --------------------------------------------------------

#[tokio::test]
async fn head_probe_returns_204_with_the_accepted_length() {
    let (app, _form, _store) = app_parts().await;

    let resp = app
        .clone()
        .oneshot(authed("HEAD", "/submission", TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    // collect requires exactly 204 here, not 200.
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(resp.headers().get("x-openrosa-version").unwrap(), "1.0");
    assert_eq!(
        resp.headers()
            .get("x-openrosa-accept-content-length")
            .unwrap(),
        "52428800"
    );

    // the probe is authenticated, and an unauthenticated one gets the
    // challenge rather than a 500.
    let resp = app
        .oneshot(Request::head("/submission").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("www-authenticate").is_some());
}

#[tokio::test]
async fn submission_becomes_typed_field_values() {
    let (app, form, store) = app_parts().await;

    let xml = instance_xml(
        form.id,
        "uuid:11111111-1111-1111-1111-111111111111",
        r#"<q_text>Alpha Site</q_text>
  <q_integer>42</q_integer>
  <q_decimal>3.5</q_decimal>
  <q_date>2026-07-28</q_date>
  <q_geopoint>51.5 -0.12 35.0 4.2</q_geopoint>
  <q_geotrace>51.5 -0.12 0 0; 51.6 -0.13 0 0</q_geotrace>
  <q_barcode>ABC-123</q_barcode>
  <q_photo>1553025782376.jpg</q_photo>
  <q_select>a</q_select>
  <q_multiselect>a b</q_multiselect>"#,
    );

    let resp = post_submission(&app, &[instance_part(&xml)]).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/xml; charset=utf-8"
    );

    // the success body is a parseable OpenRosaResponse.
    let body = body_string(resp).await;
    let doc = parse(&body);
    assert_eq!(doc.first().unwrap().name, "OpenRosaResponse");
    assert_eq!(
        doc.first().unwrap().attr("xmlns").as_deref(),
        Some("http://openrosa.org/http/response")
    );
    assert!(doc.find("message").is_some());

    let stored = store.list_submissions(form.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    let values = &stored[0].values;

    assert_eq!(
        values.get("q_text"),
        Some(&FieldValue::Text("Alpha Site".into()))
    );
    assert_eq!(values.get("q_integer"), Some(&FieldValue::Integer(42)));
    assert_eq!(values.get("q_decimal"), Some(&FieldValue::Decimal(3.5)));
    assert_eq!(
        values.get("q_date"),
        Some(&FieldValue::Date("2026-07-28".into()))
    );
    assert_eq!(
        values.get("q_geopoint"),
        Some(&FieldValue::GeoPoint(GeoPoint {
            latitude: 51.5,
            longitude: -0.12,
            altitude: Some(35.0),
            accuracy: Some(4.2),
        }))
    );
    match values.get("q_geotrace") {
        Some(FieldValue::GeoTrace(points)) => assert_eq!(points.len(), 2),
        other => panic!("expected a geotrace, got {other:?}"),
    }
    assert_eq!(
        values.get("q_barcode"),
        Some(&FieldValue::Barcode("ABC-123".into()))
    );
    assert_eq!(
        values.get("q_select"),
        Some(&FieldValue::Choice("a".into()))
    );
    assert_eq!(
        values.get("q_multiselect"),
        Some(&FieldValue::MultiChoice(vec!["a".into(), "b".into()]))
    );
    // a binary node holds the file name the attachment part will be keyed by.
    assert_eq!(
        values.get("q_photo"),
        Some(&FieldValue::Text("1553025782376.jpg".into()))
    );
    // the authenticated user is recorded, not anything the client claimed.
    assert!(stored[0].collector_id.is_some());
}

#[tokio::test]
async fn repeated_post_of_one_instance_id_stores_one_submission() {
    let (app, form, store) = app_parts().await;
    let xml = minimal_instance(form.id, "uuid:22222222-2222-2222-2222-222222222222");

    // collect resends the identical instance with each attachment batch.
    for attempt in 0..3 {
        let resp = post_submission(&app, &[instance_part(&xml)]).await;
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "attempt {attempt} must report success so collect stops retrying"
        );
    }

    let stored = store.list_submissions(form.id).await.unwrap();
    assert_eq!(stored.len(), 1, "instanceID is the idempotency key");
}

#[tokio::test]
async fn instance_id_reused_with_different_xml_conflicts() {
    let (app, form, store) = app_parts().await;
    let instance_id = "uuid:33333333-3333-3333-3333-333333333333";

    let first = instance_xml(form.id, instance_id, "<q_text>Original</q_text>");
    assert_eq!(
        post_submission(&app, &[instance_part(&first)])
            .await
            .status(),
        StatusCode::CREATED
    );

    // same key, different content: this is a collision, not a resubmission,
    // and must never overwrite what is already filed.
    let tampered = instance_xml(form.id, instance_id, "<q_text>Tampered</q_text>");
    let resp = post_submission(&app, &[instance_part(&tampered)]).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let stored = store.list_submissions(form.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].values.get("q_text"),
        Some(&FieldValue::Text("Original".into())),
        "the stored submission must be untouched"
    );
}

#[tokio::test]
async fn another_user_cannot_extend_someone_elses_instance() {
    let (app, form, store) = app_parts().await;
    store
        .create_user(&UserRecord {
            id: Uuid::new_v4(),
            email: "intruder@example.com".to_string(),
            password_hash: collecta_server::auth::hash_password(TEST_PASSWORD),
            role: "editor".to_string(),
        })
        .await
        .unwrap();

    let instance_id = "uuid:44444444-4444-4444-4444-444444444444";
    let xml = minimal_instance(form.id, instance_id);
    assert_eq!(
        post_submission(&app, &[instance_part(&xml)]).await.status(),
        StatusCode::CREATED
    );

    // byte-identical xml, so the content check passes; ownership is what stops
    // an observed instanceID being used to graft files onto another record.
    let resp = app
        .oneshot(multipart_request(
            "/submission",
            "intruder@example.com",
            TEST_PASSWORD,
            &[instance_part(&xml)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    assert_eq!(store.list_submissions(form.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn submissions_without_an_instance_id_are_rejected() {
    let (app, form, store) = app_parts().await;

    let no_meta = format!(
        r#"<?xml version="1.0"?><data id="{}" version="1"><q_text>x</q_text></data>"#,
        form.id
    );
    let empty_meta = format!(
        r#"<?xml version="1.0"?><data id="{}" version="1"><q_text>x</q_text>
        <meta xmlns="http://openrosa.org/xforms"><instanceID></instanceID></meta></data>"#,
        form.id
    );

    for xml in [no_meta, empty_meta] {
        let resp = post_submission(&app, &[instance_part(&xml)]).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // the message reaches the user through the envelope.
        let doc = parse(&body_string(resp).await);
        assert_eq!(doc.first().unwrap().name, "OpenRosaResponse");
    }
    assert!(store.list_submissions(form.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn malformed_posts_are_rejected_not_crashed() {
    let (app, form, store) = app_parts().await;
    let good = minimal_instance(form.id, "uuid:55555555-5555-5555-5555-555555555555");

    // no instance part at all.
    let resp = post_submission(
        &app,
        &[part(
            "some_file",
            Some("a.jpg"),
            "image/jpeg",
            b"x".to_vec(),
        )],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // instance that is not xml.
    let resp = post_submission(&app, &[instance_part("<data><unclosed>")]).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // a form id that is not a uuid, and one that is unknown.
    let bogus = r#"<?xml version="1.0"?><data id="not-a-uuid"><meta xmlns="http://openrosa.org/xforms"><instanceID>uuid:x</instanceID></meta></data>"#;
    assert_eq!(
        post_submission(&app, &[instance_part(bogus)])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let unknown = minimal_instance(Uuid::new_v4(), "uuid:66666666-6666-6666-6666-666666666666");
    assert_eq!(
        post_submission(&app, &[instance_part(&unknown)])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // a value that does not fit its declared type.
    let bad_type = instance_xml(
        form.id,
        "uuid:77777777-7777-7777-7777-777777777777",
        "<q_text>x</q_text><q_integer>not a number</q_integer>",
    );
    assert_eq!(
        post_submission(&app, &[instance_part(&bad_type)])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    // and the good one still works afterwards.
    assert_eq!(
        post_submission(&app, &[instance_part(&good)])
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(store.list_submissions(form.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn validation_failures_come_back_in_the_envelope() {
    let (app, form, store) = app_parts().await;

    // q_text is required by the form; omitting it must not store anything.
    let xml = instance_xml(
        form.id,
        "uuid:88888888-8888-8888-8888-888888888888",
        "<q_integer>1</q_integer>",
    );
    let resp = post_submission(&app, &[instance_part(&xml)]).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let doc = parse(&body_string(resp).await);
    assert_eq!(doc.first().unwrap().name, "OpenRosaResponse");
    assert!(
        doc.find("message").unwrap().text.contains("q_text"),
        "the message must name the offending field"
    );
    assert!(store.list_submissions(form.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn xml_entity_expansion_is_refused() {
    let (app, form, store) = app_parts().await;

    // the classic billion-laughs shape: quick-xml never substitutes declared
    // entities, and we reject the dtd outright rather than trusting that.
    let billion_laughs = format!(
        r#"<?xml version="1.0"?>
<!DOCTYPE data [
  <!ENTITY lol "lol">
  <!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
  <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
]>
<data id="{}" version="1">
  <q_text>&lol3;</q_text>
  <meta xmlns="http://openrosa.org/xforms"><instanceID>uuid:aaaa</instanceID></meta>
</data>"#,
        form.id
    );
    let resp = post_submission(&app, &[instance_part(&billion_laughs)]).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // an external-entity reference with no dtd at all is still refused.
    let undefined_entity = instance_xml(
        form.id,
        "uuid:99999999-9999-9999-9999-999999999999",
        "<q_text>&xxe;</q_text>",
    );
    let resp = post_submission(&app, &[instance_part(&undefined_entity)]).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    assert!(store.list_submissions(form.id).await.unwrap().is_empty());

    // the five predefined entities and numeric char refs still work.
    let escaped = instance_xml(
        form.id,
        "uuid:abababab-abab-abab-abab-abababababab",
        "<q_text>a &amp; b &#65;</q_text>",
    );
    assert_eq!(
        post_submission(&app, &[instance_part(&escaped)])
            .await
            .status(),
        StatusCode::CREATED
    );
    let stored = store.list_submissions(form.id).await.unwrap();
    assert_eq!(
        stored[0].values.get("q_text"),
        Some(&FieldValue::Text("a & b A".into()))
    );
}

#[tokio::test]
async fn deeply_nested_instances_are_refused() {
    let (app, form, _store) = app_parts().await;

    let depth = 500;
    let mut xml = format!(
        r#"<?xml version="1.0"?><data id="{}" version="1">"#,
        form.id
    );
    xml.push_str(&"<a>".repeat(depth));
    xml.push_str(&"</a>".repeat(depth));
    xml.push_str("</data>");

    let resp = post_submission(&app, &[instance_part(&xml)]).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn submission_requires_basic_auth() {
    let (app, form, store) = app_parts().await;
    let xml = minimal_instance(form.id, "uuid:cdcdcdcd-cdcd-cdcd-cdcd-cdcdcdcdcdcd");

    let resp = app
        .clone()
        .oneshot(
            Request::post("/submission")
                .header("content-type", "multipart/form-data; boundary=X")
                .body(Body::from("--X--\r\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("www-authenticate").is_some());

    let resp = app
        .clone()
        .oneshot(multipart_request(
            "/submission",
            TEST_EMAIL,
            "wrong password",
            &[instance_part(&xml)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    assert!(store.list_submissions(form.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn only_accounts_that_may_write_can_submit() {
    let (app, form, store) = app_parts().await;
    for (email, role) in [
        ("viewer@example.com", "viewer"),
        ("legacy-role@example.com", "collector"),
    ] {
        store
            .create_user(&UserRecord {
                id: Uuid::new_v4(),
                email: email.to_string(),
                password_hash: collecta_server::auth::hash_password(TEST_PASSWORD),
                role: role.to_string(),
            })
            .await
            .unwrap();
    }

    let xml = minimal_instance(form.id, "uuid:efefefef-efef-efef-efef-efefefefefef");
    for email in ["viewer@example.com", "legacy-role@example.com"] {
        let resp = app
            .clone()
            .oneshot(multipart_request(
                "/submission",
                email,
                TEST_PASSWORD,
                &[instance_part(&xml)],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{email}");
    }

    // a viewer can still discover forms; the role only stops the write.
    let resp = app
        .clone()
        .oneshot(authed(
            "GET",
            "/formList",
            "viewer@example.com",
            TEST_PASSWORD,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // and an unknown role does not even get that far.
    let resp = app
        .clone()
        .oneshot(authed(
            "GET",
            "/formList",
            "legacy-role@example.com",
            TEST_PASSWORD,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    assert!(store.list_submissions(form.id).await.unwrap().is_empty());
}

// ---- attachments -------------------------------------------------------

#[tokio::test]
async fn attachments_are_written_and_linked_to_their_field() {
    let (app, form, store, dir) = app_parts_with_dir().await;

    let xml = instance_xml(
        form.id,
        "uuid:aaaa1111-0000-0000-0000-000000000001",
        "<q_text>Alpha</q_text><q_photo>photo1.jpg</q_photo>",
    );
    let resp = post_submission(
        &app,
        &[
            instance_part(&xml),
            part(
                "photo1.jpg",
                Some("photo1.jpg"),
                "image/jpeg",
                b"JPEGBYTES".to_vec(),
            ),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let stored = store.list_submissions(form.id).await.unwrap();
    let attachments = store.list_attachments(stored[0].id).await.unwrap();
    assert_eq!(attachments.len(), 1);

    let attachment = &attachments[0];
    assert_eq!(attachment.filename, "photo1.jpg");
    assert_eq!(attachment.content_type, "image/jpeg");
    assert_eq!(attachment.size_bytes, 9);
    // the part is attributed to the question whose value names that file.
    assert_eq!(attachment.field_name, "q_photo");

    // the bytes really landed on disk, under the configured data directory.
    let path = std::path::Path::new(&attachment.storage_path);
    assert_eq!(std::fs::read(path).unwrap(), b"JPEGBYTES");
    assert!(
        path.starts_with(dir.path()),
        "{path:?} escaped the data directory"
    );
    // the stored path is built from uuids only, never the client's name.
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        attachment.id.to_string()
    );
    assert!(!attachment.storage_path.contains("photo1.jpg"));

    // and the submission itself references it.
    let stored = store.list_submissions(form.id).await.unwrap();
    assert_eq!(stored[0].attachments.len(), 1);
    assert_eq!(stored[0].attachments[0].filename, "photo1.jpg");
}

#[tokio::test]
async fn hostile_attachment_names_cannot_escape_the_data_directory() {
    let (app, form, store, dir) = app_parts_with_dir().await;

    let hostile = [
        "../../../../../../tmp/collecta-pwned",
        "..\\..\\..\\windows\\system32\\evil",
        "/etc/cron.d/collecta",
        "....//....//escape",
        "a/../../b",
        ".",
        "..",
        "",
    ];

    let mut parts = vec![instance_part(&instance_xml(
        form.id,
        "uuid:aaaa1111-0000-0000-0000-000000000002",
        "<q_text>Alpha</q_text>",
    ))];
    for (index, name) in hostile.iter().enumerate() {
        parts.push(part(
            name,
            Some(name),
            "application/octet-stream",
            format!("payload{index}").into_bytes(),
        ));
    }

    let resp = post_submission(&app, &parts).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let stored = store.list_submissions(form.id).await.unwrap();
    let attachments = store.list_attachments(stored[0].id).await.unwrap();

    for attachment in &attachments {
        let path = std::path::Path::new(&attachment.storage_path);
        // every stored file sits directly under <data>/attachments/<sub>/.
        assert!(
            path.starts_with(dir.path()),
            "{path:?} escaped the data directory"
        );
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            attachment.id.to_string(),
            "the file name must be a server-generated uuid"
        );
        assert!(std::fs::read(path).is_ok(), "{path:?} should exist");
    }

    // nothing was written outside the data directory.
    assert!(!std::path::Path::new("/tmp/collecta-pwned").exists());
}

#[tokio::test]
async fn resent_attachment_parts_are_not_duplicated() {
    let (app, form, store, _dir) = app_parts_with_dir().await;
    let instance_id = "uuid:aaaa1111-0000-0000-0000-000000000003";
    let xml = instance_xml(
        form.id,
        instance_id,
        "<q_text>Alpha</q_text><q_photo>photo1.jpg</q_photo>",
    );

    // odk splits a large submission into several posts, repeating the instance
    // and, on a retry, sometimes a part it already delivered.
    let photo = || {
        part(
            "photo1.jpg",
            Some("photo1.jpg"),
            "image/jpeg",
            b"A".to_vec(),
        )
    };
    let audio = || part("clip.m4a", Some("clip.m4a"), "audio/mp4", b"B".to_vec());

    assert_eq!(
        post_submission(&app, &[instance_part(&xml), photo()])
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        post_submission(&app, &[instance_part(&xml), audio()])
            .await
            .status(),
        StatusCode::CREATED
    );
    // a full retry of the first post.
    assert_eq!(
        post_submission(&app, &[instance_part(&xml), photo()])
            .await
            .status(),
        StatusCode::CREATED
    );

    let stored = store.list_submissions(form.id).await.unwrap();
    assert_eq!(stored.len(), 1, "still one submission");
    let attachments = store.list_attachments(stored[0].id).await.unwrap();
    assert_eq!(attachments.len(), 2, "one row per distinct file");
    let names: Vec<&str> = attachments.iter().map(|a| a.filename.as_str()).collect();
    assert!(names.contains(&"photo1.jpg") && names.contains(&"clip.m4a"));
}

#[tokio::test]
async fn oversized_parts_are_refused_rather_than_stored() {
    let (app, form, store, _dir) = app_parts_with_dir().await;
    let xml = instance_xml(
        form.id,
        "uuid:aaaa1111-0000-0000-0000-000000000004",
        "<q_text>Alpha</q_text>",
    );

    // one part past the advertised accepted length.
    let huge = vec![b'x'; collecta_server::openrosa::MAX_CONTENT_LENGTH + 1];
    let resp = post_submission(
        &app,
        &[
            instance_part(&xml),
            part("big.bin", Some("big.bin"), "application/octet-stream", huge),
        ],
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "an oversized body must be refused, not 500"
    );

    // nothing was stored, and the error is still an openrosa envelope.
    assert!(store.list_submissions(form.id).await.unwrap().is_empty());
    let doc = parse(&body_string(resp).await);
    assert_eq!(doc.first().unwrap().name, "OpenRosaResponse");
}

#[tokio::test]
async fn attachments_above_the_default_body_limit_are_accepted() {
    let (app, form, store, _dir) = app_parts_with_dir().await;
    let xml = instance_xml(
        form.id,
        "uuid:aaaa1111-0000-0000-0000-000000000005",
        "<q_text>Alpha</q_text><q_photo>big.jpg</q_photo>",
    );

    // axum's default body limit is 2 MiB. A 4 MiB photo proves the submission
    // route really raised it to the advertised length, rather than the earlier
    // oversize test tripping the default.
    let size = 4 * 1024 * 1024;
    let resp = post_submission(
        &app,
        &[
            instance_part(&xml),
            part("big.jpg", Some("big.jpg"), "image/jpeg", vec![b'x'; size]),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let stored = store.list_submissions(form.id).await.unwrap();
    let attachments = store.list_attachments(stored[0].id).await.unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].size_bytes, size as u64);
}

#[tokio::test]
async fn a_request_over_the_total_body_limit_is_413_not_400() {
    let (app, form, store, _dir) = app_parts_with_dir().await;
    let xml = instance_xml(
        form.id,
        "uuid:aaaa1111-0000-0000-0000-000000000006",
        "<q_text>Alpha</q_text>",
    );

    // two parts, each comfortably inside the per-part cap, together over the
    // request cap: only the body limit can catch this, not the per-part check.
    let half = collecta_server::openrosa::MAX_REQUEST_BODY / 2 + 1024 * 1024;
    assert!(half < collecta_server::openrosa::MAX_CONTENT_LENGTH);
    let resp = post_submission(
        &app,
        &[
            instance_part(&xml),
            part(
                "a.bin",
                Some("a.bin"),
                "application/octet-stream",
                vec![b'a'; half],
            ),
            part(
                "b.bin",
                Some("b.bin"),
                "application/octet-stream",
                vec![b'b'; half],
            ),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(store.list_submissions(form.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_same_instance_id_on_a_different_form_is_a_separate_submission() {
    let (app, form, store, _dir) = app_parts_with_dir().await;

    // a second form, so the two share an instanceID but nothing else.
    let mut other = kitchen_sink_form();
    other.title = "Other".to_string();
    store
        .insert_form(&other, FormWriter::system())
        .await
        .unwrap();

    let instance_id = "uuid:aaaa1111-0000-0000-0000-000000000007";
    for form_id in [form.id, other.id] {
        let xml = instance_xml(form_id, instance_id, "<q_text>Alpha</q_text>");
        assert_eq!(
            post_submission(&app, &[instance_part(&xml)]).await.status(),
            StatusCode::CREATED,
            "instanceID uniqueness is scoped to one form"
        );
    }

    // neither clobbered the other.
    assert_eq!(store.list_submissions(form.id).await.unwrap().len(), 1);
    assert_eq!(store.list_submissions(other.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn openrosa_headers_do_not_leak_onto_the_json_api() {
    let (app, _form, _store) = app_parts().await;

    let resp = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("x-openrosa-version").is_none(),
        "the openrosa layer must stay on its own routes"
    );
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
    let (app, form, _store) = app_parts().await;
    (app, form)
}

async fn app_parts() -> (axum::Router, Form, Store) {
    let (app, form, store, dir) = app_parts_with_dir().await;
    // the json-api assertions never look at disk; keep the directory alive for
    // the duration of the test anyway.
    std::mem::forget(dir);
    (app, form, store)
}

/// The router plus a direct handle on the store and the data directory, so
/// tests can assert on what was actually persisted rather than on the response.
async fn app_parts_with_dir() -> (axum::Router, Form, Store, tempfile::TempDir) {
    let store = Store::connect(":memory:").await.unwrap();
    store
        .create_user(&UserRecord {
            id: Uuid::new_v4(),
            email: TEST_EMAIL.to_string(),
            password_hash: collecta_server::auth::hash_password(TEST_PASSWORD),
            role: "editor".to_string(),
        })
        .await
        .unwrap();
    let form = kitchen_sink_form();
    store
        .insert_form(&form, FormWriter::system())
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new(TEST_SECRET, dir.path());
    config.base_url = Some("http://collecta.test".to_string());
    (router(store.clone(), config), form, store, dir)
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

// ---- multipart and instance fixtures -----------------------------------

/// One multipart part: name, optional filename, content type, bytes.
struct Part {
    name: String,
    filename: Option<String>,
    content_type: String,
    bytes: Vec<u8>,
}

fn part(name: &str, filename: Option<&str>, content_type: &str, bytes: Vec<u8>) -> Part {
    Part {
        name: name.to_string(),
        filename: filename.map(str::to_string),
        content_type: content_type.to_string(),
        bytes,
    }
}

fn instance_part(xml: &str) -> Part {
    part(
        "xml_submission_file",
        Some("submission.xml"),
        "text/xml",
        xml.as_bytes().to_vec(),
    )
}

const BOUNDARY: &str = "----collectaTestBoundary";

fn multipart_body(parts: &[Part]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let disposition = match &part.filename {
            Some(filename) => format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                part.name, filename
            ),
            None => format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name),
        };
        body.extend_from_slice(disposition.as_bytes());
        body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", part.content_type).as_bytes());
        body.extend_from_slice(&part.bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn multipart_request(uri: &str, email: &str, password: &str, parts: &[Part]) -> Request<Body> {
    let credentials = BASE64.encode(format!("{email}:{password}"));
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("Authorization", format!("Basic {credentials}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_body(parts)))
        .unwrap()
}

async fn post_submission(app: &axum::Router, parts: &[Part]) -> Response {
    app.clone()
        .oneshot(multipart_request(
            "/submission",
            TEST_EMAIL,
            TEST_PASSWORD,
            parts,
        ))
        .await
        .unwrap()
}

/// An instance shaped the way ODK Collect submits: form id and version on the
/// root, answers as children, instanceID in the openrosa meta block.
fn instance_xml(form_id: Uuid, instance_id: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<data id="{form_id}" version="1">
  {body}
  <meta xmlns="http://openrosa.org/xforms">
    <instanceID>{instance_id}</instanceID>
  </meta>
</data>"#
    )
}

fn minimal_instance(form_id: Uuid, instance_id: &str) -> String {
    instance_xml(form_id, instance_id, "<q_text>Alpha</q_text>")
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

/// Collapse whitespace runs, so assertions do not depend on the padding around
/// a substituted reference.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
