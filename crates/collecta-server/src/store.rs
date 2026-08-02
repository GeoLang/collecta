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

/// An attachment together with the form its submission belongs to, so a read
/// can be authorized against that form without a second lookup.
pub struct StoredAttachment {
    pub attachment: AttachmentRow,
    pub form_id: Uuid,
}

/// One account's read grant on a form.
#[derive(Debug, Clone)]
pub struct FormGrant {
    pub user_id: Uuid,
    pub email: String,
    pub granted_at: String,
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

/// Who a form belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormOwner {
    /// Written before creators were recorded, so there is nobody to match a
    /// caller against.
    Legacy,
    User(Uuid),
}

/// Who is writing a form.
///
/// `id` becomes the form's `creator_id` on insert. On an overwrite the write is
/// refused unless `id` matches the stored creator or `overwrite_any` is set.
#[derive(Debug, Clone, Copy)]
pub struct FormWriter {
    pub id: Option<Uuid>,
    pub overwrite_any: bool,
}

impl FormWriter {
    /// A writer with no user identity and no ownership restriction, for
    /// fixtures and CLI seeding. Forms it writes are [`FormOwner::Legacy`].
    pub fn system() -> Self {
        Self {
            id: None,
            overwrite_any: true,
        }
    }
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
                updated_at TEXT NOT NULL DEFAULT '',
                creator_id TEXT
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
            "CREATE TABLE IF NOT EXISTS form_grants (
                form_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                granted_at TEXT NOT NULL,
                PRIMARY KEY (form_id, user_id)
            )",
        ] {
            sqlx::query(ddl).execute(&self.pool).await?;
        }
        self.migrate_forms_updated_at().await?;
        self.migrate_forms_creator_id().await?;
        self.migrate_forms_deleted_at().await?;
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

    // forms written before ownership existed keep a null creator_id. They are
    // not backfilled to anyone: an unowned form is admin-only, which is the
    // closed reading, and guessing an owner would hand someone else's data out.
    async fn migrate_forms_creator_id(&self) -> Result<(), sqlx::Error> {
        self.add_column("ALTER TABLE forms ADD COLUMN creator_id TEXT")
            .await
    }

    // a deleted form keeps its row as the tombstone the forms pull hands to
    // clients: hard-deleting it would leave a client that already pulled the
    // form with no way to learn it is gone.
    async fn migrate_forms_deleted_at(&self) -> Result<(), sqlx::Error> {
        self.add_column("ALTER TABLE forms ADD COLUMN deleted_at TEXT")
            .await
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

    /// Insert a form, or overwrite one the writer is allowed to replace.
    /// Returns false when the id is already taken by someone else's form.
    ///
    /// Posting a form whose id exists is an update, so the ownership test is
    /// part of the upsert rather than a separate read: a form created between a
    /// check and a write cannot be overwritten. `creator_id` is set on insert
    /// and never reassigned, so an admin editing a form does not take it over.
    ///
    /// Writing an id that was deleted clears the tombstone, so a form can be
    /// recreated under its old id instead of the id staying dead forever. The
    /// ownership test still applies, so only its creator or an admin can.
    pub async fn insert_form(&self, form: &Form, writer: FormWriter) -> Result<bool, sqlx::Error> {
        let writer_id = writer.id.map(|id| id.to_string());
        let result = sqlx::query(
            "INSERT INTO forms (id, title, version, data, updated_at, creator_id)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title,
                 version = excluded.version,
                 data = excluded.data,
                 updated_at = excluded.updated_at,
                 deleted_at = NULL
             WHERE ? OR forms.creator_id = ?",
        )
        .bind(form.id.to_string())
        .bind(&form.title)
        .bind(form.version as i64)
        .bind(encode_json(form))
        .bind(timestamp_now())
        .bind(&writer_id)
        .bind(writer.overwrite_any)
        .bind(&writer_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The owner of a form, or `None` when there is no such form or it was
    /// deleted. Every route that names a form goes through this, so a tombstone
    /// reads as a missing form everywhere.
    pub async fn form_owner(&self, form_id: Uuid) -> Result<Option<FormOwner>, sqlx::Error> {
        let row = sqlx::query("SELECT creator_id FROM forms WHERE id = ? AND deleted_at IS NULL")
            .bind(form_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| {
            let creator: Option<String> = row.get("creator_id");
            match creator.and_then(|id| id.parse().ok()) {
                Some(id) => FormOwner::User(id),
                None => FormOwner::Legacy,
            }
        }))
    }

    pub async fn list_forms(&self) -> Result<Vec<Form>, sqlx::Error> {
        let rows = sqlx::query("SELECT data FROM forms WHERE deleted_at IS NULL ORDER BY rowid")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_json).collect())
    }

    pub async fn get_form(&self, id: Uuid) -> Result<Option<Form>, sqlx::Error> {
        let row = sqlx::query("SELECT data FROM forms WHERE id = ? AND deleted_at IS NULL")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_json))
    }

    /// Forms changed after `since`, oldest first: the definitions still live,
    /// the ids of the ones deleted, and the cursor for the next pull. An empty
    /// `since` returns everything.
    ///
    /// Deletes ride the same cursor as edits because a tombstone is the form's
    /// own row with `deleted_at` set, so a client that pulls in order cannot
    /// receive a form after the delete that removed it.
    ///
    /// The cursor is `<rfc3339>@<rowid>` and compares as a pair: on the
    /// timestamp alone, a form written in the same microsecond as the one the
    /// cursor points at would be skipped forever. A bare timestamp from an
    /// older client reads as rowid 0, which re-delivers that microsecond
    /// instead of skipping it.
    pub async fn list_forms_since(
        &self,
        since: &str,
    ) -> Result<(Vec<Form>, Vec<Uuid>, Option<String>), sqlx::Error> {
        let (updated_at, rowid) = parse_cursor(since);
        let rows = sqlx::query(
            "SELECT id, data, deleted_at, updated_at, rowid FROM forms
             WHERE updated_at > ? OR (updated_at = ? AND rowid > ?)
             ORDER BY updated_at, rowid",
        )
        .bind(updated_at)
        .bind(updated_at)
        .bind(rowid)
        .fetch_all(&self.pool)
        .await?;
        let cursor = rows.last().map(format_cursor);

        let mut forms = Vec::new();
        let mut deleted = Vec::new();
        for row in &rows {
            if row.get::<Option<String>, _>("deleted_at").is_some() {
                deleted.push(row_uuid(row, "id"));
            } else {
                forms.push(row_json(row));
            }
        }
        Ok((forms, deleted, cursor))
    }

    /// Tombstone a form and remove the data collected under it, returning the
    /// ids of the deleted submissions so their attachment files can go too.
    ///
    /// The form row survives as the tombstone. Everything below it is a hard
    /// delete, since submissions only ever travel client to server and nothing
    /// pulls them back.
    pub async fn delete_form(&self, form_id: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
        let form_id = form_id.to_string();
        let mut tx = self.pool.begin().await?;

        let rows = sqlx::query("SELECT id FROM submissions WHERE form_id = ?")
            .bind(&form_id)
            .fetch_all(&mut *tx)
            .await?;
        let submissions: Vec<Uuid> = rows.iter().map(|row| row_uuid(row, "id")).collect();

        for statement in [
            "DELETE FROM attachments WHERE submission_id IN
                (SELECT id FROM submissions WHERE form_id = ?)",
            "DELETE FROM sync_queue WHERE submission_id IN
                (SELECT id FROM submissions WHERE form_id = ?)",
            "DELETE FROM submissions WHERE form_id = ?",
            "DELETE FROM form_grants WHERE form_id = ?",
        ] {
            sqlx::query(statement)
                .bind(&form_id)
                .execute(&mut *tx)
                .await?;
        }

        // the bumped updated_at is what carries the tombstone to a pulling
        // client, so it has to move even though nothing in the form changed.
        sqlx::query("UPDATE forms SET deleted_at = ?, updated_at = ? WHERE id = ?")
            .bind(timestamp_now())
            .bind(timestamp_now())
            .bind(&form_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(submissions)
    }

    /// Delete one submission of a form. Returns false when that form holds no
    /// such submission, which is also what a mismatched pair gives.
    pub async fn delete_submission(
        &self,
        form_id: Uuid,
        submission_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let submission_id = submission_id.to_string();
        let mut tx = self.pool.begin().await?;
        // the form is part of the match, so holding one form cannot be used to
        // delete a submission filed under another.
        let deleted = sqlx::query("DELETE FROM submissions WHERE id = ? AND form_id = ?")
            .bind(&submission_id)
            .bind(form_id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        if !deleted {
            return Ok(false);
        }
        for statement in [
            "DELETE FROM attachments WHERE submission_id = ?",
            "DELETE FROM sync_queue WHERE submission_id = ?",
        ] {
            sqlx::query(statement)
                .bind(&submission_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Give an account read access to one form. Re-granting is a no-op.
    pub async fn grant_form(&self, form_id: Uuid, user_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR REPLACE INTO form_grants (form_id, user_id, granted_at) VALUES (?, ?, ?)",
        )
        .bind(form_id.to_string())
        .bind(user_id.to_string())
        .bind(timestamp_now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Withdraw a grant, reporting whether there was one.
    pub async fn revoke_form(&self, form_id: Uuid, user_id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM form_grants WHERE form_id = ? AND user_id = ?")
            .bind(form_id.to_string())
            .bind(user_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether `user_id` holds a grant on `form_id`.
    pub async fn has_grant(&self, form_id: Uuid, user_id: Uuid) -> Result<bool, sqlx::Error> {
        let row = sqlx::query("SELECT 1 FROM form_grants WHERE form_id = ? AND user_id = ?")
            .bind(form_id.to_string())
            .bind(user_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// Grants on a form, oldest first, with the account each one names.
    pub async fn list_grants(&self, form_id: Uuid) -> Result<Vec<FormGrant>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT g.user_id, u.email, g.granted_at
             FROM form_grants g JOIN users u ON u.id = g.user_id
             WHERE g.form_id = ? ORDER BY g.granted_at, g.rowid",
        )
        .bind(form_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| FormGrant {
                user_id: row_uuid(row, "user_id"),
                email: row.get("email"),
                granted_at: row.get("granted_at"),
            })
            .collect())
    }

    pub async fn user_exists(&self, user_id: Uuid) -> Result<bool, sqlx::Error> {
        let row = sqlx::query("SELECT 1 FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// Persist a submission and enqueue it for sync. Returns false when that id
    /// is already taken.
    ///
    /// A plain insert rather than a replace, and the constraint decides rather
    /// than a prior read: `id` comes from the client and is unique across every
    /// form, so replacing would let one account overwrite another's submission
    /// and carry its attachments under a form the writer controls.
    pub async fn insert_submission(&self, submission: &Submission) -> Result<bool, sqlx::Error> {
        let insert = sqlx::query("INSERT INTO submissions (id, form_id, data) VALUES (?, ?, ?)")
            .bind(submission.id.to_string())
            .bind(submission.form_id.to_string())
            .bind(encode_json(submission))
            .execute(&self.pool)
            .await;
        match insert {
            Ok(_) => {
                self.enqueue(submission).await?;
                Ok(true)
            }
            Err(e) if is_unique_violation(&e) => Ok(false),
            Err(e) => Err(e),
        }
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

    /// File names already attached to a submission, for skipping parts ODK
    /// resends when it splits one submission across several posts.
    pub async fn attached_filenames(
        &self,
        submission_id: Uuid,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query("SELECT filename FROM attachments WHERE submission_id = ?")
            .bind(submission_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|row| row.get("filename")).collect())
    }

    /// Attachments recorded for a submission, oldest first.
    pub async fn list_attachments(
        &self,
        submission_id: Uuid,
    ) -> Result<Vec<AttachmentRow>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, submission_id, field_name, filename, content_type, size_bytes, storage_path
             FROM attachments WHERE submission_id = ? ORDER BY rowid",
        )
        .bind(submission_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(attachment_row).collect())
    }

    /// One attachment by id, with the form it hangs off so a reader can be
    /// authorized against that form.
    pub async fn find_attachment(&self, id: Uuid) -> Result<Option<StoredAttachment>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT a.id, a.submission_id, a.field_name, a.filename, a.content_type,
                    a.size_bytes, a.storage_path, s.form_id
             FROM attachments a JOIN submissions s ON s.id = a.submission_id
             WHERE a.id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| StoredAttachment {
            attachment: attachment_row(&row),
            form_id: row_uuid(&row, "form_id"),
        }))
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

fn row_uuid(row: &sqlx::sqlite::SqliteRow, column: &str) -> Uuid {
    let value: String = row.get(column);
    value.parse().expect("stored id is a uuid")
}

fn attachment_row(row: &sqlx::sqlite::SqliteRow) -> AttachmentRow {
    AttachmentRow {
        id: row_uuid(row, "id"),
        submission_id: row_uuid(row, "submission_id"),
        field_name: row.get("field_name"),
        filename: row.get("filename"),
        content_type: row.get("content_type"),
        size_bytes: row.get::<i64, _>("size_bytes") as u64,
        storage_path: row.get("storage_path"),
    }
}
