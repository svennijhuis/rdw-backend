# Architecture

## Workspace

Three crates, `[workspace.dependencies]` pinned at the root `Cargo.toml`:

- `crates/rdw-client` — HTTP client for the RDW open-data Socrata API. Keyset pagination URL
  building, retry-with-backoff, `VehicleRow`/`FuelRow` (raw JSON objects, no hardcoded 98/36-column
  structs), and column-metadata fetching.
- `crates/rdw-core` — business logic with no HTTP framework or Socrata HTTP details: merge-join of
  sorted vehicle/fuel cursors, row widening (`fuel1_*`, `fuel2_*`, `fuel3_*`), CSV/ZIP assembly with
  the Excel row-limit split, the fixed-window rate limiter, and column-metadata fallback.
- `crates/rdw-api` — Axum web service: the `GET /api/v1/fuel` route, query/API-key extraction,
  Accept-header-aware error rendering, the fetch/merge/widen pipeline orchestration, and the
  single-export concurrency guard.

## Data flow

1. Client sends `GET /api/v1/fuel?brands=toyota,lexus&limit=1000&api_key=...`.
2. The API key is validated (header `X-Api-Key` takes precedence over `?api_key=`), the query is
   parsed and validated against the brand allowlist, and the fixed-window rate limiter checks both
   the daily and weekly counters for that key.
3. A single-export concurrency guard (`tokio::sync::Semaphore(1)`) ensures only one export runs at a
   time; a second concurrent request gets 429 immediately rather than being queued.
4. Vehicles are paged with keyset pagination (`$where=merk in(...) AND kenteken > '<last>'`,
   `$order=kenteken`, `$limit=50000`) — never `$offset`. For each vehicle page, the fuel dataset is
   queried for the matching `kenteken` range (`$order=kenteken,brandstof_volgnummer`).
5. The two sorted cursors are merge-joined: each vehicle row is widened with up to 3 fuel entries.
   A 4th fuel entry, an out-of-order fuel sequence, or an unsorted cursor fails loudly (502), rather
   than silently truncating or duplicating data.
6. Rows are assembled into a single CSV, or split into numbered `part-N.csv` files inside a ZIP once
   the row count exceeds Excel's 1,048,575-data-row limit. Assembly stages to a temp file and only
   returns success once every page has succeeded; on any failure, no partial CSV is ever sent and
   the temp file is cleaned up.
7. The response is sent with `Content-Length` (not streamed), `Content-Type: text/csv` or
   `application/zip`, and a `Content-Disposition: attachment` filename.

## API contract

`GET /api/v1/fuel`

| Query param | Meaning |
|---|---|
| `brands` | Comma-separated, case-insensitive; from `{LEXUS, TOYOTA, SUZUKI}`. Omitted = all three. |
| `limit` | Non-negative integer cap on output rows. Omitted = unlimited. `0` is a valid, distinct value. |
| `api_key` | Client API key. `X-Api-Key` header takes precedence when both are present. |

| Status | Meaning |
|---|---|
| 200 | CSV or ZIP body |
| 400 | Bad query (unknown brand, malformed limit) |
| 401 | Missing or invalid API key |
| 429 | Rate limit exceeded, or an export is already in progress |
| 502 | RDW upstream failure (non-timeout) or a data-integrity check failed |
| 504 | RDW request timed out |

Error bodies render as a small HTML page when the request's `Accept` header prefers `text/html`,
and as plain text otherwise (including when `Accept` is absent, `application/json`, or malformed).

## Deployment paths

- **Local/Docker** (primary): see `Dockerfile` and `docs/DEPLOYMENT.md`. No size or duration limits;
  the intended path for full, unrestricted exports.
- **Vercel** (optional, native Rust runtime): see `vercel.json`. Known limitation — Vercel Functions
  cap non-streamed response bodies at roughly 4.5MB and enforce an execution-duration limit, so a
  full unrestricted export will not work there. Small limited exports (e.g. `?limit=10000`) do.
