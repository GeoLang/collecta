//! sqlite-backed persistence for forms, submissions, and the sync queue.
//!
//! Records are stored as their canonical json plus a few indexed columns.
//! Pass `:memory:` as the path for an ephemeral database (tests); anything else
//! is a file created on first open.

use collecta_core::form::Form;
use collecta_core::submission::{AttachmentRef, Submission};
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

/// A stored attachment. `storage_path` is server-generated; no part of it comes
/// from the client (see [`crate::openrosa`]).
#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub id: Uuid,
    pub submission_id: Uuid,
    pub field_name: String,
    /// Client-supplied name, kept as metadata only.
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub storage_path: String,
}

/// A submission already filed under a `meta/instanceID`.
pub struct StoredInstance {
    pub submission: Submission,
    /// Hash of the instance xml that created it, for detecting a collision
    /// rather than a genuine resubmission.
    pub instance_hash: String,
}

/// Result of claiming a `meta/instanceID` for a form.
pub enum InstanceInsert {
    /// The instance was new; this is its submission id.
    Created(Uuid),
    /// That instance id was already taken, by this submission. Boxed to keep
    /// the enum small on the common path.
    Existing(Box<StoredInstance>),
}

/// A stored user. Internal to the server: carries the password hash and is
/// never serialized into responses.
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: Uuid,
    pub email: String,
    pub password_hash: String,
    pub role: String,
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
                data TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT ''
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
            "CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                email TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                role TEXT NOT NULL,
                created_at TEXT NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS attachments (
                id TEXT PRIMARY KEY,
                submission_id TEXT NOT NULL,
                field_name TEXT NOT NULL,
                filename TEXT NOT NULL,
                content_type TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                storage_path TEXT NOT NULL,
                created_at TEXT NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS attachments_submission
                ON attachments (submission_id)",
        ] {
            sqlx::query(ddl).execute(&self.pool).await?;
        }
        self.migrate_forms_updated_at().await?;
        self.migrate_submissions_instance_id().await
    }

    // sqlite has no `ADD COLUMN IF NOT EXISTS`: run it and swallow only the
    // duplicate-column error, so a fresh and an upgraded database converge.
    async fn add_column(&self, ddl: &'static str) -> Result<(), sqlx::Error> {
        match sqlx::query(ddl).execute(&self.pool).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let duplicate = e
                    .as_database_error()
                    .is_some_and(|db| db.message().contains("duplicate column name"));
                if duplicate { Ok(()) } else { Err(e) }
            }
        }
    }

    // databases created before the sync protocol lack forms.updated_at:
    // add it and backfill so existing forms are visible to a fresh cursor.
    async fn migrate_forms_updated_at(&self) -> Result<(), sqlx::Error> {
        self.add_column("ALTER TABLE forms ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''")
            .await?;
        sqlx::query("UPDATE forms SET updated_at = ? WHERE updated_at = ''")
            .bind(timestamp_now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // openrosa submissions carry a client-generated meta/instanceID used as the
    // idempotency key. The index is partial so rows from the json api, which
    // have no instance id, are not forced into a single '' collision.
    async fn migrate_submissions_instance_id(&self) -> Result<(), sqlx::Error> {
        self.add_column("ALTER TABLE submissions ADD COLUMN instance_id TEXT NOT NULL DEFAULT ''")
            .await?;
        self.add_column(
            "ALTER TABLE submissions ADD COLUMN instance_hash TEXT NOT NULL DEFAULT ''",
        )
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS submissions_form_instance
             ON submissions (form_id, instance_id) WHERE instance_id != ''",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_form(&self, form: &Form) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR REPLACE INTO forms (id, title, version, data, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(form.id.to_string())
        .bind(&form.title)
        .bind(form.version as i64)
        .bind(encode_json(form))
        .bind(timestamp_now())
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

    /// Forms updated after `since`, oldest first, plus the cursor for the next
    /// pull. An empty `since` returns everything.
    ///
    /// The cursor is `<rfc3339>@<rowid>` and compares as a pair: on the
    /// timestamp alone, a form written in the same microsecond as the one the
    /// cursor points at would be skipped forever. A bare timestamp from an
    /// older client reads as rowid 0, which re-delivers that microsecond
    /// instead of skipping it.
    pub async fn list_forms_since(
        &self,
        since: &str,
    ) -> Result<(Vec<Form>, Option<String>), sqlx::Error> {
        let (updated_at, rowid) = parse_cursor(since);
        let rows = sqlx::query(
            "SELECT data, updated_at, rowid FROM forms
             WHERE updated_at > ? OR (updated_at = ? AND rowid > ?)
             ORDER BY updated_at, rowid",
        )
        .bind(updated_at)
        .bind(updated_at)
        .bind(rowid)
        .fetch_all(&self.pool)
        .await?;
        let cursor = rows.last().map(format_cursor);
        Ok((rows.iter().map(row_json).collect(), cursor))
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

    /// Insert a submission only if its id is new; returns whether it was
    /// inserted. Duplicates are left untouched (first write wins), making
    /// sync push idempotent.
    pub async fn insert_submission_if_new(
        &self,
        submission: &Submission,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("INSERT OR IGNORE INTO submissions (id, form_id, data) VALUES (?, ?, ?)")
                .bind(submission.id.to_string())
                .bind(submission.form_id.to_string())
                .bind(encode_json(submission))
                .execute(&self.pool)
                .await?;
        let inserted = result.rows_affected() > 0;
        if inserted {
            self.enqueue(submission).await?;
        }
        Ok(inserted)
    }

    /// Persist an openrosa submission under its `meta/instanceID`, or report the
    /// submission that already claims that id for this form.
    ///
    /// The insert races the unique index rather than checking first, so two
    /// concurrent posts of the same instance cannot both be accepted.
    pub async fn insert_instance(
        &self,
        submission: &Submission,
        instance_id: &str,
        instance_hash: &str,
    ) -> Result<InstanceInsert, sqlx::Error> {
        let insert = sqlx::query(
            "INSERT INTO submissions (id, form_id, data, instance_id, instance_hash)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(submission.id.to_string())
        .bind(submission.form_id.to_string())
        .bind(encode_json(submission))
        .bind(instance_id)
        .bind(instance_hash)
        .execute(&self.pool)
        .await;

        match insert {
            Ok(_) => {
                self.enqueue(submission).await?;
                Ok(InstanceInsert::Created(submission.id))
            }
            Err(e) if is_unique_violation(&e) => {
                let existing = self
                    .find_instance(submission.form_id, instance_id)
                    .await?
                    .ok_or(e)?;
                Ok(InstanceInsert::Existing(Box::new(existing)))
            }
            Err(e) => Err(e),
        }
    }

    /// The submission holding `instance_id` for `form_id`, if any.
    pub async fn find_instance(
        &self,
        form_id: Uuid,
        instance_id: &str,
    ) -> Result<Option<StoredInstance>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT data, instance_hash FROM submissions
             WHERE form_id = ? AND instance_id = ?",
        )
        .bind(form_id.to_string())
        .bind(instance_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| StoredInstance {
            submission: row_json(&row),
            instance_hash: row.get("instance_hash"),
        }))
    }

    /// Record an attachment and mirror it onto the submission's own list.
    ///
    /// Read-modify-write of the submission json runs in a transaction so two
    /// attachments landing at once cannot drop one another.
    pub async fn add_attachment(&self, attachment: &AttachmentRow) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO attachments
             (id, submission_id, field_name, filename, content_type, size_bytes, storage_path, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(attachment.id.to_string())
        .bind(attachment.submission_id.to_string())
        .bind(&attachment.field_name)
        .bind(&attachment.filename)
        .bind(&attachment.content_type)
        .bind(attachment.size_bytes as i64)
        .bind(&attachment.storage_path)
        .bind(timestamp_now())
        .execute(&mut *tx)
        .await?;

        let row = sqlx::query("SELECT data FROM submissions WHERE id = ?")
            .bind(attachment.submission_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(row) = row {
            let mut submission: Submission = row_json(&row);
            submission.attachments.push(AttachmentRef {
                id: attachment.id,
                field_name: attachment.field_name.clone(),
                filename: attachment.filename.clone(),
                mime_type: attachment.content_type.clone(),
                size_bytes: attachment.size_bytes,
            });
            sqlx::query("UPDATE submissions SET data = ? WHERE id = ?")
                .bind(encode_json(&submission))
                .bind(attachment.submission_id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Field names already attached to a submission, for skipping parts ODK
    /// resends when it splits one submission across several posts.
    pub async fn attached_field_names(
        &self,
        submission_id: Uuid,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query("SELECT field_name FROM attachments WHERE submission_id = ?")
            .bind(submission_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|row| row.get("field_name")).collect())
    }

    pub async fn create_user(&self, user: &UserRecord) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, role, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(user.id.to_string())
        .bind(&user.email)
        .bind(&user.password_hash)
        .bind(&user.role)
        .bind(timestamp_now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<UserRecord>, sqlx::Error> {
        let row = sqlx::query("SELECT id, email, password_hash, role FROM users WHERE email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| {
            let id: String = row.get("id");
            UserRecord {
                id: id.parse().expect("stored user id is a uuid"),
                email: row.get("email"),
                password_hash: row.get("password_hash"),
                role: row.get("role"),
            }
        }))
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

// fixed-width rfc3339 (microseconds, utc) so lexicographic order matches
// chronological order for the forms cursor.
fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

// anything without a parsable `@<rowid>` suffix is a bare timestamp.
fn parse_cursor(since: &str) -> (&str, i64) {
    since
        .rsplit_once('@')
        .and_then(|(updated_at, rowid)| rowid.parse().ok().map(|rowid| (updated_at, rowid)))
        .unwrap_or((since, 0))
}

fn format_cursor(row: &sqlx::sqlite::SqliteRow) -> String {
    let updated_at: String = row.get("updated_at");
    let rowid: i64 = row.get("rowid");
    format!("{updated_at}@{rowid}")
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|db| db.is_unique_violation())
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
