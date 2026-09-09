# Architecture

## Workspace

Three crates, `[workspace.dependencies]` pinned at the root `Cargo.toml`:

- `crates/rdw-client` — HTTP client for the RDW open-data Socrata API. Keyset pagination and
  kenteken-range URL building, retry-with-backoff, gzip-compressed CSV fetching (parsed into
  `VehicleRow`/`FuelRow`, raw JSON-like objects, no hardcoded 98/36-column structs), and
  column-metadata fetching.
- `crates/rdw-core` — business logic with no HTTP framework or Socrata HTTP details: merge-join of
  sorted vehicle/fuel cursors, row widening (curated vehicle columns plus one `Brandstof` cell
  joining every fuel type for the plate), CSV/ZIP assembly with the Excel row-limit split, the
  fixed-window rate limiter, and column-metadata fallback.
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
4. With a `limit`, vehicles are paged sequentially with keyset pagination (`$where=merk in(...) AND
   kenteken > '<last>'`, `$order=kenteken`) — never `$offset`. For each vehicle page, fuel data is
   fetched BY PLATE with an `in(...)` filter over that page's kentekens, not by kenteken range: a
   brand's plates are scattered across the whole kenteken space, so a range query would drag in
   nearly the entire fuel dataset. Without a `limit`, vehicles are fetched via many concurrent,
   fixed-boundary kenteken RANGES (`kenteken > 'lo' AND kenteken <= 'hi'`, evaluated by Socrata, not
   the client) pulled off a shared work queue, each range still fetching its own fuel data by plate;
   a single export-wide failure counter/abort flag is shared across every range so a proportional
   fuel-failure threshold applies once, not once per range. Both vehicle and fuel data are fetched as
   gzip-compressed CSV (smaller than JSON on the wire), parsed by the response's own header row.
5. The two sorted cursors are merge-joined: each vehicle row keeps one output row, with up to 3
   fuel types joined into a single `Brandstof` cell (`Benzine, Elektriciteit` for a hybrid). A 4th
   fuel entry, an out-of-order fuel sequence, or an unsorted cursor fails loudly (502), rather than
   silently truncating or duplicating data.
6. Rows are assembled into a single CSV, or split into numbered `part-N.csv` files inside a ZIP once
   the row count exceeds Excel's 1,048,575-data-row limit. Assembly stages to a temp file and only
   returns success once every page has succeeded; on any failure, no partial CSV is ever sent and
   the temp file is cleaned up.
7. The response is streamed via `handlers.rs:268-285`, `Content-Type: text/csv` or
   `application/zip`, and a `Content-Disposition: attachment` filename. The staged CSV is
   gzip-compressed on disk; a client that sends `Accept-Encoding: gzip` gets those bytes as-is
   (`Content-Encoding: gzip` + a known `Content-Length`), and any other client gets them decompressed
   on the fly (chunked, since the decompressed length is not known up front) — unless the staged size
   exceeds `UNCOMPRESSED_SIZE_THRESHOLD_MB` (default 50MB), in which case the request is refused with
   406 rather than decompressing a potentially multi-hundred-MB body for a client that could simply
   have asked for gzip. Every mainstream browser sends `Accept-Encoding: gzip` automatically, so this
   406 does not affect the plain browser-download flow.

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
| 406 | Staged export exceeds `UNCOMPRESSED_SIZE_THRESHOLD_MB` and the client did not send `Accept-Encoding: gzip` |
| 429 | Rate limit exceeded, or an export is already in progress |
| 502 | RDW upstream failure (non-timeout) or a data-integrity check failed |
| 504 | RDW request timed out |

Error bodies render as a small HTML page when the request's `Accept` header prefers `text/html`,
and as plain text otherwise (including when `Accept` is absent, `application/json`, or malformed).

## Deployment paths

- **Local/Docker** (primary): see `Dockerfile` and `docs/DEPLOYMENT.md`. No size or duration limits;
  the intended path for full, unrestricted exports.
- **Vercel** (optional): see `vercel.json`, which now runs this service under `"runtime": "container"`
  rather than the Vercel Functions runtime, so the historical ~4.5MB response-body cap no longer
  applies. Vercel's platform-level request timeout and container restart/cold-start behavior still
  apply, and a full unrestricted export can still be large and slow enough that local Docker remains
  the primary path for it; small limited exports (e.g. `?limit=10000`) work well on either.
