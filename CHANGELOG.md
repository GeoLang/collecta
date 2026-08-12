# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Attachment download (2026-08-02): `GET /api/v1/attachments/{id}` serves the
  stored bytes under their recorded content type, to whoever may read the
  submission they hang off. Always `Content-Disposition: attachment` with
  `X-Content-Type-Options: nosniff`, since the type came off a field device.
- Deletes (2026-08-02): `DELETE /api/v1/forms/{id}` and
  `DELETE /api/v1/forms/{id}/submissions/{sid}`, both owner or admin. Deleting a
  form removes its submissions, their queue entries, their attachment rows and
  files, and the grants on it, leaving the form row as a tombstone. Deleting a
  submission is a hard delete.
- Form tombstones in the sync protocol (2026-08-02): `FormsPullResponse` gained
  `deleted`, the ids removed since the cursor. Tombstones are the deleted form's
  own row, so they ride the existing cursor and cannot arrive out of order
  against an edit. The field defaults to empty when absent, so a payload from an
  older server still parses.
- Per-form grants (2026-08-02): a `form_grants` table plus
  `GET`/`POST /api/v1/forms/{id}/grants` and
  `DELETE /api/v1/forms/{id}/grants/{user_id}`, owner or admin only. A grant is
  read-only and covers one form's submissions and their attachments. It does not
  let the grantee delete anything, re-share the form, or see who else holds a
  grant.
- OpenRosa compatibility layer so ODK Collect can use collecta as its server:
  `GET /formList`, `GET /forms/{id}/form.xml`, and `HEAD`/`POST /submission`,
  authenticated with HTTP Basic against the existing users table.
- XForm generation from the stored form model, putting the preserved
  relevant/constraint/calculation expressions into the binds for Collect to
  evaluate, with the XLSForm `${name}` shorthand rewritten to the referenced
  field's path the way pyxform does it (relative within a repeat, absolute
  otherwise) and `${name}` in labels and hints emitted as `<output>`. A
  reference that cannot be resolved fails rendering instead of emitting broken
  XPath that JavaRosa rejects.
- Submission ingest: instance XML parsed into typed field values, run through the
  existing validation engine, made idempotent on `meta/instanceID`.
- Attachment storage on disk under `COLLECTA_DATA_DIR`, indexed by a new
  `attachments` table, with per-part and per-request size limits.
- `COLLECTA_DATA_DIR` and `COLLECTA_BASE_URL` environment variables.

### Changed
- `router()` now takes a `Config` instead of a bare JWT secret. The `/api/v1`
  routes themselves are unchanged.
- `Store::insert_form` takes a `FormWriter` and returns whether the write was
  applied; `Store::list_forms_since` also returns the ids deleted since the
  cursor. `Store::insert_submission` returns whether the row was inserted, false
  meaning that id was already taken.

### Security
- The JSON API records who filed a submission (2026-08-11). `POST
  /api/v1/forms/{id}/submissions` and `POST /api/v1/sync/push` stored the
  `collector_id` from the request body, so submissions arrived with no submitter
  at all or with one the client picked, including another account's id. Both
  routes now overwrite it with the caller's token subject, the same identity the
  OpenRosa route already recorded. A batch push is filed under the pushing
  account, since that is the only identity behind the connection. Rows stored
  before this keep a null `collector_id` and are not backfilled: no submission
  path is open without credentials, so null means nobody recorded a submitter
  rather than that nobody submitted it.
- Submission ids are no longer claimable (2026-08-02). The JSON submit route
  wrote the client's submission with a replace on an id that is client-chosen
  and unique across every form, so an account that had seen a
  submission id could refile it under a form of its own. That moved the row, and
  the attachments hanging off it, into a form the writer controlled, which is
  the form every per-form check reads authority from. It made a read grant a
  permanent read plus a delete on the grantor's data, since the ids a grantee
  legitimately sees kept working after the grant was revoked. An id already on
  file is now a 409 rather than an overwrite, and the constraint decides so
  there is no window between the check and the write.
- The submission body can no longer name a form other than the one in the path
  (2026-08-02). Validation ran against the path's form while the row was filed
  under the body's, so a submission could land in someone else's form without
  ever being checked against its schema. A mismatch is now a 400.
- Storage errors are no longer echoed to callers (2026-08-02). The JSON API put
  the sqlx error text, which carries query and schema detail, in the 500 body
  and in per-item sync push messages. It now goes to the server log and the
  caller gets `storage error`, matching what the OpenRosa surface already did.
- An attachment a caller may not read answers 404 rather than 403 (2026-08-02),
  so holding a guessed id cannot confirm it exists.
- Role-based authorization on top of authentication (2026-08-01). Until now any
  valid token or Basic credential had full access to every form and every
  submission over both APIs.
  - Roles are `admin`, `editor` and `viewer`. A role string outside those three
    is refused rather than downgraded, over both the JWT and the OpenRosa
    surface, so an account nobody gave a valid role can log in and do nothing.
    `create-user` now rejects such a role instead of creating the account.
    Existing rows with any other role stop working and have to be updated.
  - Forms record a `creator_id`. Creating, importing and submitting need editor
    or admin; reading a form's submissions needs to be its creator or an admin.
    Reposting a form id that belongs to someone else is refused, and an admin
    overwriting a form does not take ownership of it.
  - Form discovery stays open to any authenticated account (`GET /api/v1/forms`,
    `GET /api/v1/forms/{id}`, `GET /api/v1/sync/forms`, `/formList`,
    `form.xml`): collectors need it, including for forms they did not create.
  - `GET /api/v1/sync/status`, whose counts cover the whole instance, is
    admin-only.
  - Forms created before this change have no creator. They are readable by
    admins only, since there is nobody to match a caller against.
  - Access is instance-wide per role. Per-form grants landed on 2026-08-02, see
    above.
