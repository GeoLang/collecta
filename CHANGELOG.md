# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
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
  applied; `Store::list_forms_since` takes an optional creator filter.

### Security
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
  - Access is instance-wide per role: there is no per-form grant, so sharing one
    form's submissions with a second account is not expressible yet.
