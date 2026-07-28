//! `HEAD` and `POST /submission`.
//!
//! ODK Collect splits one submission across several POSTs when its attachments
//! exceed the advertised accepted length, repeating the identical instance XML
//! each time. `meta/instanceID` is therefore the idempotency key: the first POST
//! creates the submission, later ones only add attachment parts.

use axum::Extension;
use axum::extract::{Multipart, State, multipart::Field};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use collecta_core::validation;

use super::instance;
use super::{MAX_CONTENT_LENGTH, OpenRosaError, OpenRosaUser, envelope, error, xml_headers};
use crate::AppState;
use crate::store::InstanceInsert;

/// The multipart part carrying the instance XML.
const INSTANCE_PART: &str = "xml_submission_file";

/// ODK Aggregate's marker for "more parts are coming". Accepted and ignored:
/// merging by instanceID gives the same result without tracking a flag.
const INCOMPLETE_PART: &str = "*isIncomplete*";

/// `HEAD /submission` — the preflight Collect uses to decide whether this is an
/// OpenRosa server and whether its credentials work.
///
/// The spec's status table lists 204 for HEAD, and ODK Collect requires it.
pub async fn probe() -> Response {
    (StatusCode::NO_CONTENT, xml_headers()).into_response()
}

/// One buffered attachment part.
pub struct AttachmentPart {
    /// The multipart part name, which for ODK is the file name the instance
    /// refers to. Metadata only: it never reaches a filesystem path.
    pub name: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// `POST /submission` — ingest one instance plus any attachment parts.
pub async fn submit(
    State(state): State<AppState>,
    Extension(user): Extension<OpenRosaUser>,
    multipart: Multipart,
) -> Result<Response, OpenRosaError> {
    let (instance_xml, attachments) = read_parts(multipart).await?;

    let instance_xml = instance_xml.ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            format!("missing {INSTANCE_PART} part"),
        )
    })?;

    // every parse failure, missing instanceID included, is a bad request: the
    // client sent something this server cannot file.
    let parsed = instance::parse(&instance_xml)
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;

    let form = state
        .store
        .get_form(parsed.form_id)
        .await?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "unknown form"))?;

    let (submission, coercion_errors) =
        instance::to_submission(&parsed, &form, &user.id.to_string());
    if !coercion_errors.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, coercion_errors.join("; ")));
    }

    let validation_errors = validation::validate(&form, &submission);
    if !validation_errors.is_empty() {
        let message = validation_errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(error(StatusCode::BAD_REQUEST, message));
    }

    let instance_hash = hash(&instance_xml);
    let outcome = state
        .store
        .insert_instance(&submission, &parsed.instance_id, &instance_hash)
        .await?;

    let (submission_id, resubmission) = match outcome {
        InstanceInsert::Created(id) => (id, false),
        InstanceInsert::Existing(existing) => {
            // an instanceID this server already holds may only be reused to
            // attach more media to the very same instance. Different bytes mean
            // a different record trying to take an id that is already claimed.
            if existing.instance_hash != instance_hash {
                return Err(error(
                    StatusCode::CONFLICT,
                    "a submission with this instanceID already exists with different xml",
                ));
            }
            // and only its own submitter may extend it, so a guessed or
            // observed instanceID cannot be used to graft files onto someone
            // else's record.
            if existing.submission.collector_id.as_deref() != Some(&*user.id.to_string()) {
                return Err(error(
                    StatusCode::CONFLICT,
                    "a submission with this instanceID belongs to another user",
                ));
            }
            (existing.submission.id, true)
        }
    };

    let stored = store_attachments(&state, submission_id, &attachments).await?;

    let message = if resubmission {
        format!("attachments received ({stored} new)")
    } else {
        "full submission upload was successful!".to_string()
    };
    Ok((StatusCode::CREATED, xml_headers(), envelope(&message)).into_response())
}

/// Read every multipart part, enforcing the per-part cap while streaming.
async fn read_parts(
    mut multipart: Multipart,
) -> Result<(Option<Vec<u8>>, Vec<AttachmentPart>), OpenRosaError> {
    let mut instance_xml = None;
    let mut attachments = Vec::new();

    loop {
        let field = multipart.next_field().await.map_err(|e| {
            // a body over the layer's limit surfaces here; report it as the
            // protocol's "too large" rather than a generic failure.
            let status = e.status();
            if status == StatusCode::PAYLOAD_TOO_LARGE {
                error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "submission exceeds the accepted content length",
                )
            } else {
                error(StatusCode::BAD_REQUEST, "malformed multipart body")
            }
        })?;
        let Some(field) = field else { break };

        let name = field.name().unwrap_or_default().to_string();
        let filename = field.file_name().map(str::to_string);
        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();

        if name == INCOMPLETE_PART {
            let _ = read_capped(field).await?;
            continue;
        }

        let bytes = read_capped(field).await?;
        if name == INSTANCE_PART {
            instance_xml = Some(bytes);
        } else {
            attachments.push(AttachmentPart {
                name,
                filename,
                content_type,
                bytes,
            });
        }
    }

    Ok((instance_xml, attachments))
}

/// Buffer one part, aborting as soon as it passes the per-part cap.
///
/// Read chunk by chunk rather than with `Field::bytes()` so an oversized part
/// is refused after one chunk instead of being fully buffered first.
async fn read_capped(mut field: Field<'_>) -> Result<Vec<u8>, OpenRosaError> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let chunk = field
            .chunk()
            .await
            .map_err(|_| error(StatusCode::BAD_REQUEST, "malformed multipart part"))?;
        let Some(chunk) = chunk else { break };
        if bytes.len() + chunk.len() > MAX_CONTENT_LENGTH {
            return Err(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "attachment exceeds the accepted content length",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Persisting attachment bytes lands in the next milestone.
async fn store_attachments(
    _state: &AppState,
    _submission_id: uuid::Uuid,
    attachments: &[AttachmentPart],
) -> Result<usize, OpenRosaError> {
    Ok(attachments.len())
}

/// Content hash used to tell a genuine resubmission from an instanceID
/// collision. SHA-256 rather than the protocol's MD5: this one decides whether
/// an existing record may be extended, so it has to resist a crafted collision.
fn hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
