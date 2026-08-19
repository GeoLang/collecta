# Collecta

[![CI](https://github.com/GeoLang/collecta/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/collecta/actions)

**Schema-driven field data collection**: form schemas, validation, JWT auth, and a sync protocol for the GeoLang ecosystem.

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024_edition-orange.svg)](https://www.rust-lang.org/)

> Part of the [GeoLang](https://github.com/GeoLang) geospatial platform.

---

## Overview

Collecta aims to be an open-source alternative to ArcGIS Field Maps, KoboToolbox, and
ODK Collect. Today it is the server and the shared schema library, not the field app.
It provides:

- **Form schemas** with typed fields, conditional logic, and validation constraints
- **GPS capture types** (point, trace, shape) in the form model
- **REST API** for form management and submission ingestion, persisted to SQLite
- **JWT auth** with argon2id password hashing, admin-seeded users, and three roles
- **Sync protocol**: idempotent submission push and cursor-based form pull
- **XLSForm import** (`.xlsx` survey/choices/settings) into the form model
- **Offline sync queue** as a library type in `collecta-core`, for a client to build on

### Status

**Working:** the Axum server, SQLite persistence, JWT auth with role, form-ownership and
per-form grant checks, form CRUD including deletes, submission validation and ingestion,
attachment download, XLSForm import, the push/pull sync endpoints, and an OpenRosa
compatibility layer that ODK Collect can submit to. All are covered by tests
(`cargo test` runs 117).

**Not built yet:**

- **No first-party client application.** There is no mobile app, desktop app, or FFI
  layer of our own; ODK Collect is the supported client. The `SyncQueue` and
  `AttachmentStore` types in `collecta-core` are there for a client author to use, and
  nothing in this repo uses them.
- **No attachment sync.** Attachments can be uploaded over OpenRosa and downloaded over
  the JSON API, but they are not part of the push/pull protocol.
- **No Ptolemy integration.** Writing collected features through to the geodatabase is
  planned, not wired up.

---

## Architecture

```
┌────────────────────────────────────────────────────────┐
│  Mobile App (TerraVista + Collecta FFI)     [PLANNED]  │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────────┐ │
│  │  Form    │  │  Offline │  │    Attachment         │ │
│  │  Render  │  │  Queue   │  │    Store (photos,     │ │
│  │  Engine  │  │  & Sync  │  │    audio, signatures) │ │
│  └──────────┘  └──────────┘  └──────────────────────┘ │
├────────────────────────────────────────────────────────┤
│  collecta-core (Rust library)                          │
│  ┌────────┐ ┌────────────┐ ┌──────────┐ ┌──────────┐ │
│  │ Form   │ │ Submission │ │Validation│ │  Sync    │ │
│  │ Schema │ │  & Values  │ │  Engine  │ │  Queue   │ │
│  └────────┘ └────────────┘ └──────────┘ └──────────┘ │
├────────────────────────────────────────────────────────┤
│  collecta-server (Axum REST API)                       │
│  Form CRUD · Submission ingestion · Sync endpoints     │
├────────────────────────────────────────────────────────┤
│  ptolemy (geodatabase) — spatial storage    [PLANNED]  │
└────────────────────────────────────────────────────────┘
```

The top and bottom layers are the target design. Only the two middle layers,
`collecta-core` and `collecta-server`, exist in this repo.

---

## Features

### Form Schema

- **20+ field types**: Text, Integer, Decimal, Date, DateTime, Time, Select, MultiSelect, GeoPoint, GeoTrace, GeoShape, Photo, Audio, Video, File, Barcode, Signature, Boolean, Repeat, Note
- **Validation constraints**: Min/Max value, Min/Max length, glob-style pattern, OneOf
- **Conditional visibility**: only over the XLSForm path, where the raw `relevant`
  expression is carried as metadata into the XForm bind and evaluated on the device by
  ODK Collect. The form model's own `Condition` type is not read by anything.
- **Repeat groups**: Nested sub-forms for multiple entries (e.g., "list all items
  inspected"). They round-trip through the model, the XForm renderer and the submission
  parser, but validation does not descend into them.
- **Help text**: Per-field hints for data collectors

### Offline Sync Queue

A `collecta-core` library type for a client to drive. No client in this repo uses it.

- **Queue submissions locally** so collection does not need connectivity
- **Exponential backoff retry**: 5s → 10s → 20s → 40s → ... capped at 5min
- **Max retries** with permanent failure status after threshold
- **Status tracking**: Pending, Synced, Failed, Abandoned
- Submissions only. Attachment sync is not implemented on either side.

### Validation Engine

- Required field enforcement
- Numeric range validation (min/max)
- Text length validation
- Pattern matching (glob-style)
- OneOf constraint (value must be from allowed set)
- Unknown field detection
- Full error reporting (all errors returned, not just first)

All of it applies to top-level fields only. Validation does not recurse into a repeat's
children, so a required field inside a repeat is not enforced. Defaults are not applied
either: a field's `default` is imported and stored, but nothing substitutes it on ingest
and the XForm renderer emits empty instance nodes, so it never reaches Collect.

### REST API

| Method | Endpoint | Description | Who |
|--------|----------|-------------|-----|
| GET | `/health` | Health check (public) | anyone |
| POST | `/api/v1/auth/login` | Exchange email/password for a JWT (public) | anyone |
| GET | `/api/v1/forms` | List all forms | any account |
| POST | `/api/v1/forms` | Create a form (JSON) | editor, admin |
| POST | `/api/v1/forms/import` | Import an XLSForm (`.xlsx` request body) | editor, admin |
| GET | `/api/v1/forms/{id}` | Get form schema | any account |
| DELETE | `/api/v1/forms/{id}` | Delete a form and everything collected under it | form creator, admin |
| GET | `/api/v1/forms/{id}/submissions` | List submissions | creator, grantee, admin |
| POST | `/api/v1/forms/{id}/submissions` | Submit data (validates against schema) | editor, admin |
| DELETE | `/api/v1/forms/{id}/submissions/{sid}` | Delete one submission and its attachments | form creator, admin |
| GET | `/api/v1/forms/{id}/grants` | List the accounts this form is shared with | form creator, admin |
| POST | `/api/v1/forms/{id}/grants` | Share the form's data (`{"user_id": "<uuid>"}`) | form creator, admin |
| DELETE | `/api/v1/forms/{id}/grants/{user_id}` | Withdraw a grant | form creator, admin |
| GET | `/api/v1/attachments/{id}` | Download an attachment's bytes | creator, grantee, admin |
| GET | `/api/v1/sync/status` | Count of submissions received (whole instance) | admin |
| POST | `/api/v1/sync/push` | Batch-upload queued submissions (idempotent) | editor, admin |
| GET | `/api/v1/sync/forms?since=<cursor>` | Form definitions changed since cursor | any account |

All endpoints except `/health` and login require `Authorization: Bearer <jwt>`. See
[Authentication and roles](#authentication-and-roles).

A submission carries its own id and names its form, both chosen by the client. The form in
the path wins: a body naming a different one is a 400 rather than a silent correction. Ids
are unique across every form, so posting one already on file is a 409, never an overwrite.
Deleting a submission is a hard delete, so its id is free again afterwards.

The submitter is not client-chosen. Both `POST /api/v1/forms/{id}/submissions` and
`POST /api/v1/sync/push` overwrite `collector_id` with the account the token names, so a
body claiming someone else is ignored, and a pushed batch is filed under the account that
pushed it. Submissions stored before this was recorded have `collector_id: null` and are
not backfilled to anyone.

Attachment ids come from the `attachments` list on each submission. The bytes come back
under a content type collecta chose, not the one the device claimed: a capture format
(`image/jpeg`, `image/png`, `image/webp`, `image/heic`, `audio/3gpp`, `audio/aac`,
`audio/mp4`, `audio/mpeg`, `audio/ogg`, `audio/wav`, `video/3gpp`, `video/mp4`,
`video/quicktime`, `video/webm`, `application/pdf`) is kept, and everything else, markup
and scripts included, is recorded and served as `application/octet-stream`. The bytes
themselves are stored whole either way. On top of that every response is
`Content-Disposition: attachment` with `X-Content-Type-Options: nosniff`, so an upload
can never be rendered as a page on this origin. An id the
caller may not read answers 404 rather than 403, since nothing lists attachments to an
account without read on their form and the id is the only thing protecting the bytes.

Deleting a form leaves a tombstone the form pull hands to clients. Its submissions, their
attachments (rows and files) and the grants on it are removed outright. Deleting a
submission is a hard delete, since submissions only ever travel client to server. Writing
a form under a deleted id clears the tombstone and brings the id back.

---

## OpenRosa (ODK Collect)

Collecta serves the OpenRosa APIs at the server root, so ODK Collect can use it as
its server. Point Collect at the server URL and give it an account created with
`create-user`.

| Method | Endpoint | Description | Who |
|--------|----------|-------------|-----|
| GET | `/formList` | Form list XML, one entry per renderable form | any account |
| GET | `/forms/{id}/form.xml` | The form rendered as an ODK XForm | any account |
| HEAD | `/submission` | Auth and capability preflight (204) | any account |
| POST | `/submission` | `multipart/form-data` submission plus attachments | editor, admin |

These routes use **HTTP Basic** against the same users table as the JSON API, since
Collect has no concept of a bearer token. Give a collector the `editor` role: a `viewer`
can download forms but gets a 403 on submit. A request without credentials gets a 401
with `WWW-Authenticate: Basic realm="collecta"`, which is the challenge Collect waits
for. **Run this behind TLS**: the OpenRosa auth spec forbids Basic over plain HTTP,
and nothing here can enforce that for you. Every response carries
`X-OpenRosa-Version: 1.0`; bodies are `OpenRosaResponse` envelopes.

Forms are generated from the stored form model. Field types map to XForm binds
(`text`→`xsd:string`, `integer`→`xsd:int`, `geopoint`/`geotrace`/`geoshape`,
`photo`/`audio`/`video`/`file`/`signature`→`binary` uploads, `select_one`→`select1`
with inline choices, repeats→repeat groups). The `relevant`, `constraint`, and
`calculation` expressions the XLSForm importer preserved go into the binds, and
**Collect evaluates them on the device**. Collecta still does not evaluate them
server-side, so what the server enforces on ingest is only what its own validation
engine models.

The one thing the renderer rewrites is the XLSForm `${name}` shorthand, which is not
XPath. Like pyxform, it becomes the referenced field's path: absolute
(`/data/consent`) in general, and relative (`../sibling`) when the referring field
and its target are in the same repeat, since an absolute path there would resolve to
the first repeat instance rather than the current one. A `${name}` in a label or hint
becomes an inline `<output value="..."/>`. Everything around a reference is passed
through untouched.

A reference that cannot be resolved fails rendering rather than emitting broken
XPath, and the form is omitted from `/formList` with the reason available from the
download route. That covers a name no field defines, a name defined twice, anything
that is not a plain field name (`${last-saved#x}` included), and a `${...}` in
`constraint_message`, which becomes the `jr:constraintMsg` attribute and cannot carry
an `<output>` child.

`meta/instanceID` is the idempotency key. Collect splits a large submission across
several POSTs, repeating the identical instance XML each time, so:

- a repeat POST of the same instanceID adds attachments and does not create a second
  submission (201 either way, as the spec requires),
- the same instanceID with **different** XML is a collision, not a resubmission, and
  returns 409 without touching the stored record,
- an instanceID already held by a **different** user also returns 409,
- the same instanceID under a different form is an independent submission.

Attachments are written to `<data dir>/attachments/<submission uuid>/<attachment
uuid>`. Both path components are server-generated UUIDs; the client's file name is
recorded as metadata only and never becomes a path component. The part's content type is
narrowed to a capture format or to `application/octet-stream` on the way in, so the
recorded type is always one collecta chose. Parts are capped at 50 MB each (the advertised
`X-OpenRosa-Accept-Content-Length`) with a slightly larger whole-request cap; oversized
requests get 413.

**Not supported:** form manifests and external media (`manifestUrl` is not emitted),
`listAllVersions`, `verbose` descriptions, entity lists, encrypted forms, Digest auth,
and draft/test endpoints. A form whose field names are not valid XML names cannot be
rendered as an XForm and is omitted from `/formList`.

---

## Authentication and roles

Users are admin-seeded, there is no signup endpoint. Passwords are hashed with
argon2id, tokens are HS256 JWTs (claims `sub`/`exp`/`role`, 24h expiry, same
conventions as tiletopia-server).

Every account has one of three roles, and the same roles apply over the OpenRosa
routes:

| Role | Can |
|------|-----|
| `viewer` | find forms and read form schemas, nothing else |
| `editor` | that, plus create and import forms, and submit data to any form |
| `admin` | everything, including every form's submissions and the sync queue status |

A form records who created it. Its submissions are readable by that account and by
admins, and its id cannot be reused by anyone else. Forms created before roles were
enforced have no creator, so only an admin can read their submissions.

Beyond that, a form's creator (or an admin) can grant another account read on that one
form with `POST /api/v1/forms/{id}/grants`. A grant covers the form's submissions and
their attachments and nothing else: a grantee cannot delete the form or its data, cannot
re-share it, and cannot list who else holds a grant. Grants are per form, so they do not
widen what the account reaches anywhere else, and they disappear with the form.

A grant is read-only in both directions, which depends on submission ids being unclaimable
rather than on the grant check alone. A grantee sees the ids of the submissions and
attachments they may read, so if refiling one of those ids under a form of their own were
allowed, it would carry that row and its attachments into a form they control and survive
the revoke. That is why an id already on file is refused rather than replaced.

Any other role string is refused everywhere rather than treated as the weakest role,
so an account carrying one can log in and do nothing. `create-user` rejects it.

```bash
# seed a user (password read from stdin, role defaults to admin)
cargo run -p collecta-server -- create-user admin@example.com
cargo run -p collecta-server -- create-user field@example.com editor

# log in, then send the token as a bearer header
curl -X POST http://localhost:3000/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email": "admin@example.com", "password": "..."}'
```

---

## Sync Protocol

Clients queue submissions offline (`collecta-core` `SyncQueue`) and sync in two
directions:

- `POST /api/v1/sync/push` takes `{"submissions": [...]}` and returns a per-item
  result: `accepted`, `duplicate` (that submission id is already stored, re-pushing
  a batch never duplicates rows), or `error` with a message (validation failure,
  unknown form). `SyncQueue::build_push_request` / `apply_push_response` implement
  the client side over the shared `sync_protocol` wire types.
- `GET /api/v1/sync/forms?since=<cursor>` returns form definitions changed after
  the cursor, the ids of the forms deleted since it (`deleted`), and the next cursor.
  Omit `since` for a full refresh. The cursor is opaque (currently
  `<rfc3339>@<rowid>`), so store it and echo it back url-encoded.

A deleted form keeps its row as the tombstone, so deletes ride the same cursor as edits
and a client pulling in order cannot receive a form after the delete that removed it. A
client that has been away longer than it takes to delete a form still gets the tombstone,
since nothing prunes them. Submission deletes do not sync: submissions only travel client
to server, so there is no pull that could hand one back. Attachments are not part of the
protocol.

---

## Persistence

Server state is stored in SQLite (`forms`, `submissions`, `sync_queue`, `users`,
`attachments`, `form_grants` tables), so forms and submissions survive restarts.
Attachment bytes live on disk under the data directory, and the table holds their
metadata and paths. A deleted form keeps its `forms` row with `deleted_at` set, both the
tombstone and what hides it from every read path.

Environment variables:

- `COLLECTA_DB` — database path (default `./collecta.db`; `:memory:` for ephemeral)
- `COLLECTA_ADDR` — listen address (default `0.0.0.0:3000`)
- `COLLECTA_JWT_SECRET` — JWT signing secret, required, at least 32 bytes
  (e.g. `openssl rand -hex 32`); the server refuses to start without it, and an
  empty value counts as unset
- `COLLECTA_DATA_DIR` — root for attachment blobs (default `./collecta-data`)
- `COLLECTA_BASE_URL` — absolute origin advertised in OpenRosa `downloadUrl`s
  (e.g. `https://collect.example.org`). Unset, it is derived from each request's
  `Host`, which is fine directly on the internet but wrong behind a proxy that
  rewrites it.

---

## XLSForm Import

`POST /api/v1/forms/import` accepts an [XLSForm](https://xlsform.org) `.xlsx`
(raw body) and registers the parsed form. The engine models a subset of XLSForm;
the importer maps what it can and preserves the rest rather than dropping it.

**Supported types** (`survey.type`): `text`/`string`, `integer`, `decimal`,
`date`, `time`, `dateTime`, `note`, `geopoint`, `geotrace`, `geoshape`, `image`/`photo`,
`audio`, `video`, `file`, `barcode`, `signature`, `select_one <list>`,
`select_multiple <list>`, `begin_repeat`/`end_repeat`, `begin_group`/`end_group`.

**Mapping notes:**

- `choices` and `settings` (`form_title`, `version`) sheets are read; sheet names
  are matched case-insensitively.
- `required` (`yes`/`true`/`1`) maps to the field's required flag.
- `select_one` attaches its choice list and a `OneOf` constraint the validation
  engine enforces. `select_multiple` attaches choices but membership is not enforced
  (the engine does not validate multi-choice values).
- `begin_group`/`end_group` is flattened (the model has no group container); each
  inner field keeps its group name under `metadata.group`. `begin_repeat` maps to a
  `Repeat` field with nested children.

**Preserved as metadata, not evaluated:** raw `constraint` and `relevant`
expressions, `constraint_message`, `choice_filter`, `appearance`, `calculation`, and
the select `list_name` are stored on `FormField.metadata` verbatim. XLSForm
expression evaluation is not implemented yet, so these are carried through rather
than enforced.

**Unsupported:** computed/logic types such as `calculate`, `rank`, and `range` are
rejected with an error rather than silently coerced.

---

## Quick Start

```bash
# Build
git clone https://github.com/GeoLang/collecta.git
cd collecta && cargo build --release

# Run tests
cargo test

# Start server
cargo run -p collecta-server
```

### Create a Form

```bash
curl -X POST http://localhost:3000/api/v1/forms \
  -H "Content-Type: application/json" \
  -d '{
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "title": "Site Inspection",
    "version": 1,
    "fields": [
      {"name": "site_name", "label": "Site Name", "field_type": "Text", "required": true, "constraints": [], "hint": null, "default": null, "relevant": null, "choices": null, "children": null},
      {"name": "location", "label": "GPS Location", "field_type": "GeoPoint", "required": true, "constraints": [], "hint": null, "default": null, "relevant": null, "choices": null, "children": null},
      {"name": "condition", "label": "Condition", "field_type": "Select", "required": true, "constraints": [], "hint": null, "default": null, "relevant": null, "choices": [{"value": "good", "label": "Good"}, {"value": "fair", "label": "Fair"}, {"value": "poor", "label": "Poor"}], "children": null},
      {"name": "photo", "label": "Site Photo", "field_type": "Photo", "required": false, "constraints": [], "hint": "Take a photo of the site", "default": null, "relevant": null, "choices": null, "children": null}
    ]
  }'
```

---

## Target Use Cases

- **Utility inspections** — pole/pipe condition surveys with GPS and photos
- **Environmental monitoring** — water quality sampling, species observations
- **Construction** — daily reports, safety checklists, progress photos
- **Agriculture** — crop health surveys, soil sampling, pest reports
- **Humanitarian** — needs assessments, health surveys, damage reports
- **Property** — building inspections, property valuations, compliance audits

---

## Related GeoLang Projects

| Project | Integration |
|---------|-------------|
| [TerraVista](https://github.com/GeoLang/terravista) | Map engine core for a future field app |
| [Ptolemy](https://github.com/GeoLang/ptolemy) | Geodatabase backend for collected features |
| [ViewTopia](https://github.com/GeoLang/viewtopia) | Field Data panel lists forms and loads submissions as a map layer |

---

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE).

Copyright (C) 2026 Grok Image Compression Inc.
