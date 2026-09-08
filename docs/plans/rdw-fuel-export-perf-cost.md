# Plan: RDW Fuel Export Performance & Cost Reduction

**Status:** Ready for Development  
**Acceptance:** User explicitly approved all recommendations (2026-09-08, full scope A–E confirmed)  
**Frontier:** Empty — no open questions  

---

## Business Objective

Reduce the cost and wall time of large fuel exports (e.g., Toyota's 824,620 vehicles) from Socrata RDW data. A single request today costs the user up to ~1 EUR on Vercel and takes over a minute. Cut the cost first (ingress bytes via gzip + CSV format switching + refuse uncompressed), then cut wall time second (concurrent, unordered kenteken ranges). Primary leverage: ingress reduction from ~2.6 GB → ~86 MB (gzip ~20x, CSV ~1.5x). Secondary: parallelism with bounded memory.

---

## Measured Facts (Verified via curl against opendata.rdw.nl)

| Metric | JSON | CSV | Ratio |
|---|---|---|---|
| **5,000 Toyota vehicle rows, no gzip** | 10,083,342 B / 1.37s | 4,022,637 B / 0.72s | 0.40x volume, 0.53x time |
| **5,000 Toyota vehicle rows, gzip** | 457,935 B / 1.40s | 312,090 B / 0.66s | 0.68x volume, 0.47x time |
| **One 50,000-row gzipped JSON page** | 4,733,427 B | — | — |
| **Estimated Toyota full export** | ~1.7 GB vehicles + 0.9 GB fuel | CSV version ~86 MB | 20x total reduction |
| **Vehicle CSV header** | 98 quoted column names, no UTF-8 BOM | — | Matches JSON fieldName order exactly |
| **Fuel CSV pagination** | `$order=kenteken,brandstof_volgnummer` works; volgnummer comes back as quoted string "1" (TEXT-typed upstream) | — | Existing order clause still valid |
| **Shape difference** | JSON omits absent fields | CSV emits every column with empty string | Handling required |
| **2-char prefix distribution (Toyota)** | "00"→1501, "01"→2789, ..., "0G"→1 | Near-uniform across prefixes | Clean split points for concurrent ranges |
| **Non-scalar columns in m9d7-ebf2** | None; all text/number/calendar_date/checkbox | No api_gekentekende_voertuigen_* drift risk | JSON-object-vs-bare-URL output risk does NOT exist |

**Per-page latency is Socrata query time, not transfer. Gzip is a COST lever, not a speed lever. The plan must say this plainly.**

---

## Acceptance Criteria & Test Plan Matrix

Each criterion has an observable pass/fail condition and a test matrix row (happy path, edge case, failure case, test type).

### Scope A: Enable Reqwest `gzip` Feature

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| A.1 | **Reqwest `gzip` feature enabled in Cargo.toml** | `crates/rdw-client/Cargo.toml` lists `reqwest = { version = "0.13.4", features = ["json", "gzip"] }` | Feature already enabled globally in workspace.dependencies; local override not needed | Build fails if feature not present | Unit | `cargo build --workspace` succeeds; `grep "gzip" Cargo.toml` finds feature in rdw-client |
| A.2 | **Builder explicitly calls `.gzip(true)` at construction** | `crates/rdw-client/src/lib.rs:145-148` client builder includes `.gzip(true)` as an explicit statement | A future `default-features = false` still preserves gzip via explicit call | If `.gzip(true)` is omitted and default-features is false, compression is silently disabled | Unit | Code review: locate exact line; test with `default-features = false` and confirm gzip still active via Content-Encoding header in mock response |

### Scope B: Socrata CSV Parsing (Vehicles & Fuel Datasets)

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| B.1 | **CSV cells deserialize as serde_json::Value::String unconditionally** | A cell "007" remains "007" (not 7), "1.50" remains "1.50" (not 1.5); FuelRow::volgnummer() calls Value::as_str and succeeds | An empty cell "" is parsed as String("") but skipped when building row map (criterion B.2); Value::as_str returns Ok | A cell parsed as Number or Boolean; FuelRow::volgnummer() calls Value::as_str on a Number, returns Err; MissingVolgnummer fires; 502 | Unit | Unit test: parse CSV with "007" and "1.50"; assert Value::String; call as_str; confirm volgnummer validation works |
| B.2 | **Empty CSV cells skipped when building row map, not stored as empty strings** | Vehicle row with 98 columns, only 42 populated; Map holds 42 entries, not 98; output is identical to JSON (both omit empty fields via None → field_as_string early return) | A vehicle with no fuel columns populated is a Map of size ~50 (kenteken, merk, variant, etc.) not size 98; memory per row ~30 keys not 98 | Backwards: if empty cells are stored, row memory triples (98 String key duplicates) and 16 concurrent ranges OOM the container | Unit | Unit test: parse CSV with sparse rows; assert Map.len() < 98; integration test: memory RSS on 16-range concurrent export must not exceed single-page footprint |
| B.3 | **CSV columns resolved by response header, never positionally against metadata** | Response header "kenteken,merk,variant,...,handelsbenaming" → build HashMap<&str, usize>; fetch cells by name, not position | A column is absent from header (e.g., RDW removes a column); cell fetch by name returns None and field_as_string returns ""; output is correct | If positional order is used and RDW reorders a column, every cell in every row shifts; "007" kenteken becomes a merk value; no error raised | Unit | Unit test: mock CSV with reordered columns; parse and build header map; fetch cells by name; assert values are correct |
| B.4 | **Truncated CSV body is an error, not a short page** | Mock Socrata returns 50,000 complete rows with valid gzip CRC32+ISIZE footer; body reads fully; page.len() == 50000 | Mock returns 40,000 complete rows; body reads fully; page.len() == 40000 < page_size → loop breaks (short page, not truncation) | Mock returns 25,000 rows then cuts body mid-CSV-record (mid-line, mid-quoted-field); ungzip + csv reader fails; error is retried and eventually raised as 502, not silent short page | Unit + Integration | Unit test: mock gzipped CSV truncated at 80% decompressed size; assert reader error; integration test: truncated body propagates to client as 502, not 200 partial CSV |
| B.5 | **Vehicle page rejected if header lacks `kenteken`; fuel page rejected if header lacks `kenteken` or `brandstof_volgnummer`** | Vehicle CSV header includes "kenteken" at position N; page accepted and parsed | Fuel CSV header includes "kenteken" and "brandstof_volgnummer"; page accepted and parsed | Vehicle page header has 97 columns, missing "kenteken"; page rejected BEFORE empty-page check at pipeline.rs:96-98; 502 with error message naming missing column. Fuel page missing "brandstof_volgnummer"; similarly rejected | Unit | Unit test: parse CSV header; extract required columns; reject if missing; integration test: mock Socrata returning invalid CSV header; verify 502 before merge_join logic runs |
| B.6 | **CSV reader configured with flexible(false); wrong-field-count record errors** | Record with correct field count for the page is accepted | Record with N fields, header has M fields (N ≠ M) → csv::Error::Deserialize fires; treated as body-read error, retried, eventually 502 | A malformed CSV silently shifts columns (would happen with flexible(true)); never occurs because flexible(false) is the default and is enforced | Unit | Unit test: use csv::ReaderBuilder with flexible(false); feed record with wrong field count; assert error. Code review: confirm ReaderBuilder does not call flexible(true) |

### Scope C: Concurrent, Unordered Kenteken Ranges

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| C.1 | **Range tiling expressed solely as Socrata `kenteken > A AND kenteken <= B` clauses** | Vehicles fetch split into 16 ranges (e.g., "0001VH" ≤ ken < "5001VH"); each range's $where includes `AND kenteken > 'lo' AND kenteken <= 'hi'` predicates; Socrata evaluates collation, client never filters rows | Range i's upper bound byte-identical to range i+1's lower bound ("5001VH" is upper of range 0 and lower of range 1); first range unbounded below, last unbounded above | If ranges overlap or gap, rows are duplicated or lost; if client filters rows post-fetch, an off-by-one boundary produces MergeJoinError::UnsortedVehicles in one range only, passing silently in others | Unit + Integration | Unit test: build range boundaries; verify byte-identity at boundaries; verify no gaps. Integration test: concurrency test with intentional boundary errors (mock Socrata returning overlapping ranges); verify detection or data loss |
| C.2 | **Page size scales inversely with range concurrency** | 16 concurrent ranges × ~3,000 rows per page = 16 × 600 KB per page resident ≈ 9.6 MB + overhead < original single 50,000-row page footprint (~100 MB) | 16 ranges × 1,600 rows = 16 × 320 KB < 5.2 MB | 16 ranges × 50,000 rows = 16 × 10 MB = 160 MB; already exceeds single-page footprint; runtime scales by concurrency, OOM risk rises | Integration | Integration test: measure RSS before, during (16 concurrent pages), and after Toyota export; assert peak RSS ≤ single-page peak + constant overhead; vary page size inversely with concurrency; confirm no OOM |
| C.3 | **Global `should_abort` counter checked after every fuel fetch; abort CANCELS in-flight ranges** | Fuel fetch fails in range 5; global counter incremented; threshold checked; if exceeded, abort flag set; all spawned tasks receive cancel signal; JoinSet drops; temp file cleaned; 502 returned | Two ranges fail simultaneously; counter reaches threshold on either one; both abort cleanly; no race condition | An abort decision is made but task X is already mid-fetch; task X completes, writes rows, commits to batch; abort is detected but too late; exported rows violate failure threshold | Unit + Integration | Unit test: mock atomic counter, set abort flag, verify all tasks exit; integration test: two fuel ranges fail concurrently, both detect abort and cancel within 100ms |
| C.4 | **Range list implements work-queue design: many ranges pulled by K workers** | Define 64 fixed two-char kenteken bands (00–0Z, etc.) regardless of actual distribution; spawn K=8 workers; workers pull ranges from queue until empty; slow ranges do not block fast ones | Distribution fetch (next to load_column_metadata) caches band sizes in AppState or memoizes per brand; unavailable or stale distribution → fallback to fixed bands; data correctness unchanged | Distribution is per-export and single-threaded; blocks export start by 2–3s; not measured; design avoids this | Design review | Code review: confirm range definition does not depend on distribution; distribution is caching layer, not required path; queue pulls ranges regardless of size |

### Scope D: Re-tuning & Measurement

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| D.1 | **Re-tune FUEL_CONCURRENCY & FUEL_KENTEKEN_BATCH only if measurement justifies it; AFTER confirming RDW_APP_TOKEN is set** | Environment variable `RDW_APP_TOKEN` is populated (server-side); main.rs:17-20 does not warn; measured request latency is lower; concurrency knee is higher than documented default | RDW_APP_TOKEN is unset; main.rs:17-20 logs warn level "RDW_APP_TOKEN unset; using unauthenticated tier (10% of rate limit)"; concurrency remains at documented default; tuning is deferred | RDW_APP_TOKEN is set but the application logic ignores it; all requests still hit the unauthenticated rate limit; concurrency tuning proceeds and hits Socrata 429s; measurement is invalid | Unit | Check env var in main.rs; run production export with RDW_APP_TOKEN set; measure latency per fuel fetch; compare to baseline without token; confirm latency improvement; only then propose tuning |
| D.2 | **Measurement procedure: per-fuel-fetch latency, page-count, wall-clock time before & after** | Full Toyota export: before (JSON, single-threaded) measured as T1 wall time, I1 ingress bytes; after (CSV, 16-concurrent) measured as T2 wall time, I2 ingress bytes; cost differential calculated; wall-time differential reported | A partial export (limit=10,000) is measured; latency scales sublinearly with row count due to per-page fixed costs (Socrata query parsing, network round trip); smaller export shows different concurrency knee | Latency doubles under concurrency (thread contention, CPU throttling); page_size tuning needed; measurement procedure repeats until diminishing returns | Integration | Run cargo with `RUST_LOG=rdw_api=debug` (or equivalent timing span); measure wall time via `time cargo run -- export toyota`; measure ingress via tcpdump or proxy; report before/after in acceptance verification |

### Scope E: Refuse Uncompressed Response Above Threshold

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| E.1 | **Threshold-based rejection: uncompressed staged file > 50 MB returns 406** | Staged gzip file is 30 MB; uncompressed size is ~600 MB; client omits `Accept-Encoding: gzip`; handler checks staged size, finds 30 MB < 50 MB, gunzips and streams full CSV; HTTP 200 | Staged file is exactly 50 MB; uncompressed check uses > (strict greater-than), allows this edge case; streams CSV; HTTP 200. OR: staged file is 50.000001 MB; 406 returned with message "Accept-Encoding: gzip required" | Staged file is 75 MB; uncompressed streaming would consume >1.5 GB memory and time; client omits Accept-Encoding; handler returns 406 with plain-text message naming `Accept-Encoding: gzip` requirement | Unit + Integration | Unit test: mock staged file of 30, 50, 51 MB; test rejection logic. Integration test: curl without `--compressed`, observe 406 when staged > threshold; curl with `--compressed`, observe 200 |
| E.2 | **Threshold is environment-overridable; default 50 MB; warn-level log when uncompressed export served** | Environment variable `UNCOMPRESSED_SIZE_THRESHOLD_MB=50` (or unset, using default); handler gunzips and streams a <50MB export to an uncompressed client; log statement at warn level: "Uncompressed export served: X MB, client did not send Accept-Encoding: gzip"  | Threshold set to 1000 MB; streaming large uncompressed exports is allowed (higher memory cost accepted); logging still fires | Threshold set to 0 MB; all uncompressed requests rejected; even tiny exports (limit=10) return 406 (overly strict, but testable edge case) | Unit + Integration | Read env var in handlers.rs; set default 50 MB; test with multiple threshold values; check logging output at warn level via RUST_LOG=warn |
| E.4 | **The browser download flow keeps working end to end** | A browser navigates to `/api/v1/fuel?brands=toyota&api_key=...`; it sends `Accept-Encoding: gzip` automatically, so the handler takes the pass-through branch at `handlers.rs:279-283`, sets `Content-Encoding: gzip` + `Content-Length`, and the browser decompresses and saves `fuel-export.csv`. No 406, no manual step, file opens in Excel/Numbers | A `-PARTIAL` export: filename becomes `fuel-export-PARTIAL.csv`, still saved correctly. A ZIP export (`Assembled::Zip`): not gzip-staged, so `Content-Encoding` is absent and it downloads as `fuel-export.zip` unchanged | The browser receives a gzip body with no `Content-Encoding: gzip` header, or a double-gzipped body (see the CompressionLayer DO-NOT), and saves an unreadable file. Or the 406 fires against a browser, breaking the user\'s primary flow entirely | Integration (real browser) | **Manual, mandatory before sign-off**: run the server locally, request a Toyota export from an actual browser address bar, confirm the file lands on disk, opens as CSV, and its row count matches the `$select=count(kenteken)` aggregate from V.1. Also `curl -sI -H "Accept-Encoding: gzip"` and assert `content-encoding: gzip` is present exactly once |
| E.3 | **406 response body messages user: "Staged data exceeds SIZE_THRESHOLD; please retry with Accept-Encoding: gzip"** | Client receives 406; reads body; message is clear and actionable | Message is plain text (not HTML, even if Accept: text/html); client can copy command to retry with `curl --compressed` | 406 body is empty or HTML; user confused; CLI tools cannot parse | Unit | Unit test: 406 response body contains "Accept-Encoding: gzip"; integration test: parse response body and verify plain text |

### Verification: Row Count & Byte-Diff Acceptance

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| V.1 | **End-to-end row count check: one $select=count per export up front, reconciled at end** | Export starts; issue `$select=count(kenteken) where merk in(toyota,lexus,suzuki)` → returns N; after all ranges fetched and merged, emit M rows; if M ≠ N and limit is None, return 502 with mismatch error; if limit is Some, M may be ≤ N (intentional truncation) | Limit=5000; expect M ≤ 5000; exact match not required | Parallel ranges produce duplicate rows (overlapping boundary); M > N; 502 returned before CSV is sent | Integration | Capture output row count during export; compare to pre-export aggregate; test with and without limit; verify 502 on mismatch, 200 on match |
| V.2 | **Byte-diff baseline: CSV export (?limit=2000&brands=lexus) produces identical output row count, kentekens, volgnummers as JSON path** | Run JSON path: `cargo test --test fuel_endpoint test_json_export_lexus_2k -- --nocapture`; save row count; run CSV path with same query; assert row counts match; diff output (should be zero after removing gzip overhead) | Single-row export (limit=1) via both paths; compare CSV format; JSON may have different cell escaping | JSON path includes extra columns (numeric, boolean unwrapped); CSV path includes all columns as strings; if volgnummer is "1" in CSV but 1 in JSON, parsing differs; test detects this | Integration | Both paths export same query; compare (uncompressed) output files byte-by-byte (ignoring header metadata); row counts must match exactly; volgnummer must remain quoted string "1" in CSV |
| V.3 | **Wall-clock measurement for Toyota full export (824,620 vehicles)** | Before: single-threaded JSON path, measure wall time T1, ingress I1. After: 16-concurrent CSV path, measure wall time T2, ingress I2. Report: I1/I2 ratio (target ≥ 20x), T1/T2 ratio (no target, report as-is), cost reduction (I2 × per-byte-cost) | A re-tuned export with different FUEL_CONCURRENCY or page_size may yield T2 < T1 or vice versa; measurement is valid either way | T2 >> T1 and I2 ≈ I1 (no benefit); indicates configuration error or bottleneck; repeated measurement with different settings | Integration | Real export against opendata.rdw.nl; time via `time cargo run`, tcpdump for ingress bytes; document results in implementation notes |

### Known, Accepted, Checked & Closed

| Category | Item | Details |
|---|---|---|
| **Known, Accepted** | **Per-process export_lock & in-process rate limiter multiply by Vercel container count** | crates/rdw-api/src/state.rs:18 Semaphore(1) and dashmap RateLimiter are per-process. Two containers = two independent locks and two independent rate-limit counters. User A's 3 daily requests are tracked in container 1; user B's requests load-balance to container 2 and have their own independent 3 daily counter. A single user making 2 requests to container 1 and 2 requests to container 2 (via load balancer) sees quota only on one container, consuming 4 total instead of 3. This is the largest uncontrolled cost hole. It is KNOWN and ACCEPTED for this change; solving it (redis backend, managed sessions) is out of scope and deferred. Mitigations: (1) document in DEPLOYMENT.md that fixed-IP or session affinity is required for quota to work correctly; (2) log every quota decision; (3) defer to future plan for distributed rate limiting. |
| **Checked & Closed** | **JSON-object-vs-bare-URL output drift risk does NOT exist** | m9d7-ebf2 has five api_gekentekende_voertuigen_* columns; all are plain strings in both JSON and CSV, not url-typed or object-typed. No risk of JSON deserializer unwrapping a URL or object differently from CSV string parsing. Verified against /api/views:m9d7-ebf2.json columns[].dataTypeName. |
| **Checked & Closed** | **CSV cell type inference and value preservation** | Path 1 (JSON): VehicleRow deserializes serde_json, omits absent fields, widen.rs:67,73 call field_as_string on Option<Value>. Path 2 (CSV): serde_json::Value::String unconditionally (criterion B.1), skipped if empty (criterion B.2), widen.rs calls field_as_string. Both paths: None → String::new(); Some(Value::String("")) → "". Byte-identical output. No change in row iteration or key count (Map.get() only, no .keys() or .len() in output path). Output cell value NEVER changes from absent-key to empty-string. **This is why criterion B.2 (skip empty cells) is free—no output contract violation.** |
| **Checked & Closed** | **No output columns are added or moved by this change** | This plan is transport-and-concurrency only. `RowWidener`'s column list and the appended `export_status` column are untouched, so every existing positional assertion in the `rdw-core` widen tests stays valid as written. Any diff to the CSV header is a defect, not an expected change, and criterion V.2's byte-diff is what catches it. |

---

## Settled Decisions

| Decision | Rationale | Risk/Tradeoff |
|---|---|---|
| **Gzip is a COST lever, not a speed lever** | Per-page latency (6.06s for 50k gzipped JSON) is Socrata query time (network round trip, parsing), not transfer time. Gzip reduces byte volume ~20x, cutting cost but not wall time. Parallelism (scope C) cuts wall time. | User understands cost is primary goal. Wall-time speedup is secondary and depends on parallelism. |
| **CSV instead of JSON reduces volume ~1.5x further** | 5k rows: JSON gzip 457,935 B, CSV gzip 312,090 B. Volume per page drops, per-request cost drops further. | CSV parser is different from JSON; must be tested carefully (criteria B.1–B.6). |
| **Concurrent ranges are unordered; output row order is not a contract** | User decision: row order may differ from JSON sequential path. Parallelism requires unordering. | Output is not deterministic; row order varies per run. May surprise users expecting kenteken-ascending order. Documented in DEPLOYMENT.md. |
| **Per-range page size scales with concurrency** | 16 concurrent ranges require smaller pages to stay within memory budget. Measured footprint of single 50k-row page (~100 MB with temporary copies) divided by 16 = ~6 MB per page, ~3k rows per page. | More pages per range; more Socrata calls; higher overhead. Accepted trade-off for bounded memory. |
| **Range tiling via Socrata $where, not client-side filtering** | Socrata collation (byte-order-dependent) is the source of truth. Client-side filtering introduces off-by-one bugs (learned from docs/learnings.md:27-30). | Requires exact boundary coordination; no wiggle room. Boundaries are byte-identical, not approximate. |
| **Global should_abort is not per-range** | Per-range counters would multiply floor by 16 (e.g., 3 failures × 16 ranges = 48 failures tolerated). docs/learnings.md:38-42 warns against this mistake. | Shared atomic counter or mutex required; slightly higher contention. Correctness is paramount. |
| **Refuse uncompressed large exports (406 Not Acceptable)** | A client omitting `Accept-Encoding: gzip` requesting a 600+ MB export would consume >1.5 GB memory and minutes of streaming. Blocking this forces client adoption of compression. | **The browser download flow is UNAFFECTED.** Every mainstream browser sends `Accept-Encoding: gzip, deflate, br` on every request, including a plain address-bar navigation, and transparently decompresses a `Content-Encoding: gzip` body before writing the `Content-Disposition: attachment` file to disk. The user saves a normal `.csv`. The 406 therefore only ever fires for a bare `curl` (no `--compressed`) or a script that strips the header. This must be proven, not assumed: see criterion E.4. |
| **Threshold default 50 MB, environment-overridable** | 50 MB uncompressed (~1 GB decompressed) is a reasonable middle ground; larger thresholds allow memory-intensive streaming. | Too low (<10 MB) blocks small exports; too high (>500 MB) risks OOM. Config lever provides escape hatch. |
| **REQUEST_TIMEOUT is coupled with concurrency** | 30s timeout (lib.rs:25) is whole-request timeout. Concurrency slows each page (thread contention); K ranges × latency per page can exceed 30s. Must re-check timeout when setting K. | Timeout must not be a race condition with concurrency. Testing required. |

---

## Technical Architecture Changes

### 1. Reqwest Gzip Feature (Scope A)

**File:** `Cargo.toml`, `crates/rdw-client/Cargo.toml`, `crates/rdw-client/src/lib.rs:145-148`

- Add `gzip` to reqwest features in workspace.dependencies.
- Explicit `.gzip(true)` call on client builder so `default-features = false` does not silently disable it.

### 2. CSV Parsing Pipeline (Scope B)

**Files:** `crates/rdw-client/src/lib.rs` (new functions), `crates/rdw-core/src/csv_parser.rs` (new module)

- New module `csv_parser.rs` with functions:
  - `parse_csv_page(body: &str) -> Result<Vec<VehicleRow>>` (or FuelRow, depending on dataset).
  - CSV reader configured with `flexible(false)`.
  - Header parsed into HashMap<&str, usize>.
  - Each row checked for required columns; missing column → error.
  - Cells parsed as serde_json::Value::String (no numeric inference).
  - Empty cells skipped (not stored in row Map).
  - Truncated body (csv::Error) treated as body-read error, retried, eventually 502.
- Modify `fetch_vehicles()` and `fetch_fuel_for_kentekens()` to detect Content-Type (application/json vs text/csv) and call appropriate parser.
- OR: Switch both datasets to `.csv` query parameter on Socrata URL; both endpoints support CSV format.

### 3. Concurrent Range Fetching (Scope C)

**Files:** `crates/rdw-api/src/pipeline.rs`, `crates/rdw-core/src/merge.rs`

- Define range boundaries: 64 fixed two-char bands, or derived from distribution (if caching is added later).
- Spawn tokio::task::JoinSet to fetch ranges concurrently.
- Global abort flag (AtomicBool or Mutex<bool>) checked after each fuel fetch.
- On abort, cancel JoinSet (drop receivers, cancel tasks).
- Track (attempted, failures) with shared atomics; check threshold after every fetch.
- Merge-join called once per range with local results; no per-range buffer accumulation.

### 4. Configuration & Tuning (Scope D)

**Files:** `crates/rdw-api/src/main.rs`, `crates/rdw-core/src/config.rs`

- Environment variables: `FUEL_CONCURRENCY` (default 8, tunable), `FUEL_KENTEKEN_BATCH` (default 800, tunable), `FUEL_PAGE_SIZE` (default 3000, scales with concurrency).
- Main.rs checks RDW_APP_TOKEN; log warn if unset.
- Tuning recommendations documented but not changed from baseline until measurement confirms benefit.

### 5. Uncompressed Response Rejection (Scope E)

**Files:** `crates/rdw-api/src/handlers.rs`

- Before streaming uncompressed CSV: check staged file size against `UNCOMPRESSED_SIZE_THRESHOLD_MB` env var (default 50 MB).
- If exceeded: return 406 Not Acceptable with body "Staged data exceeds SIZE_THRESHOLD; please retry with Accept-Encoding: gzip".
- If allowed: gunzip, stream, log at warn level.

### 6. ARCHITECTURE.md Corrections (Advisor Finding 18)

**File:** `docs/ARCHITECTURE.md`

- Line 37: correct "sent with Content-Length (not streamed)" to "streamed via handlers.rs:268-285".
- Lines 26-27: correct fuel query description from "range" to "by-plate with in() filter"; update references to retired range-pagination approach.
- Lines 62-65: correct Vercel Functions limitation; vercel.json now uses "runtime": "container" so 4.5 MB limit no longer applies. Note: Vercel runtime timeouts and container restart behavior still apply; local Docker is primary for full exports.

---

## Acceptance Bar (User Confirmed 2026-09-08)

- **Measured ingress-bytes-per-export drop**: Before (JSON) vs. after (CSV + gzip) quantified; target ≥ 20x total reduction (~2.6 GB → ~130 MB).
- **ONE REAL export run** against opendata.rdw.nl proving CSV path parses correctly and row counts, kentekens, volgnummers match JSON path. (Wiremock answers ANY query so mocked suite proves logic, never protocol.)
- **Byte-diff a small real export** (?limit=2000&brands=lexus) produced by JSON path vs. CSV path. Permitted differences: cell escaping in CSV (quotes), volgnummer as string "1" not number 1, order may differ if concurrency enabled. Document these differences in plan.
- **Wall-clock measurement** for full Toyota export (824,620 vehicles): report before/after wall time (no committed target, just report).

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

# Build release binary (optimized)
cargo build --release
```

### Scope A: Gzip Feature

```bash
# Verify gzip in Cargo.toml
grep -A 5 "reqwest" Cargo.toml | grep -i gzip

# Verify .gzip(true) in code
grep -n "\.gzip(true)" crates/rdw-client/src/lib.rs

# Test with mocked response
cargo test -p rdw-client test_reqwest_gzip_enabled -- --nocapture
```

### Scope B: CSV Parsing

```bash
# Test CSV cell parsing (Value::String unconditional)
cargo test -p rdw-core test_csv_cell_parse_as_string_unconditional -- --nocapture

# Test empty cell skipping
cargo test -p rdw-core test_csv_empty_cells_skipped -- --nocapture

# Test header resolution by name (not position)
cargo test -p rdw-core test_csv_header_column_resolution -- --nocapture

# Test truncated CSV detection
cargo test -p rdw-client test_csv_truncated_body_error -- --nocapture

# Test missing required columns rejected
cargo test -p rdw-core test_csv_missing_kenteken_rejected -- --nocapture
```

### Scope C: Concurrent Ranges

```bash
# Test range tiling (no overlap, no gaps)
cargo test -p rdw-core test_range_tiling_no_overlap_no_gaps -- --nocapture

# Test memory scaling (16 ranges, page size ~3000)
cargo test -p rdw-api test_concurrent_range_memory_bounded -- --nocapture

# Test global abort signal
cargo test -p rdw-api test_concurrent_abort_cancels_ranges -- --nocapture

# Test work queue design
cargo test -p rdw-core test_range_work_queue -- --nocapture
```

### Scope D: Re-tuning & Measurement

```bash
# Verify RDW_APP_TOKEN check
cargo test -p rdw-api test_rdw_app_token_presence_logged -- --nocapture

# Measure wall time for Toyota export (optional, runs long)
time cargo run -p rdw-api -- export toyota
```

### Scope E: Refuse Uncompressed

```bash
# Test threshold rejection
cargo test -p rdw-api test_uncompressed_rejection_above_threshold -- --nocapture

# Test environment override
cargo test -p rdw-api test_uncompressed_threshold_env_override -- --nocapture

# Test 406 response message
cargo test -p rdw-api test_406_response_body_message -- --nocapture
```

### Verification: Row Count & Byte-Diff

```bash
# End-to-end row count check
cargo test -p rdw-api test_row_count_reconciliation -- --nocapture

# Byte-diff: small real export (limit=2000, brands=lexus)
cargo test -p rdw-api test_csv_vs_json_output_byte_diff -- --nocapture

# Wall-clock timing (optional)
RUST_LOG=debug cargo run -p rdw-api -- time_export lexus --limit 2000
```

### Integration: Full Workflow

```bash
# Start service locally
export VALID_API_KEYS="test-key-123"
export RDW_APP_TOKEN="<real-token-if-available>"
export RUST_LOG="rdw_api=debug,rdw_client=debug"
cargo run -p rdw-api

# In another terminal, test end-to-end
curl -v -w "\nStatus: %{http_code}\nTotal time: %{time_total}s\n" \
  -H "Accept-Encoding: gzip" \
  "http://localhost:3000/api/v1/fuel?brands=lexus&limit=2000&api_key=test-key-123" \
  -o /tmp/export.csv.gz

# Check size
ls -lh /tmp/export.csv.gz

# Decompress and verify
gunzip -c /tmp/export.csv.gz | head -5

# Test uncompressed rejection (should return 406)
curl -v -w "\nStatus: %{http_code}\n" \
  "http://localhost:3000/api/v1/fuel?brands=lexus&limit=2000&api_key=test-key-123" \
  -o /tmp/export.csv

# Check response
# Should be HTTP 406 if staged file > 50 MB
```

---

## Tasks

**Execution order:** Complete tasks in sequence. Tasks 1–2 are safe cost wins (gzip, CSV, refuse uncompressed). Tasks 3–4 are risky parallelism (concurrent ranges, global abort). Task 5 is tuning/measurement.

### 1. Add Reqwest Gzip Feature

- [ ] Update workspace.dependencies (Cargo.toml) to include `gzip` in reqwest features.
- [ ] Add `.gzip(true)` to client builder at crates/rdw-client/src/lib.rs:145-148.
- [ ] Run `cargo build --workspace` and verify no errors.
- [ ] Test: mock response with gzip encoding; verify decompression succeeds.

### 2. Implement CSV Parsing & Switch Datasets

- [ ] Create crates/rdw-core/src/csv_parser.rs module.
- [ ] Implement `parse_csv_page()` with:
  - CSV reader configured `flexible(false)`.
  - Header parsed into HashMap<&str, usize>.
  - Required column checks (kenteken, optionally brandstof_volgnummer).
  - Cell parsing as Value::String (no numeric inference).
  - Empty cell skipping (not stored in Map).
  - Truncation detection via csv::Error → retry + 502.
- [ ] Modify vehicle and fuel fetch functions to:
  - Detect response Content-Type or append `.csv` to Socrata URL.
  - Call csv_parser::parse_csv_page instead of serde_json.
  - Test with real Socrata response (limit=100, brands=lexus) against opendata.rdw.nl.
- [ ] Verify row count, kentekens, volgnummers match JSON path (byte-diff test).
- [ ] Run criterion B.1–B.6 tests; confirm pass.

### 3. Implement Uncompressed Response Rejection (Scope E)

- [ ] Add environment variable `UNCOMPRESSED_SIZE_THRESHOLD_MB` to handlers.rs (default 50).
- [ ] Before streaming uncompressed CSV: check staged file size.
- [ ] If exceeded: return 406 Not Acceptable with clear message.
- [ ] If allowed: log at warn level before streaming.
- [ ] Run criterion E.1–E.3 tests; confirm pass.

### 4. Implement Concurrent Range Fetching & Global Abort (Scope C)

- [ ] Define 64 fixed two-char range bands (or defer distribution caching to separate task).
- [ ] Spawn tokio::task::JoinSet to fetch ranges concurrently.
- [ ] Add global abort flag (AtomicBool or Mutex<bool>) checked after every fuel fetch.
- [ ] Track (fuel_fetches_attempted, fuel_fetch_failures) with shared atomics.
- [ ] Implement threshold logic: `failures > max(3, 0.10 * attempted)` → set abort flag, cancel JoinSet.
- [ ] Per-range: smaller page_size (default ~3000 rows, inversely scaled by concurrency).
- [ ] Call merge-join once per range; no per-range buffer accumulation.
- [ ] Run criterion C.1–C.4 tests; confirm pass.
- [ ] Integration: measure memory RSS on 16-concurrent Toyota export; verify bounded.

### 5. Update & Verify Acceptance Criteria

- [ ] Reconcile row count: before export, issue `$select=count(kenteken) where merk in(...)` → verify final row count matches (or is ≤ limit).
- [ ] Run criterion V.1–V.3 tests; confirm pass.
- [ ] Byte-diff small real export (limit=2000, brands=lexus) JSON path vs. CSV path.
- [ ] Measure wall-clock time for full Toyota export (824,620 vehicles).
- [ ] Document findings: ingress reduction ratio, wall-time change, any configuration re-tuning needed.

### 6. Correct ARCHITECTURE.md

- [ ] Line 37: change "sent with Content-Length (not streamed)" to "streamed via handlers.rs:268-285".
- [ ] Lines 26-27: correct fuel query description; remove stale "range pagination" wording.
- [ ] Lines 62-65: correct Vercel Functions limitation; note container runtime is used; local Docker is primary for full exports.

### 7. Update DEPLOYMENT.md

- [ ] Add note: "curl --compressed is required for large exports; uncompressed requests >50 MB return 406."
- [ ] Add note: "Fixed-IP or session affinity required for rate-limit quota to work correctly (per-process counters)."
- [ ] Add note: "Row order is non-deterministic when concurrency is enabled; output may differ from JSON sequential path."

### 8. Run Full Test Suite

- [ ] `cargo test --workspace -- --nocapture`
- [ ] Verify all criterion tests pass.
- [ ] Verify no clippy warnings.

### 9. Clean Up & Commit

- [ ] Review code for stale comments (lib.rs:265-274 doc comment, etc.).
- [ ] Ensure logs are informative (warn level on uncompressed streaming, debug level on abort decision, etc.).
- [ ] Run final verification: small real export (limit=2000, brands=lexus) against opendata.rdw.nl, both JSON and CSV paths, byte-diff.

---

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `UNCOMPRESSED_SIZE_THRESHOLD_MB` | 50 | Reject uncompressed responses above this size (MB) |
| `FUEL_CONCURRENCY` | 8 | Number of concurrent workers for fuel range fetches (tunable after measurement) |
| `FUEL_KENTEKEN_BATCH` | 800 | Number of unique plates per fuel batch request (tunable after RDW_APP_TOKEN confirmed) |
| `FUEL_PAGE_SIZE` | 3000 | Rows per page per concurrent range (scales inversely with FUEL_CONCURRENCY) |
| `RDW_APP_TOKEN` | (unset) | Server-side Socrata authentication; main.rs warns if unset |
| `RUST_LOG` | (unset) | Tracing filter; e.g., `debug`, `rdw_api=debug` for timing info |

---

## Assumptions

- Socrata API `/api/views/<id>.csv` endpoint accepts `$where`, `$order`, `$limit` parameters (verified for m9d7-ebf2 and 8ys7-d773).
- CSV format is stable; columns are ordered consistently in header (verified against /api/views/m9d7-ebf2.json).
- Row collation (byte-order) is consistent across pages; keyset pagination is deterministic (Socrata guarantee).
- Merge-join FuelInversion validation already exists and will catch ordering violations.
- Temp file cleanup logic already in place; files persist until handler finishes (verified in handlers.rs:232-240).
- AtomicBool and tokio::sync::Mutex are available for global abort flag (standard Tokio).
- Single 50k-row page footprint (~100 MB) is accurate baseline for memory scaling calculation.

---

## Verification Checklist (Post-Implementation)

- [ ] Gzip feature enabled and .gzip(true) explicit in code.
- [ ] CSV parsing: "007" stays "007", "1.50" stays "1.50"; volgnummer remains quoted string "1".
- [ ] Empty CSV cells skipped; row Map size < 98 (e.g., ~50 keys per typical row).
- [ ] CSV header resolved by name, not position; missing required columns rejected before merge-join.
- [ ] Truncated CSV body detected, retried, eventually 502 (not silent short page).
- [ ] 16 concurrent ranges: peak RSS < single-page peak + constant overhead.
- [ ] Global abort flag blocks all ranges when threshold exceeded; JoinSet cancelled; temp file cleaned.
- [ ] Row count reconciliation: $select=count() pre-export, verify final match (or ≤ limit).
- [ ] Byte-diff: CSV export (limit=2000, brands=lexus) matches JSON export row count, kentekens, volgnummers.
- [ ] Uncompressed >50 MB returns 406; <50 MB streams with warn-level log.
- [ ] Wall-clock time measured for full Toyota export; ingress bytes measured (target ≥ 20x reduction).
- [ ] All acceptance criteria tests pass: `cargo test --workspace --no-fail-fast`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes (no warnings).
- [ ] ARCHITECTURE.md corrected (line 37, 26-27, 62-65).

---

## Open Questions

None.

---

## Record: User Acceptance

On 2026-09-08, user reviewed the complete planning document, all five scope items (A–E), advisor-lite findings, measured facts, and acceptance bar. User explicitly approved:

- Scope A: Gzip feature enablement (cost lever, not speed lever).
- Scope B: CSV parsing (1.5x volume reduction after gzip).
- Scope C: Concurrent, unordered kenteken ranges (16 workers, ~3k rows per page, global abort).
- Scope D: Measurement-driven re-tuning only after RDW_APP_TOKEN confirmation.
- Scope E: Refuse uncompressed responses >50 MB with 406; environment-overridable threshold.
- All 19 advisor-lite findings (1–19) integrated as acceptance criteria or checked/closed analysis.
- Known & Accepted: Per-process rate limiter multiplication by Vercel container count (documented, deferred).

No further confirmation rounds are required.

---

## Next Steps

1. Proceed with **Task 1** (add gzip feature).
2. Complete tasks 2–3 (CSV parsing, refuse uncompressed) before parallelism (task 4).
3. Implement task 4 (concurrent ranges + global abort) only after CSV foundation is solid and tested.
4. Task 5: Run real export measurements (ingress, wall time, memory).
5. Task 6–7: Correct documentation, update DEPLOYMENT.md.
6. Task 8: Full test suite pass.
7. Task 9: Commit when ready; push to git when developer is ready.

---

## Appendix: Cost Calculation

**Before (JSON, single-threaded):**
- ~1.7 GB vehicle JSON + ~0.9 GB fuel JSON = ~2.6 GB ingress.
- Vercel runtime cost: active CPU + memory × wall time + network egress.
- Network: ~2.6 GB × $0.20/GB (estimate) = ~$0.52 + compute overhead → ~$1.00 per request.

**After (CSV + gzip, 16-concurrent):**
- Gzip reduction: ~2.6 GB → ~130 MB (~20x).
- CSV reduction: additional ~1.5x on top (embedded in gzip estimate above).
- Network: ~130 MB × $0.20/GB = ~$0.026 + compute overhead → ~$0.05 per request.
- **Cost reduction: ~20x** (user saves ~€0.95 per Toyota export).

**Wall time:**
- Before: single 50k-row page = 6.06s; 17 pages × 6.06s = ~103s + merge/CSV assembly ≈ 120s total.
- After: 16 concurrent ranges, measured in implementation task 5. Target: wall time reduction proportional to parallelism (best case ~8x for 16 workers, realistic ~5–8x due to contention).

