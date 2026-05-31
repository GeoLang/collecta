# Collecta

[![CI](https://github.com/GeoLang/collecta/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/collecta/actions)

**Schema-driven field data collection** — offline-first mobile forms, validation, attachments, and sync for the GeoLang ecosystem.

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024_edition-orange.svg)](https://www.rust-lang.org/)

> Part of the [GeoLang](https://github.com/GeoLang) geospatial platform.

---

## Overview

Collecta is an open-source alternative to ArcGIS Field Maps, KoboToolbox, and ODK Collect. It provides:

- **Form schemas** with typed fields, conditional logic, and validation constraints
- **Offline-first submissions** with sync queue and exponential backoff retry
- **GPS capture** (point, trace, shape) integrated into forms
- **Attachment handling** (photos, audio, video, signatures, barcodes)
- **REST API** for form management and submission ingestion
- **XLSForm-compatible** field types and structure

### Comparison

| Feature | Collecta | ArcGIS Field Maps | KoboToolbox | ODK Collect |
|---------|----------|-------------------|-------------|-------------|
| Open source | ✅ AGPL-3.0 | ❌ | ✅ (AGPL) | ✅ (Apache) |
| Self-hosted | ✅ | ❌ | ✅ | ✅ |
| Offline-first | ✅ | ✅ | Partial | ✅ |
| Binary size | ~5 MB | ~100 MB | Web-based | ~30 MB |
| GPS accuracy tracking | ✅ | ✅ | ✅ | ✅ |
| Geodatabase integration | ✅ (Ptolemy) | ✅ (Esri) | ❌ | ❌ |
| Single binary server | ✅ | ❌ | ❌ (Django) | ❌ (Java) |
| Repeat groups | ✅ | ✅ | ✅ | ✅ |
| Conditional logic | ✅ | ✅ | ✅ | ✅ |
| Barcode/QR scan | ✅ | ✅ | ✅ | ✅ |

---

## Architecture

```
┌────────────────────────────────────────────────────────┐
│  Mobile App (TerraVista + Collecta FFI)                │
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
│  ptolemy (geodatabase) — versioned spatial storage      │
└────────────────────────────────────────────────────────┘
```

---

## Features

### Form Schema

- **20+ field types**: Text, Integer, Decimal, Date, DateTime, Time, Select, MultiSelect, GeoPoint, GeoTrace, GeoShape, Photo, Audio, Video, File, Barcode, Signature, Boolean, Repeat, Note
- **Validation constraints**: Min/Max value, Min/Max length, regex pattern, OneOf
- **Conditional visibility**: Show/hide fields based on other field values
- **Repeat groups**: Nested sub-forms for multiple entries (e.g., "list all items inspected")
- **Default values**: Pre-fill fields with constants or calculated values
- **Help text**: Per-field hints for data collectors

### Offline Sync Queue

- **Queue all submissions locally** — no connectivity required to collect data
- **Exponential backoff retry** — 5s → 10s → 20s → 40s → ... capped at 5min
- **Max retries** with permanent failure status after threshold
- **Status tracking**: Pending, InProgress, Synced, Failed, Abandoned
- **Attachment sync** — binary files synced separately with progress tracking

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
| GET | `/health` | Health check |
| GET | `/api/v1/forms` | List all forms |
| POST | `/api/v1/forms` | Create a form |
| GET | `/api/v1/forms/{id}` | Get form schema |
| GET | `/api/v1/forms/{id}/submissions` | List submissions |
| POST | `/api/v1/forms/{id}/submissions` | Submit data (validates against schema) |
| GET | `/api/v1/sync/status` | Get sync queue status |

---

## Quick Start

```bash
# Build

[![CI](https://github.com/GeoLang/collecta/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/collecta/actions)
git clone https://github.com/GeoLang/collecta.git
cd collecta && cargo build --release

# Run tests

[![CI](https://github.com/GeoLang/collecta/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/collecta/actions)
cargo test

# Start server

[![CI](https://github.com/GeoLang/collecta/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/collecta/actions)
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

## Use Cases

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
| [TerraVista](https://github.com/GeoLang/terravista) | Mobile rendering + GPS for field apps |
| [Ptolemy](https://github.com/GeoLang/ptolemy) | Geodatabase backend for collected features |
| [GeoGit](https://github.com/GeoLang/geogit) | Version control for collected datasets |
| [ViewTopia](https://github.com/GeoLang/viewtopia) | Web viewer for submitted data |
| [GeoKode](https://github.com/GeoLang/geokode) | Reverse geocode submission locations |

---

## License

AGPL-3.0-or-later · Copyright © 2024 GeoLang
