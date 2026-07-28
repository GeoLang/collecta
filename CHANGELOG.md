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
