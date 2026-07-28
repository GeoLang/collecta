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
- **JWT auth** with argon2id password hashing and admin-seeded users
- **Sync protocol**: idempotent submission push and cursor-based form pull
- **XLSForm import** (`.xlsx` survey/choices/settings) into the form model
- **Offline sync queue** as a library type in `collecta-core`, for a client to build on

### Status

**Working:** the Axum server, SQLite persistence, JWT auth, form CRUD, submission
validation and ingestion, XLSForm import, and the push/pull sync endpoints. All are
covered by tests (`cargo test` runs 57).

**Not built yet:**

- **No client application.** There is no mobile app, desktop app, or FFI layer. The
  `SyncQueue` and `AttachmentStore` types in `collecta-core` are there for a client
  author to use, and nothing in this repo uses them.
- **Media attachments are deferred.** `Photo`, `Audio`, `Video`, `File`, and `Signature`
  exist as form field types and an `Attachment` struct exists in `collecta-core`, but
  the server has no upload or download endpoint, no multipart handling, and no blob
  table. Nothing captures or stores a photo today.
- **No deletes.** There are no tombstones and no delete endpoints for forms or
  submissions, so sync cannot propagate a deletion.
- **Roles are not enforced.** The JWT carries a `role` claim and no endpoint checks it.
  Any valid token gets full access.
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
- **Conditional visibility**: Show/hide fields based on other field values
- **Repeat groups**: Nested sub-forms for multiple entries (e.g., "list all items inspected")
- **Default values**: Pre-fill fields with constants or calculated values
- **Help text**: Per-field hints for data collectors

### Offline Sync Queue

A `collecta-core` library type for a client to drive. No client in this repo uses it.

- **Queue submissions locally** so collection does not need connectivity
- **Exponential backoff retry**: 5s → 10s → 20s → 40s → ... capped at 5min
- **Max retries** with permanent failure status after threshold
- **Status tracking**: Pending, InProgress, Synced, Failed, Abandoned
- Submissions only. Attachment sync is not implemented on either side.

### Validation Engine

- Required field enforcement
- Numeric range validation (min/max)
- Text length validation
- Pattern matching (glob-style)
- OneOf constraint (value must be from allowed set)
- Unknown field detection
- Full error reporting (all errors returned, not just first)

### REST API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check (public) |
| POST | `/api/v1/auth/login` | Exchange email/password for a JWT (public) |
| GET | `/api/v1/forms` | List all forms |
| POST | `/api/v1/forms` | Create a form (JSON) |
| POST | `/api/v1/forms/import` | Import an XLSForm (`.xlsx` request body) |
| GET | `/api/v1/forms/{id}` | Get form schema |
| GET | `/api/v1/forms/{id}/submissions` | List submissions |
| POST | `/api/v1/forms/{id}/submissions` | Submit data (validates against schema) |
| GET | `/api/v1/sync/status` | Get sync queue status |
| POST | `/api/v1/sync/push` | Batch-upload queued submissions (idempotent) |
| GET | `/api/v1/sync/forms?since=<cursor>` | Form definitions updated since cursor |

All endpoints except `/health` and login require `Authorization: Bearer <jwt>`. There is
no attachment upload endpoint and no delete endpoint.

---

## Authentication

Users are admin-seeded, there is no signup endpoint. Passwords are hashed with
argon2id, tokens are HS256 JWTs (claims `sub`/`exp`/`role`, 24h expiry, same
conventions as tiletopia-server). The `role` claim is carried but not checked: every
valid token has the same access.

```bash
# seed a user (password read from stdin)
cargo run -p collecta-server -- create-user admin@example.com

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
- `GET /api/v1/sync/forms?since=<cursor>` returns form definitions updated after
  the cursor plus the next cursor; omit `since` for a full refresh. The cursor is
  opaque (currently `<rfc3339>@<rowid>`) — store and echo it back url-encoded.

Deletion does not sync. There are no tombstones, so a form or submission removed on
one side stays on the other. Attachments are not part of the protocol.

---

## Persistence

Server state is stored in SQLite (`forms`, `submissions`, `sync_queue`, `users`
tables), so forms and submissions survive restarts.

Environment variables:

- `COLLECTA_DB` — database path (default `./collecta.db`; `:memory:` for ephemeral)
- `COLLECTA_ADDR` — listen address (default `0.0.0.0:3000`)
- `COLLECTA_JWT_SECRET` — JWT signing secret, required, at least 32 bytes
  (e.g. `openssl rand -hex 32`); the server refuses to start without it

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
| [GeoGit](https://github.com/GeoLang/geogit) | Version control for collected datasets |
| [ViewTopia](https://github.com/GeoLang/viewtopia) | Web viewer for submitted data |
| [GeoKode](https://github.com/GeoLang/geokode) | Reverse geocode submission locations |

---

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE).

Copyright (C) 2026 Grok Image Compression Inc.
