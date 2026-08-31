//! Pulled forms — the form definitions a client has fetched, and how far it read.

use serde::{Deserialize, Serialize};

use crate::form::Form;
use crate::sync_protocol::FormsPullResponse;

/// The form definitions a client holds, with the cursor of the last pull.
///
/// The whole thing serializes, so a client can keep it in a file between runs
/// and pick up where it left off.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PulledForms {
    forms: Vec<Form>,
    cursor: String,
}

/// What one pull changed on the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullOutcome {
    /// Forms stored for the first time or overwritten with a newer definition.
    pub updated: usize,
    /// Forms dropped because the response tombstoned them.
    pub removed: usize,
}

impl PulledForms {
    /// Create an empty set, which pulls everything on its first request.
    pub fn new() -> Self {
        Self::default()
    }

    /// The forms held, in the order they first arrived.
    pub fn forms(&self) -> &[Form] {
        &self.forms
    }

    /// The cursor to send as `since` on the next pull. Empty until the first
    /// response.
    pub fn cursor(&self) -> &str {
        &self.cursor
    }

    /// Store the definitions the response carries, drop the ones it tombstones,
    /// and take its cursor.
    ///
    /// A form already held is replaced by the incoming definition, so pulling
    /// the same window twice leaves the same set.
    pub fn apply_pull_response(&mut self, response: &FormsPullResponse) -> PullOutcome {
        for form in &response.forms {
            match self.forms.iter_mut().find(|stored| stored.id == form.id) {
                Some(stored) => *stored = form.clone(),
                None => self.forms.push(form.clone()),
            }
        }

        let before_tombstones = self.forms.len();
        self.forms
            .retain(|form| !response.deleted.contains(&form.id));
        self.cursor = response.cursor.clone();

        PullOutcome {
            updated: response.forms.len(),
            removed: before_tombstones - self.forms.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(forms: Vec<Form>, deleted: Vec<uuid::Uuid>, cursor: &str) -> FormsPullResponse {
        FormsPullResponse {
            forms,
            deleted,
            cursor: cursor.to_string(),
        }
    }

    #[test]
    fn test_pull_stores_forms_and_the_cursor() {
        let mut pulled = PulledForms::new();
        assert_eq!(pulled.cursor(), "");

        let outcome = pulled.apply_pull_response(&response(
            vec![Form::new("inspection"), Form::new("survey")],
            Vec::new(),
            "2026-08-31T10:00:00Z@2",
        ));

        assert_eq!(outcome.updated, 2);
        assert_eq!(outcome.removed, 0);
        assert_eq!(pulled.forms().len(), 2);
        assert_eq!(pulled.cursor(), "2026-08-31T10:00:00Z@2");
    }

    #[test]
    fn test_pull_replaces_a_form_it_already_holds() {
        let mut pulled = PulledForms::new();
        let mut form = Form::new("inspection");
        pulled.apply_pull_response(&response(vec![form.clone()], Vec::new(), "c1"));

        form.title = "inspection v2".to_string();
        form.version = 2;
        let outcome = pulled.apply_pull_response(&response(vec![form.clone()], Vec::new(), "c2"));

        assert_eq!(outcome.updated, 1);
        assert_eq!(pulled.forms().len(), 1);
        assert_eq!(pulled.forms()[0].title, "inspection v2");
        assert_eq!(pulled.forms()[0].version, 2);
        assert_eq!(pulled.cursor(), "c2");
    }

    #[test]
    fn test_a_tombstone_drops_the_form() {
        let mut pulled = PulledForms::new();
        let kept = Form::new("inspection");
        let removed = Form::new("survey");
        pulled.apply_pull_response(&response(
            vec![kept.clone(), removed.clone()],
            Vec::new(),
            "c1",
        ));

        let outcome = pulled.apply_pull_response(&response(Vec::new(), vec![removed.id], "c2"));

        assert_eq!(outcome.updated, 0);
        assert_eq!(outcome.removed, 1);
        assert_eq!(pulled.forms().len(), 1);
        assert_eq!(pulled.forms()[0].id, kept.id);
    }

    #[test]
    fn test_a_tombstone_for_a_form_never_held_changes_nothing() {
        let mut pulled = PulledForms::new();
        pulled.apply_pull_response(&response(vec![Form::new("inspection")], Vec::new(), "c1"));

        let outcome =
            pulled.apply_pull_response(&response(Vec::new(), vec![uuid::Uuid::new_v4()], "c2"));

        assert_eq!(outcome.removed, 0);
        assert_eq!(pulled.forms().len(), 1);
        assert_eq!(pulled.cursor(), "c2");
    }

    #[test]
    fn test_pulled_forms_round_trip_through_json() {
        let mut pulled = PulledForms::new();
        let form = Form::new("inspection");
        pulled.apply_pull_response(&response(vec![form.clone()], Vec::new(), "c1"));

        let restored: PulledForms =
            serde_json::from_str(&serde_json::to_string(&pulled).unwrap()).unwrap();

        assert_eq!(restored.cursor(), "c1");
        assert_eq!(restored.forms()[0].id, form.id);
    }
}
