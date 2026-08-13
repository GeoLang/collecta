//! What an attachment's content type may be.
//!
//! The type arrives with the upload, so it is a field device's claim rather than
//! anything this server observed. Only capture formats are recorded as sent;
//! everything else, markup and scripts included, is recorded and served as
//! opaque bytes, so no upload can choose to come back as a type a browser runs.

/// What an upload is recorded as when its claimed type is not a capture format.
pub const OPAQUE_CONTENT_TYPE: &str = "application/octet-stream";

/// The types a photo, audio, video, or document question actually produces.
const CAPTURE_CONTENT_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/heic",
    "audio/3gpp",
    "audio/aac",
    "audio/mp4",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "video/3gpp",
    "video/mp4",
    "video/quicktime",
    "video/webm",
    "application/pdf",
];

/// The type an upload is stored under and later served with.
///
/// Parameters are dropped and the comparison ignores case, so the stored value
/// is always one of the constants here and can never carry header syntax of its
/// own.
pub fn recorded_content_type(claimed: &str) -> &'static str {
    let media_type = claimed.split(';').next().unwrap_or_default().trim();
    CAPTURE_CONTENT_TYPES
        .iter()
        .find(|capture| capture.eq_ignore_ascii_case(media_type))
        .copied()
        .unwrap_or(OPAQUE_CONTENT_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_types_survive_parameters_and_casing() {
        assert_eq!(recorded_content_type("image/jpeg"), "image/jpeg");
        assert_eq!(
            recorded_content_type("IMAGE/JPEG; charset=binary"),
            "image/jpeg"
        );
        assert_eq!(recorded_content_type(" audio/mp4 "), "audio/mp4");
    }

    #[test]
    fn anything_a_browser_could_run_becomes_opaque() {
        for claimed in [
            "text/html",
            "image/svg+xml",
            "application/xhtml+xml",
            "text/javascript",
            "image/jpeg, text/html",
            "",
        ] {
            assert_eq!(recorded_content_type(claimed), OPAQUE_CONTENT_TYPE);
        }
    }
}
