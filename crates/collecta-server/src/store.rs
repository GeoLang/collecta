//! sqlite-backed persistence for forms, submissions, and the sync queue.
//!
//! Records are stored as their canonical json plus a few indexed columns.
//! Pass `:memory:` as the path for an ephemeral database (tests); anything else
//! is a file created on first open.

use collecta_core::form::Form;
use collecta_core::submission::Submission;
use collecta_core::sync_queue::{QueueItem, SyncStatus};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

/// Handle to the persistent store. Cheap to clone (shares the pool).
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// Sync-queue counts by status.
#[derive(Default)]
pub struct SyncCounts {
    pub pending: usize,
    pub synced: usize,
    pub failed: usize,
    pub abandoned: usize,
    pub total: usize,
}

impl Store {
    /// Open (creating if needed) the database at `db_path`.
    pub async fn connect(db_path: &str) -> Result<Self, sqlx::Error> {
        let in_memory = db_path == ":memory:";
        let options = if in_memory {
            SqliteConnectOptions::new().in_memory(true)
        } else {
            SqliteConnectOptions::new()
                .filename(db_path)
                .create_if_missing(true)
        };
        // one connection for in-memory so schema and data outlive a single query.
        let max_connections = if in_memory { 1 } else { 5 };
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;
        let store = Self { pool };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> Result<(), sqlx::Error> {
        for ddl in [
            "CREATE TABLE IF NOT EXISTS forms (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                version INTEGER NOT NULL,
                data TEXT NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS submissions (
                id TEXT PRIMARY KEY,
                form_id TEXT NOT NULL,
                data TEXT NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS sync_queue (
                submission_id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                data TEXT NOT NULL
            )",
        ] {
            sqlx::query(ddl).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn insert_form(&self, form: &Form) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR REPLACE INTO forms (id, title, version, data) VALUES (?, ?, ?, ?)")
            .bind(form.id.to_string())
            .bind(&form.title)
            .bind(form.version as i64)
            .bind(encode_json(form))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_forms(&self) -> Result<Vec<Form>, sqlx::Error> {
        let rows = sqlx::query("SELECT data FROM forms ORDER BY rowid")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_json).collect())
    }

    pub async fn get_form(&self, id: Uuid) -> Result<Option<Form>, sqlx::Error> {
        let row = sqlx::query("SELECT data FROM forms WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_json))
    }

    /// Persist a submission and enqueue it for sync.
    pub async fn insert_submission(&self, submission: &Submission) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR REPLACE INTO submissions (id, form_id, data) VALUES (?, ?, ?)")
            .bind(submission.id.to_string())
            .bind(submission.form_id.to_string())
            .bind(encode_json(submission))
            .execute(&self.pool)
            .await?;
        self.enqueue(submission).await
    }

    // every received submission enters the sync queue as pending, mirroring the
    // offline-first client model and giving /sync/status persisted counts.
    async fn enqueue(&self, submission: &Submission) -> Result<(), sqlx::Error> {
        let item = QueueItem {
            submission: submission.clone(),
            status: SyncStatus::Pending,
            retry_count: 0,
            queued_at: chrono::Utc::now(),
            last_attempt: None,
            last_error: None,
        };
        sqlx::query(
            "INSERT OR REPLACE INTO sync_queue (submission_id, status, data) VALUES (?, ?, ?)",
        )
        .bind(submission.id.to_string())
        .bind(status_label(item.status))
        .bind(encode_json(&item))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_submissions(&self, form_id: Uuid) -> Result<Vec<Submission>, sqlx::Error> {
        let rows = sqlx::query("SELECT data FROM submissions WHERE form_id = ? ORDER BY rowid")
            .bind(form_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_json).collect())
    }

    pub async fn sync_counts(&self) -> Result<SyncCounts, sqlx::Error> {
        let rows = sqlx::query("SELECT status, COUNT(*) AS n FROM sync_queue GROUP BY status")
            .fetch_all(&self.pool)
            .await?;
        let mut counts = SyncCounts::default();
        for row in &rows {
            let status: String = row.get("status");
            let n = row.get::<i64, _>("n") as usize;
            counts.total += n;
            match status.as_str() {
                "Pending" => counts.pending = n,
                "Synced" => counts.synced = n,
                "Failed" => counts.failed = n,
                "Abandoned" => counts.abandoned = n,
                _ => {}
            }
        }
        Ok(counts)
    }
}

fn status_label(status: SyncStatus) -> &'static str {
    match status {
        SyncStatus::Pending => "Pending",
        SyncStatus::InProgress => "InProgress",
        SyncStatus::Synced => "Synced",
        SyncStatus::Failed => "Failed",
        SyncStatus::Abandoned => "Abandoned",
    }
}

fn encode_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("model serializes to json")
}

fn row_json<T: DeserializeOwned>(row: &sqlx::sqlite::SqliteRow) -> T {
    let data: String = row.get("data");
    serde_json::from_str(&data).expect("stored json is valid")
}
