# Plan: RDW Fuel Data CSV API (Rust/Axum)

**Status:** Ready for Development  
**Acceptance:** User explicitly approved all recommendations (2026-09-07, "please do all what is bestt")  
**Frontier:** Empty — no open questions  

---

## Business Objective

Expose a single HTTP endpoint (`GET /api/v1/fuel`) that retrieves vehicle registration and fuel consumption data for Toyota, Lexus, and Suzuki from the RDW (Rijksdienst voor het Wegverkeer) open-data Socrata API, and returns it as a downloadable CSV file (or zipped CSV parts if data exceeds Excel's row limit). The service must be local-first, require no database or Redis, and remain cheap to run and host.

---

## Acceptance Criteria & Test Plan Matrix

Each criterion below has an observable pass/fail condition and a test matrix row (happy path, edge case, failure case, test type).

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| 1 | **Endpoint exists and accepts query parameters** | GET `/api/v1/fuel?brands=toyota,lexus&limit=100&api_key=test-key` returns 200 with CSV content-type | Query with no brands parameter returns data for all three brands; limit=0 parsed correctly | Invalid brand name returns 400; malformed query string returns 400 | Unit + Integration | `curl` test against running service; unit test for query parsing |
| 2 | **CSV output has correct schema** | Response CSV header row lists all 98 vehicle columns plus `fuel1_*`, `fuel2_*`, `fuel3_*` (35 fuel columns each, ~203 total); first data row has matching column count | Vehicle with 0 fuel entries has fuel1_* columns all empty; vehicle with 3 fuel entries has fuel3_* columns populated | Vehicle with 4+ fuel entries (not expected but data must fail loudly, not silently drop) detected and causes 502 error | Unit + Integration | Unit test: row widening logic with 0, 1, 2, 3 fuel entries; Integration: mock Socrata returning 4 entries per vehicle |
| 3 | **Rate limiting enforced: 3/day, 5/week per API key** | First 3 requests in a calendar day succeed (200); 4th returns 429; counter resets next calendar day | Request at 23:59 on day N succeeds; request at 00:01 on day N+1 succeeds (counter reset). Week boundary behaves similarly over 7 calendar days | 5 requests within same calendar week returns 429 on 6th; making request without valid API key returns 401 | Unit + Integration | Unit test: mock system clock, test fixed-window counter increment and reset; Integration: submit requests and verify 429 threshold |
| 4 | **Excel row-limit splitting: single CSV vs. ZIP** | Export with ≤1,048,575 data rows returns single `.csv` file with Content-Type `text/csv` | Export with exactly 1,048,575 rows succeeds as single CSV; 1,048,576 rows triggers ZIP | Export with >1 million rows returns `.zip` with multiple numbered CSV files (`part-1.csv`, `part-2.csv`), each ≤1,048,575 rows including header | Integration | Mock Socrata with counted row responses; verify ZIP structure and row counts per part |
| 5 | **AMENDED by docs/plans/rdw-fuel-partial-failure.md (2026-09-07): Partial CSV delivered when fuel enrichment fails below threshold; status markers clarify unavailable data. Vehicle-page failure still returns 502.** | Vehicle-page fetch fails (transient error exhausts 5 retries) → 502, no CSV sent, temp file cleaned up, quota released (unchanged, zero tolerance: see `failure_vehicle_page_failure_returns_502_not_partial_csv` in `crates/rdw-api/tests/fuel_endpoint.rs`) | A fuel-range fetch fails but the failure count stays at or below `max(3, 10% of fuel fetches attempted)`: export still returns 200 with a partial CSV/ZIP, affected vehicles marked `export_status=fuel_unavailable`, `Content-Disposition` filename marked `-PARTIAL`, and an `X-Export-Warnings` header naming the failure and affected-vehicle counts (see `fuel_page_failure_below_threshold_returns_200_partial_csv`) | Fuel-range failures exceed the threshold → export aborts, 502, no CSV sent, quota released (see `fuel_page_failure_above_threshold_returns_502`); a vehicle-page failure is never degraded, only ever aborted | Integration + Resilience | Mock Socrata with controlled vehicle-page and fuel-range failures; `crates/rdw-api/tests/fuel_endpoint.rs` verifies 502+no-CSV for vehicle-page failure and above-threshold fuel failure, and 200+partial-CSV+markers for below-threshold fuel failure |
| 6 | **Keyset pagination prevents row loss** | Export includes all vehicles for selected brands across all page boundaries without duplication | Pagination crossing a vehicle name boundary (e.g., 'TOYOT' prefix) handles correctly; last vehicle on page N is not repeated on page N+1 | Mock Socrata returns unsorted pages; merge-join detects and fails loudly rather than silently accepting corrupt data | Unit + Integration | Unit test: merge-join logic with out-of-order input; Integration: mock Socrata pagination with intentional disorder, verify detection and 502 response |
| 7 | **Merge-join correctly pairs vehicles with fuel entries** | Vehicle A with 2 fuel records and vehicle B with 1 fuel record are correctly widened; output rows match input row count (1 row per vehicle) | Fuel dataset has vehicle with kenteken X but vehicles dataset doesn't (stale RDW state); merge-join skips unpaired fuel entry | Fuel entry has kenteken that exists in vehicles dataset but at different position in sort order; merge-join detects inverted ordering and fails loudly | Unit + Integration | Unit test: merge-join with matching and mismatched data; Integration: mock Socrata with intentional gaps and inversions |
| 8 | **API key validation: env var + header + query param** | Requests with valid key in header (`X-Api-Key: valid-key`), query param (`?api_key=valid-key`), or both succeed (200) | Request with key in query and different key in header uses header value (header takes precedence); missing key in both places returns 401 | Invalid key (not in env var) returns 401; empty key string returns 401 | Unit + Integration | Unit test: extractor logic; Integration: test all three parameter paths with valid and invalid keys |
| 9 | **Error responses render based on Accept header** | Request with `Accept: text/plain` and invalid brand returns plain-text 400 body; request with `Accept: text/html` returns HTML 400 page | Request with no Accept header defaults to plain text; `Accept: application/json` still returns plain text (JSON not supported) | Malformed Accept header still triggers plain-text fallback gracefully | Unit | Unit test: error response renderer with various Accept values |
| 10 | **Startup: column headers fetched from RDW metadata; fallback used if RDW unreachable** | Service starts, fetches metadata from `https://opendata.rdw.nl/api/views/m9d7-ebf2.json` and `8ys7-d773.json`, caches column names in AppState | RDW metadata endpoint returns 500; service logs warning and uses compiled-in fallback column names; CSV is still correct | Network timeout on metadata fetch at startup; fallback used; note that fallback may become stale if RDW adds columns | Integration | Launch service with mocked RDW metadata endpoint returning 500; verify fallback is used and CSV output is correct |
| 11 | **Concurrency guard: only one export at a time** | First request begins export (takes minutes for full dataset); second concurrent request returns 429; first completes and second is allowed | Two rapid requests attempt concurrency; first acquires lock, second waits and eventually times out (request timeout) or gets 429 | Export in progress; third request arrives during second's wait; queuing logic prevents thundering herd | Integration | Submit overlapping requests; observe that only one processes concurrently and others get 429 or timeout |
| 12 | **Workspace structure: three crates (rdw-client, rdw-core, rdw-api) compile and integrate** | Cargo build succeeds; `cargo test --workspace` runs all unit tests; no warnings from clippy | Shared workspace.dependencies used consistently across all crates | A crate depends on a version not in workspace.dependencies; build fails | Unit | `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace` |

---

## Settled Decisions

| Decision | Rationale | Risk/Tradeoff |
|---|---|---|
| **One endpoint only: GET /api/v1/fuel** | Simplicity; user chose this explicitly from multiple-choice options. | Future feature requests require separate plan. |
| **Query parameters: ?brands=toyota,lexus&limit=200&api_key=KEY** | User preference for query params; brand names case-insensitive on input, matched uppercase exact (`TOYOTA`, `LEXUS`, `SUZUKI`) against `merk` field. Omitting brands fetches all three; omitting limit fetches all data. | No JSON request body; no POST. Prototype API key leaked in logs/proxies — documented as acceptable for prototype stage. |
| **Unknown brand → 400; allowlist only LEXUS, TOYOTA, SUZUKI exact merk match** | Protects against injection and typos; user requirement for exact merk match (variants like `TOYOTA-CHINOOK` excluded). | User cannot request other brands without code change. |
| **Output: single CSV with ~203 columns (98 vehicle + fuel widened as fuel1_*, fuel2_*, fuel3_*)** | One row per vehicle; max 3 fuel entries per plate, so widening is bounded. Simpler Excel/import workflow than separate vehicle/fuel sheets. | 4th fuel entry encountered → 502, not silent truncation. CSV is wide but flat. |
| **Excel splitting: ≤1,048,575 data rows → single .csv; above → .zip with numbered parts** | Excel row limit is 1,048,576 including header; ZIP maintains Excel compatibility. | Client must handle ZIP decompression; one more step than single file. |
| **PAGINATION: keyset/seek only, never $offset** | Socrata does not guarantee stable row ordering across offset pages, risking silent row duplication/loss. Keyset pagination is deterministic. | Requires ordering by kenteken; cannot paginate by arbitrary column. Slightly more complex fetch logic. |
| **JOIN STRATEGY: merge-join over sorted cursors (two pages at a time)** | Fuel dataset has no merk column; Socrata has no cross-dataset join. Merge-join avoids 1.25M-row in-memory hash join (>1 GB RAM) and thousands of single-vehicle requests. O(number of pages) RDW calls, bounded memory. | More complex than a hash join; requires both datasets sorted identically. Data must be validated to detect inversion. |
| **NEVER deliver partial/incorrect CSV** | User's absolute rule: "dont give incocrrect csv back not allowed" (verbatim). | Retry logic + exponential backoff + temp file staging; any failure → 502, no CSV sent. Slower convergence on transients. |
| **Retry: up to 5 times, exponential backoff 250ms → 4s + jitter, only on timeout/5xx/429** | Protects against transient network issues and Socrata throttling. 5 attempts + backoff caps total time per page at ~30s (safe under 30s request timeout). | If RDW is down persistently, client gets 502; no graceful degradation to stale cache. |
| **Rate limiting: fixed-window counter, 3/day + 5/week, in-memory, keyed on api_key OR client IP** | GCRA (governor crate) leaks tokens continuously, turning "3/day" into one request every 8 hours. Fixed-window is simpler and matches user intent. | In-memory state resets on process restart; no Redis required. Stale IP keys can grow the map if eviction is not run; requires periodic cleanup. |
| **API key: ?api_key query param AND X-Api-Key header, valid keys from env var** | User accepted the logging risk for prototyping. Header path is safer for production. | Query param logs in server access logs; use header in production. RDW app token (RDW_APP_TOKEN env var) is server-only, separate, never exposed to client. |
| **Status codes: 200 (success), 400 (bad query), 401 (auth), 429 (rate limit), 502 (RDW failure), 504 (RDW timeout 30s)** | Clear client semantics; 502 and 504 used when upstream fails. | HTML error pages when Accept header prefers HTML; plain text otherwise. Explicit coverage of both success and error paths. |
| **Column headers from RDW metadata endpoint, cached at startup with fallback** | Authoritative schema from RDW; ensures CSV stays in sync with upstream changes (within app restart). | Metadata endpoint fetch at startup can fail; fallback list (hardcoded or config) used; service still starts. Fallback may lag if RDW adds columns. |
| **Cargo WORKSPACE with crates/rdw-client, crates/rdw-core, crates/rdw-api** | Central [workspace.dependencies] for consistency; clear separation of concerns (HTTP, logic, web). | More files to maintain than monolithic binary; requires workspace discipline. |
| **CSV writing with `csv` crate (1.4.0), NOT polars** | Polars is heavy (long build, large binary, ~200MB unpacked). User stated "cheaper is better"; lighter csv crate matches constraint. User accepted recommendation after seeing facts. | Polars has nicer dataframe API; csv crate requires manual row iteration. No performance penalty for this use case. |
| **Tests: unit tests only, NO live-network integration test** | User explicitly skipped manual live smoke test; mocked Socrata via wiremock. | Production verification must be done manually or via separate smoke-test pipeline outside this plan. |
| **Deployment: Dockerfile for local/portable; vercel.json for Vercel native Rust runtime** | User has paid Vercel plan and requested Vercel setup. Local is primary (no size limits). | ADVISOR WARNING (explicit risk): Vercel Functions cap non-streamed bodies (~4.5MB) and have execution-duration limits, so full unrestricted exports cannot run on Vercel. Small limited exports (e.g., limit=10000) work. Document this as a known limitation. Local is the path forward for production. |
| **All code, identifiers, comments in English; RDW field names stay Dutch** | User requirement; upstreaming RDW's native field names ensures traceability. | No localization; developers must understand Dutch column names (e.g., `brandstof_volgnummer` = fuel sequence number). |
| **Concurrency guard: one export at a time** | Full export takes minutes; multiple concurrent requests would hammer RDW and spike resource use. | Sequential processing may queue requests; slower response for bursts. Request that cannot acquire lock gets 429. |

---

## Assumptions

- Valid client API keys are supplied at runtime via an environment variable (e.g., `VALID_API_KEYS=key1,key2,...`).
- `RDW_APP_TOKEN` environment variable may be absent locally; requests will use Socrata's unauthenticated tier (lower rate limit). Service must not crash if this var is missing.
- RDW Socrata API remains available and responsive (30s timeout per call).
- Column metadata fetched at startup; if RDW metadata endpoint is down, compiled-in fallback is used.
- Client IP address is available in request (via `ConnectInfo`); used as fallback key when `api_key` is not supplied.
- Temp files can be safely created and cleaned up in the OS temp directory during request processing.
- CSV row count estimation via `$select=merk,count(*)` is representative (actual row count in subsequent paged fetches may vary slightly due to data churn; query is re-run each request, so numbers are fresh).

---

## Technical Architecture

### Workspace Layout

```
rdw-backend/
├── Cargo.toml                          # Workspace root with [workspace.dependencies]
├── crates/
│   ├── rdw-client/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # HTTP client, keyset pagination, retry logic
│   │       └── ...
│   ├── rdw-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # Merge join, row widening, CSV/ZIP assembly, rate-limit counter
│   │       └── ...
│   └── rdw-api/
│       ├── Cargo.toml
│       ├── src/
│       │   ├── main.rs                 # Axum server, routes, startup
│       │   ├── handlers.rs             # GET /api/v1/fuel handler
│       │   ├── extractors.rs           # Query parameter parsing, API key extraction
│       │   ├── errors.rs               # Error types and response rendering
│       │   └── ...
│       └── ...
├── docs/
│   ├── plans/
│   │   └── rdw-fuel-csv-api.md         # This file
│   └── decisions.md                    # (if it exists, read it; do not create)
├── Dockerfile                          # Local/portable deployment
└── vercel.json                         # Vercel native Rust runtime config
```

### Data Flow

1. **Client Request**: GET `/api/v1/fuel?brands=toyota,lexus&limit=1000&api_key=mykey`
2. **Parsing & Validation**: Extract query params; validate API key against env var; check rate limit (fixed-window counter).
3. **Row Count Estimate**: Call Socrata `$select=merk,count(*) where merk in (...)` to determine if output will fit in single CSV or needs ZIP.
4. **Fetch & Merge**:
   - Vehicles: Keyset paginate (`$where=merk in(...) AND kenteken > '<last>' ORDER BY kenteken LIMIT 50000`).
   - For each vehicle batch, fetch matching fuel rows: `$where=kenteken >= '<batch_lo>' AND kenteken <= '<batch_hi>' ORDER BY kenteken, brandstof_volgnummer`.
   - Merge-join the two cursors, widening each vehicle row with its fuel entries (fuel1_*, fuel2_*, fuel3_*).
   - Validate: if a vehicle has >3 fuel entries, return 502 (data error; do not silently drop).
5. **CSV Assembly**: Write merged rows to temp file as batches complete. If single CSV, write all; if ZIP, write multiple CSVs with header rows.
6. **Response**: Only after all pages succeed and temp file is cleanly closed, send response with Content-Length (not streamed). Any failure → 502, no bytes sent, temp file cleaned.
7. **Cleanup**: Delete temp file.

### Socrata API Usage

- **Vehicles** (`m9d7-ebf2`): 98 columns; keyset pagination by `kenteken`.
- **Fuel** (`8ys7-d773`): 36 columns (including `kenteken`, `brandstof_volgnummer`); merged via range query on `kenteken`.
- **Metadata**: `https://opendata.rdw.nl/api/views/{dataset_id}.json` → columns[].fieldName.
- **Authentication**: `X-App-Token: <RDW_APP_TOKEN>` header (server-side only, from env var; client never sees it).

### Rate-Limit Counter (Fixed-Window)

**Per api_key or client IP:**
- **Daily window**: reset at 00:00:00 UTC (calendar day).
- **Weekly window**: reset at Monday 00:00:00 UTC (calendar week).
- Both checked; request succeeds only if both counters are within limits (3/day AND 5/week).
- In-memory DashMap with periodic eviction of stale entries (age > 8 days) to prevent unbounded growth.
- If a request fails with 502/504, quota is NOT consumed.

### Dependencies (Exact Versions from workspace.dependencies)

```toml
[workspace.dependencies]
axum = "0.8.9"
tokio = { version = "1.53.1", features = ["full"] }
reqwest = { version = "0.13.4", features = ["json"] }
tower-http = "0.7.1"
serde = { version = "1.0.229", features = ["derive"] }
csv = "1.4.0"
zip = "8.6.0"
tracing = "0.1.44"
tracing-subscriber = "0.3"
wiremock = "0.6"  # for tests only
```

Note: `governor` is NOT used (GCRA token leakage issue).

---

## Error Handling & Resilience

- **Request Timeout**: 30s per RDW call; HTTP 504.
- **RDW HTTP Errors (5xx, 429)**: Retry up to 5 times with exponential backoff (250ms, 500ms, 1s, 2s, 4s + jitter). After 5th failure, HTTP 502.
- **Data Validation**: Merge-join detects out-of-order cursors or >3 fuel entries per vehicle → HTTP 502 with explanatory plain-text or HTML error (based on Accept header).
- **API Key**: Invalid or missing → HTTP 401.
- **Rate Limit**: Exceeded → HTTP 429 with Retry-After header.
- **Query Validation**: Unknown brand, invalid limit, malformed params → HTTP 400.
- **CSV Assembly Failure**: Temp file write error → HTTP 502.

---

## Verification Commands

Run all verifications in the project root (`/Users/svennijhuis/Desktop/development/personal/rdw-backend/`).

### Build & Format

```bash
# Build all crates
cargo build --workspace

# Check code formatting
cargo fmt --check --all

# Run clippy linter
cargo clippy --workspace --all-targets -- -D warnings

# Build release binary (smaller, optimized)
cargo build --release
```

### Test

```bash
# Run all unit tests with output
cargo test --workspace -- --nocapture

# Run tests for specific crate
cargo test -p rdw-api

# Run tests matching a pattern
cargo test test_merge_join --workspace
```

### Runtime Verification

```bash
# Start the service locally
cargo run -p rdw-api

# In another terminal, test a request (with valid API key from env var)
export VALID_API_KEYS="test-key-123"
export RDW_APP_TOKEN="test-app-token"
curl -v "http://localhost:3000/api/v1/fuel?brands=toyota&limit=10&api_key=test-key-123"

# Verify CSV output
curl -s "http://localhost:3000/api/v1/fuel?brands=toyota&limit=10&api_key=test-key-123" \
  | head -5 | cut -d, -f1-10

# Test invalid API key (should return 401)
curl -v "http://localhost:3000/api/v1/fuel?brands=toyota&limit=10&api_key=wrong-key"

# Test rate limit (after 3 requests in same day, 4th should return 429)
for i in {1..5}; do
  curl -w "\nStatus: %{http_code}\n" \
    "http://localhost:3000/api/v1/fuel?brands=toyota&limit=10&api_key=test-key-123"
done

# Test invalid brand
curl -v "http://localhost:3000/api/v1/fuel?brands=ford&api_key=test-key-123"

# Test Accept header for HTML error response
curl -v -H "Accept: text/html" \
  "http://localhost:3000/api/v1/fuel?brands=ford&api_key=test-key-123"
```

### Deployment (Docker)

```bash
# Build Docker image
docker build -t rdw-api:latest .

# Run container locally on port 3000
docker run -p 3000:3000 \
  -e VALID_API_KEYS="test-key-123" \
  -e RDW_APP_TOKEN="test-app-token" \
  rdw-api:latest

# Test within container
docker run --rm rdw-api:latest cargo test --workspace
```

---

## Tasks

**Execution order:** Complete tasks 1–15 in sequence. Task 16 (Vercel) is optional and can be deferred.

### 1. Initialize Cargo Workspace

Create `Cargo.toml` (workspace root) with `[workspace]` and `[workspace.dependencies]` defining exact versions for all shared crates (axum, tokio, reqwest, tower-http, serde, csv, zip, tracing, wiremock).

### 2. Create `crates/rdw-client` (Socrata HTTP Client)

- Implement HTTP client wrapping `reqwest`.
- Implement keyset pagination logic: accept `merk` list, `limit`, `last_kenteken` cursor; build SODA $where/$order/$limit query.
- Implement retry logic: 5 attempts, exponential backoff 250ms → 4s + jitter, retry only on timeout/5xx/429.
- Expose `VehicleRow` and `FuelRow` structs (deserialize from Socrata JSON; handle omitted null fields).
- Unit tests with wiremock-mocked Socrata responses.

### 3. Create `crates/rdw-core` (Business Logic)

- Implement merge-join: accept two sorted cursors (vehicle batch + fuel batch), produce one row per vehicle widened with fuel1_*, fuel2_*, fuel3_* columns.
- Validate that fuel entries are in order by kenteken, then brandstof_volgnummer; detect inversion and fail loudly.
- Validate that max fuel entries per vehicle is 3; if >3 found, return error (do not silently truncate).
- Implement CSV writer using `csv` crate: write merged rows to temp file.
- Implement ZIP writer: partition rows into numbered CSVs, each with ≤1,048,575 data rows + header.
- Implement fixed-window rate-limit counter: track 3/day and 5/week per api_key or IP, with periodic eviction.
- Implement column metadata caching: struct to hold cached column names; function to fetch from RDW; fallback list.
- Unit tests for merge-join, widening, rate-limit counter, CSV/ZIP assembly.

### 4. Create `crates/rdw-api` (Axum Web Service)

- Setup Axum server on `0.0.0.0:3000` with routes.
- Implement GET `/api/v1/fuel` handler: extract query params (brands, limit, api_key), validate API key, check rate limit, orchestrate fetch/merge/csv.
- Implement query extractor: parse `brands` (comma-separated, case-insensitive, validate against allowlist), `limit` (positive int or omitted), `api_key` (optional, query param or X-Api-Key header).
- Implement error types and response rendering: 400, 401, 429, 502, 504 with plain-text body; if Accept: text/html, render small HTML error page.
- Startup logic: fetch column metadata from RDW; log warnings if fallback is used; initialize AppState with cached metadata, rate-limit counter, HTTP client.
- Concurrency guard (Mutex) to ensure only one export runs at a time; other requests get 429.
- Unit and integration tests with wiremock-mocked RDW.

### 5. Create Root `Cargo.toml` and Workspace Configuration

Link all three crates. Verify `cargo build --workspace` succeeds.

### 6. Run `cargo fmt` and `cargo clippy`

Ensure all code is formatted and passes lint checks with no warnings.

### 7. Write Comprehensive Unit Tests

- **rdw-client**: Socrata pagination logic, retry backoff, timeout handling.
- **rdw-core**: Merge-join with matching/mismatched data, >3 fuel entries detection, rate-limit counter (mocked clock), CSV/ZIP assembly with exact row counts.
- **rdw-api**: Query parameter parsing, API key validation, error response rendering, Accept header handling.

Run `cargo test --workspace`.

### 8. Verify All Acceptance Criteria

Execute verification commands above; confirm each criterion passes.

### 9. Create `Dockerfile`

- Build stage: `rust:latest` image, cargo build --release.
- Runtime stage: minimal image (ubuntu:24.04 or alpine), copy binary and set entry point.
- Expose port 3000.
- Set env var defaults (VALID_API_KEYS, RDW_APP_TOKEN, RUST_LOG).

### 10. Create `.dockerignore`

Exclude `.git`, `target/`, `docs/`, to minimize image size.

### 11. Create `vercel.json`

Configure for Vercel's native Rust runtime. Specify that entry point is the rdw-api binary and port is 3000. Add deployment notes warning about body size and execution-time limits.

### 12. Create `.env.example`

Document required env vars: `VALID_API_KEYS`, `RDW_APP_TOKEN` (optional), `RUST_LOG`, `SERVER_PORT`, with example values.

### 13. Create `/docs/ARCHITECTURE.md`

High-level overview of workspace structure, data flow, API contract, and deployment paths (local Docker, Vercel).

### 14. Create `/docs/DEPLOYMENT.md`

Step-by-step instructions: build Docker image, run locally, test endpoints. Include Vercel deployment steps and warning about body-size/timeout limits.

### 15. Create `/docs/RATE_LIMIT.md`

Explanation of fixed-window counter, daily/weekly windows, eviction strategy, and the fact that state resets on service restart (no persistence).

### 16. *(Optional, can defer)* Set up Vercel Deployment

- Push repo to GitHub.
- Link GitHub repo to Vercel project.
- Set env vars in Vercel dashboard (VALID_API_KEYS, RDW_APP_TOKEN).
- Deploy using `vercel deploy --prod`.
- Test small export with `limit=1000` to verify it works within Vercel's constraints.
- Document known limitation: full unrestricted export exceeds Vercel's 4.5MB response body limit; local Docker is the solution for production.

---

## Open Questions

None.

---

## Dependencies & Risk Mitigations

| Dependency | Version | Risk | Mitigation |
|---|---|---|---|
| Socrata API stability | N/A | RDW endpoint could be down or slow | 30s timeout per call; retry up to 5 times with exponential backoff; fallback error page if RDW unavailable. |
| Column schema drift | N/A | RDW adds/removes columns in future | Metadata is fetched at startup and cached; app remains running until restart. If schema changes mid-run, next restart picks up new schema. |
| Vercel body limit | 4.5MB | Full unrestricted export may exceed limit | Document as known limitation. Local Docker is primary for production. Small limited exports (e.g., limit=10000) work on Vercel. |
| Rate-limit state loss | In-memory (no Redis) | Process restart clears all rate-limit counts | Documented in RATE_LIMIT.md as expected behavior for prototype. Production should consider Redis or database-backed counter. |
| Temp file cleanup failure | OS file system | Crashed request leaves orphaned temp files | Always use defer/finally to clean up; monitor disk space in production. |

---

## Scope

### In Scope

- Single CSV endpoint (`GET /api/v1/fuel`) with query parameters.
- Socrata HTTP client with keyset pagination and retry logic.
- Merge-join of vehicles and fuel datasets.
- CSV and ZIP assembly with Excel row-limit splitting.
- Fixed-window rate limiting (3/day, 5/week per key).
- Error handling and plain-text/HTML error responses.
- Workspace scaffolding with three crates and shared dependencies.
- Dockerfile for local/portable deployment.
- `vercel.json` for Vercel setup (with documented body-size limitation).
- Unit tests with wiremock-mocked RDW.
- Documentation: ARCHITECTURE.md, DEPLOYMENT.md, RATE_LIMIT.md.

### Out of Scope

- Database or Redis; all state in-memory.
- Second endpoint or JSON API.
- HTML data pages (HTML only for error responses).
- Authentication beyond prototype API key.
- Live-network integration tests (all tests mocked).
- Actually deploying to Vercel (setup and local test only).
- Multi-region or high-availability setup.

---

## Record: User Acceptance

On 2026-09-07, user reviewed the complete planning document (including facts, recommendations, settled decisions, and tradeoffs) and answered: **"please do all what is bestt."**

This constitutes explicit acceptance of:
1. The recommendation to use `csv` crate instead of polars (lighter, faster build, cheaper to run).
2. All settled decisions and their tradeoffs.
3. The documented risks and limitations (e.g., Vercel body-size limit, in-memory rate-limit state loss, temp file cleanup).

No further confirmation rounds are required.

---

## Next Steps

1. Proceed with **Task 1** (initialize Cargo workspace).
2. Implement crates in sequence (rdw-client, rdw-core, rdw-api).
3. Run verification commands (build, fmt, clippy, test) after each task.
4. After all tasks complete, run final acceptance-criteria verification.
5. Push code to git (when developer is ready) and optionally deploy.

---

## Review Merge — Round 1

**Verdict:** NOT VERIFIED

**Reason:** Criterion 2 and Criterion 11 are not verified (gates block `pass`).

**Security Gate:** RAN — API-key auth path, untrusted SoQL query input, outbound HTTP to third party, temp file staging/deletion, HTML error rendering.

**Test Suite Result:** 62 passed, 0 failed (`cargo test --workspace --no-fail-fast`). All formatting and lint checks pass.

### Consolidated Findings (5 total, deduped by location + cause)

Ranked by severity. A `fail` or `not verified` criterion blocks `pass`.

| Rank | Severity | Criterion | Location | Problem | Evidence | Recommendation |
|---|---|---|---|---|---|---|
| 1 | HIGH | 2 | crates/rdw-core/src/metadata.rs:20 | Fallback path incomplete: fallback_vehicle_columns() ~47 of 98 required; fallback_fuel_columns() ~20 of 36 required. CSV is ~104 columns wide instead of ~203 when RDW metadata fetch fails. | squad-verifier Criterion 2 marked `not verified`; squad-reviewer Finding 1 | Populate both fallback lists with complete column sets (98 from m9d7-ebf2, 36 from 8ys7-d773) per https://opendata.rdw.nl/api/views/{id}.json columns[].fieldName. Add test asserting header width equals 98 + 3*35. |
| 2 | HIGH | — | crates/rdw-api/src/pipeline.rs:47 | Memory accumulation: all widened rows held in Vec<Vec<String>> before assemble() called. ~5–10 GB RAM estimated for full 1.25M-vehicle export at ~200 columns; contradicts plan's bounded-memory rationale for merge-join. | squad-reviewer Finding 2 | Stage each completed page to temp file immediately instead of accumulating. Change assemble() to accept iterator/stream of row batches and write each batch immediately. Preserve never-partial guarantee by sending only after every page succeeds. |
| 3 | HIGH | — | Dockerfile:2 | Unpinned base images: `rust:latest` and runtime base are floating tags. Builds not reproducible; future image compromise or change enters silently. (A02 Security Misconfiguration per OWASP Top 10:2025) | squad-security-reviewer Finding 1 | Pin both stages to explicit versions by digest. Replace `FROM rust:latest AS builder` with pinned toolchain matching project Rust version. Pin runtime base to specific patch level. |
| 4 | HIGH | — | Dockerfile:9 | No USER directive: service runs as UID 0 inside container. Any process compromise holds root-equivalent capability. (A02 Security Misconfiguration per OWASP Top 10:2025) | squad-security-reviewer Finding 2 | Create unprivileged user and switch to it before ENTRYPOINT, e.g., `RUN useradd -m -u 1000 appuser` followed by `USER appuser`. Ensure temp directory for CSV staging is writable by that user. |
| 5 | MEDIUM | 11 | crates/rdw-api/tests/fuel_endpoint.rs | No test exercises real concurrency: every integration test sequential, so single-export Semaphore and Busy-to-429 mapping never exercised under real concurrency. | squad-verifier Criterion 11 marked `not verified`; squad-reviewer Finding 3 | Add tokio integration test holding first export open against slow mock while issuing second request concurrently. Assert second receives 429 and guard serializes exports. |

### Assessments Explicitly Cleared (No Findings)

- **squad-simplifier:** No duplication, abstraction, or wrong-altitude work detected. Three-crate split justified; shared retry/timeout/error handling in client layer; test fixtures module-local; Dockerfile/vercel.json/docs implement plan tasks 9–15 without redundancy.
- **squad-reviewer secondary:** Keyset cursor strict ordering prevents duplication/skipping; merge-join validation covers ordering/inversion/>3-fuel/orphan rows; header/widen iteration identical (no ragged CSV); rate-limit day/week/quota logic correct; retry per-call; temp cleanup on all paths.
- **squad-security-reviewer secondary:** SoQL injection prevented (allowlist + escape_soql() double-quote); timing channel not realistic (SipHash randomized); HTML escaping applied; no RDW body/header reflection; secrets not in logs/tracing/CSV; temp files staged/removed; no path-traversal/SSRF (hardcoded endpoints); no known CVEs per cargo audit; .env excluded by .dockerignore.

### Replan Required

Yes — every finding is fixable within this diff. Fixes do not require plan changes or new tasks; all fall into existing work scope (Dockerfile refinement, metadata completion, pipeline buffering, test addition).
