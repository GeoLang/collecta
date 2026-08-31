//! Fetching form definitions from `GET /api/v1/sync/forms`.

use std::path::Path;

use collecta_core::sync_protocol::FormsPullResponse;
use collecta_core::{PullOutcome, PulledForms};

use crate::Result;
use crate::http;
use crate::json_file;

const FORMS_PATH: &str = "/api/v1/sync/forms";

/// Fetch the forms changed since the stored cursor and record them.
///
/// The file is written only once the server has answered, so a device that
/// cannot reach the server keeps the forms it already had.
pub fn run(forms_path: &Path, server: &str, token: Option<&str>) -> Result<()> {
    let mut forms: PulledForms = json_file::load(forms_path)?;

    println!("pulling from {server}");
    let response: FormsPullResponse =
        http::get(server, FORMS_PATH, token, &[("since", forms.cursor())])?;

    let outcome = forms.apply_pull_response(&response);
    json_file::save(forms_path, &forms)?;
    println!("{}", summarize(outcome, forms.forms().len(), forms_path));
    Ok(())
}

fn summarize(outcome: PullOutcome, held: usize, forms_path: &Path) -> String {
    let path = forms_path.display();
    if outcome.updated == 0 && outcome.removed == 0 {
        return format!("up to date, {held} forms in {path}");
    }
    format!(
        "{} updated, {} deleted, {held} forms in {path}",
        outcome.updated, outcome.removed
    )
}
