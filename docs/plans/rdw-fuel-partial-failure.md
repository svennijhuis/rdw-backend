# Plan: RDW Fuel Partial Failure & Pagination Bug Fix

**Status:** Ready for Development  
**Acceptance:** User explicitly approved all recommendations (2026-09-07, "Yes all fine")  
**Frontier:** Empty — no open questions  

---

## Business Objective

Fix a critical live bug in fuel data pagination that silently discards ~89% of fuel records, and replace the "never deliver partial CSV" rule with a controlled degradation strategy: deliver a partial CSV when fuel enrichment fails, and mark the failure unmistakably via three layers (status column, response header, ZIP report). Maintain the absolute abort rule for vehicle-page fetch failure (unknown rows = silent gap with no marker). Rate-limit quota is consumed on partial success only; true upstream failure (502/504) releases it.

---

## Acceptance Criteria & Test Plan Matrix

Each criterion below has an observable pass/fail condition and a test matrix row (happy path, edge case, failure case, test type).

| # | Business Criterion | Happy Path | Edge Case | Failure Case | Test Type | Verifiable By |
|---|---|---|---|---|---|---|
| 1 | **LIVE BUG FIX: Fuel range fetch is keyset-paginated by kenteken** | Fuel range covering 472,130 rows in 8ys7-d773 (kenteken 0001VH..11STK5) is fetched in full across multiple pages, ordered by kenteken then brandstof_volgnummer; merge_join receives all rows; export rows match input row count | Last page of fuel range ends mid-volgnummer group for a kenteken; pagination boundary respects kenteken boundary only, never splits within volgnummer group | Mock Socrata fuel range returns exactly 50,000 rows (full limit) on first two pages, 22,130 on third; service continues paging until short page received; validator confirms all 472,130 rows in output | Unit + Integration | Unit test: pagination logic with mock returning full-limit pages followed by short page, assert all rows collected; Integration test: known-size fuel range (>50k rows) fetched completely, row count matches |
| 2 | **Partial CSV allowed on fuel failure; status column added (last position, index 203)** | Vehicle A with fuel data present and fuel1_*, fuel2_*, fuel3_* populated; export_status column shows `ok`; CSV width is 204 columns (98 vehicle + 3*35 fuel + 1 status) | Vehicle B in same range where fuel fetch succeeded but no fuel rows exist in 8ys7-d773; export_status shows `no_fuel_data` and fuel1_/fuel2_/fuel3_* remain blank; positional width preserved | Vehicle C in range where fuel fetch failed; export_status shows `fuel_unavailable` and fuel1_/fuel2_/fuel3_* remain blank; RowWidener integration and widen.rs tests assert row[2], row[6], and final row[203] without breaking | Unit + Integration | Unit test: widen() produces row with status column at index 203; Integration test: export with mixed status values; test that RowWidener positional assertions still pass; test that existing consumers see 204-wide CSV |
| 3 | **Failure vocabulary: three status values** | Fuel rows successfully returned for vehicle → `ok` | Fetch succeeded, no rows found in 8ys7-d773 → `no_fuel_data` (common orphan case) | Fetch failed with 5xx/timeout → `fuel_unavailable` | Unit | Unit test: merge_join sets correct status based on (fetch_success, rows_present); test all three paths with mock data |
| 4 | **Proportional failure threshold: abort when failures > max(3, 0.10 * fuel_fetches_attempted)** | 5 fuel pages attempted, 1 fails → 0.20 ratio > 0.10, exceeds threshold (abort, 502) | 500 fuel pages attempted, 30 fail → 0.06 ratio < 0.10 but > 3 floor (abort, 502); 500 pages, 2 fail → 0.004 ratio < 0.10 and < 3 floor (continue, partial CSV with status markers) | 3 fuel pages attempted, all fail → 1.0 ratio exceeds both floor (3) and ratio (0.10); abort immediately, 502, no CSV | Unit + Integration | Unit test: counter logic with env vars FUEL_FAILURE_FLOOR and FUEL_FAILURE_RATIO (default 3 and 0.10); test threshold crossing; Integration: mock multiple fuel-page failures and verify abort vs. partial decision |
| 5 | **HTTP 200 with partial CSV when fuel fails below threshold** | Export with 500 vehicle pages, 2 fuel pages fail; failures (0.004) < floor; response 200, CSV sent, vehicles have mixed status (ok/no_fuel_data/fuel_unavailable) | Export with only 1 vehicle page (5 fuel fetches), 1 fails (0.20 > 0.10); response 200, partial CSV sent | Request returns 502 because failures >= threshold (not applicable to 200 criterion, but verified in separate test) | Integration | Integration test: mock scenario with failures below threshold, assert HTTP 200 and CSV bytes sent; verify merge_join includes failed ranges with status markers |
| 6 | **X-Export-Warnings response header names fuel failure count and affected vehicle count** | Response includes `X-Export-Warnings: fuel_failures=2 vehicles_affected=47` (2 ranges failed, 47 vehicles in those ranges) | Zero fuel failures → header absent or header `X-Export-Warnings: fuel_failures=0 vehicles_affected=0` (clarify format) | Multiple failures, header scales to `X-Export-Warnings: fuel_failures=12 vehicles_affected=1241` | Integration | Integration test: mock multiple fuel-page failures, capture response headers, assert count accuracy |
| 7 | **Content-Disposition filename marked PARTIAL when any fuel failure occurred** | No fuel failures → `Content-Disposition: attachment; filename="fuel-export.csv"` | At least 1 fuel failure below threshold → `Content-Disposition: attachment; filename="fuel-export-PARTIAL.csv"` (user sees in browser download) | All failures above threshold → 502 response, no Content-Disposition sent | Integration | Integration test: mock failure scenario, assert filename contains PARTIAL; test no-failure scenario, assert filename clean; verify browser download displays correct filename |
| 8 | **_EXPORT_REPORT.txt in ZIP when any fuel failure occurred** | Single CSV export (no ZIP) → no report file | ZIP export with 0 fuel failures → ZIP contains CSV parts + _EXPORT_REPORT.txt with "All fuel data successfully fetched" | ZIP export with failures → _EXPORT_REPORT.txt lists each kenteken range that failed and affected vehicle count; e.g., "Range 0001VH-5000VH: 47 vehicles, fetch failed on 2025-09-07T14:32:15Z" | Integration | Integration test: mock ZIP-trigger scenario with fuel failures, extract ZIP, verify _EXPORT_REPORT.txt content; test no-failure ZIP, verify report exists and states success |
| 9 | **Rate-limit quota consumed on partial success (HTTP 200); released on true failure (502/504)** | Partial CSV returned (200), fuel failures below threshold → quota decremented (user burns one request) | Full CSV returned (200), zero fuel failures → quota decremented | Vehicle page fails (502/504) → quota NOT decremented, request does not count against 3/day + 5/week | Integration | Integration test: submit requests that result in 200 (partial), 200 (full), and 502; check rate-limit counter after each; verify quota consumed only on 200, not on 502 |
| 10 | **Fuel range fetch retries on 5xx/timeout/429 (5 attempts, exponential backoff) before marking range as failed** | First attempt times out, retry succeeds on attempt 2 → range succeeds, status `ok` for vehicles in range | All 5 attempts fail with 502 → range marked as failed, status `fuel_unavailable` for all vehicles in range | 4th attempt succeeds after 3 timeouts → range succeeds, full rows returned | Integration | Integration test: mock fuel-range endpoint with transient failures (timeouts, 502s), verify retry logic executes and succeeds/fails correctly; monitor backoff delays |
| 11 | **Existing criterion 5 from rdw-fuel-csv-api.md is rewritten to allow partial CSV** | Criterion 5 now reads: "Partial CSV delivered when fuel enrichment fails below threshold; status markers clarify unavailable data. True failure → 502." | Test `failure_persistent_upstream_500_returns_502_not_partial_csv` in crates/rdw-api/tests/fuel_endpoint.rs updated: vehicle-page failure still returns 502 with no CSV (absolute abort rule); fuel-page failure that exceeds threshold also returns 502 | Criterion 5 test updated to verify partial CSV when fuel failure below threshold | Unit + Integration | Re-run existing test suite in fuel_endpoint.rs with updated assertions; verify old test name still reflects vehicle-page abort, new test covers partial-CSV path |
| 12 | **All text English; RDW field names remain Dutch; failure markers unmistakable without color** | CSV column `export_status` has values `ok`, `no_fuel_data`, `fuel_unavailable` (English, unambiguous) | Response header `X-Export-Warnings`, filename `fuel-export-PARTIAL.csv`, report `_EXPORT_REPORT.txt` all English | All-caps PARTIAL in filename and all-caps in report title ensure visibility without requiring color | Integration | Integration test: verify all headers, column names, file names, and report content in English; user reading CSV without color can distinguish statuses |

---

## Settled Decisions (User Confirmed 2026-09-07, "Yes all fine")

| Decision | Rationale | Risk/Tradeoff |
|---|---|---|
| **Criterion 1: Fuel pagination bug is PRIORITY** | 472,130 fuel rows in kenteken range 0001VH..11STK5 silently discarded on live deployment; ~89% data loss. Proven on first 50k-vehicle page of Toyota/Lexus/Suzuki. Escaped testing because wiremock mocks never return full 50k rows and smoke test used limit=5. Must fix first. | Requires keyset pagination implementation; split only on kenteken boundary, never mid-volgnummer group. merge_join validates order; FuelInversion would hard-fail if violated. |
| **New rule: Partial CSV allowed on fuel failure** | Replaces absolute "never deliver partial" with: "deliver partial only when fuel enrichment failed, and mark it unmistakably." Vehicle-page abort rule unchanged (zero tolerance); fuel-page degrades. Rationale: a failed fuel range already has vehicles in hand (no silent gap); marking status clarifies to user which rows are degraded. | User may download and use partial export without noticing status column; quota is consumed (3 daily requests). Contractual obligation to test and document the column position and values. |
| **Vehicle-page failure still aborts (HTTP 502, zero CSV)** | Vehicle page = up to 50,000 cars never fetched. No row to mark; only alternative is silent gap. User requirement: zero tolerance. | Export terminates immediately; in-flight temp file removed; rate-limit quota released. |
| **Fuel-page failure degrades (HTTP 200, partial CSV)** | Vehicles in kenteken range are already in hand; emitted with status `fuel_unavailable` and fuel1_/fuel2_/fuel3_* blank. Threshold prevents "fire hose" of failures. | Requires new failure counter and proportional logic; test must cover both floor and ratio. |
| **Three-layer failure marking without color** | Layer 1: `export_status` column (index 203, last position) with values `ok`, `no_fuel_data`, `fuel_unavailable`. Layer 2: Response header `X-Export-Warnings: fuel_failures=N vehicles_affected=M`. Layer 3: ZIP report `_EXPORT_REPORT.txt` listing failed ranges. No color used (browser download never surfaces headers to human; Excel does not render color reliably across users). | Column position is final to preserve RowWidener compatibility; adds one column to existing 203. Header and report are supplementary for API clients and archive users. |
| **Status vocabulary: THREE values, not two** | `ok` (rows returned), `no_fuel_data` (fetch succeeded, range genuinely has no rows), `fuel_unavailable` (fetch failed). Two-value design is ambiguous: blank fuel is indistinguishable from orphan. | Requires merge_join to distinguish success + empty from failure. Advisor confirmed existing code already does this via per-range fetch-failed flag. |
| **Proportional threshold: max(3, 0.10 * fuel_fetches_attempted)** | Floor of 3 prevents single-page export failing at 100% ratio (3 fetches, 1 fails). Ratio 0.10 (10%) prevents degradation when a few ranges fail during large export (500 fetches, 30 fail = 0.06 < 0.10 → continue). Both configurable by env var. | Requires counter and calculation logic; test must verify both floor and ratio crossing. |
| **Proportional threshold uses ATTEMPTED, not COMPLETED fetches** | Attempted = fetches initiated (including retried ones). Clearer semantics: a fetch that times out 5 times still counts as 1 attempted fetch; user sees cumulative picture. | Alternative is "unique ranges queried"; implementation simpler with attempted count. |
| **HTTP 200 on partial success; quota consumed** | Partial CSV is a valid response; user explicitly requested export. Burning one of 3 daily quota is acceptable price for degraded data (user chooses to use it). | High-cost consequence for user who discards partial file; documented in plan and release notes. Rate-limit design keeps quota conservative (3/day, 5/week). |
| **True upstream failure (502/504) still releases quota** | Vehicle-page 502 or fuel-page 502 that exceeds threshold → return 502, release quota. Rationale: request did not complete; should not consume user's daily allowance. | Imbalance: partial success consumes, true failure releases. User aware via documentation. |
| **Content-Disposition filename PARTIAL marking** | Browser download UI shows filename; user sees `fuel-export-PARTIAL.csv` immediately without reading headers. X-Export-Warnings header does not surface in browser. | API clients must check both header AND filename (or ignore filename if header is present). Clear guidance in API docs required. |
| **ZIP report _EXPORT_REPORT.txt required when any failure** | ZIP output already packages multiple files; report is natural addition. Users archiving export can reference report later. CSV-only exports use filename + header instead. | ZIP with zero failures still includes report (states success); slightly larger ZIP but no ambiguity. |
| **All text English** | User requirement; consistency with codebase. RDW's Dutch field names (e.g., `brandstof_volgnummer`, `merk`) remain as upstream. | Developers must recognize both English identifiers and Dutch column names. |

---

## Technical Changes Required

### 1. Fuel Pagination Bug Fix (crates/rdw-client/src/lib.rs)

**Current Issue (lines 179–186):**
```rust
fn fuel_range_url(&self, from: &str, to: &str) -> String {
    format!(
        "{}/8ys7-d773.json?$where=kenteken>='{}' AND kenteken<='{}'&$order=kenteken,brandstof_volgnummer&$limit=50000",
        self.base, from, to
    )
}
```
Builds URL with single `$limit=50000`, no pagination. When result set is larger than 50,000 rows, Socrata returns only the first 50,000; service never fetches remaining rows.

**Fix:** Implement keyset pagination for fuel range fetch.
- Add pagination state: track `last_kenteken` and `last_volgnummer` from previous page.
- Modify `fuel_range_url()` to accept optional `after_kenteken` and `after_volgnummer` parameters.
- Modify `fetch_fuel_range()` to loop: fetch page, parse results, if page is at full limit (50,000 rows), continue pagination by updating cursor, else return.
- Update `$where` clause to include `AND (kenteken > '<after_kenteken>' OR (kenteken = '<after_kenteken>' AND brandstof_volgnummer > <after_volgnummer>))`.
- Never split a kenteken group: once `kenteken` changes, that is a safe boundary.
- Validate sort order in returned rows; if out of order, fail loudly (FuelInversion already exists in merge_join).
- Return complete Vec<FuelRow> spanning all pages in the range.

### 2. Partial CSV Support (crates/rdw-api/src/pipeline.rs, crates/rdw-core/src/merge.rs, crates/rdw-core/src/widen.rs)

**Changes to pipeline.rs (fetch_and_widen loop):**
- Track fuel-page failures: add counter `fuel_fetch_failures` and `fuel_fetches_attempted`.
- For each kenteken range: call `fetch_fuel_range()` with retry logic; if all 5 retries fail, log failure, increment counter, call `merge_join(page, &[])` (empty fuel vec) to emit vehicles with `fuel_unavailable` status.
- After each range: check threshold: `if failures > max(floor, ratio * attempted) { return Err(FailureLimitExceeded) }`.
- If threshold exceeded, abort (return 502 via handler); if not, continue to next range and accumulate partial rows.
- Pass failure metadata to assembler: (failure_count, affected_vehicle_count, list of failed ranges with boundaries).

**Changes to merge.rs (merge_join):**
- Signature extends to accept per-range fetch-failed flag: `merge_join(vehicle_page: &[VehicleRow], fuel_page: &[FuelRow], fuel_fetch_failed: bool) -> Vec<WidenedRow>`.
- For each vehicle in page:
  - If fuel_fetch_failed: set status to `fuel_unavailable`.
  - Else if no matching fuel rows: set status to `no_fuel_data`.
  - Else if matching fuel rows present: set status to `ok`.
- Continue widening with status column; validate order as before.

**Changes to widen.rs (RowWidener):**
- Add `export_status: String` field to WidenedRow.
- Modify `widen()` to append status as final column (index 203).
- Modify `header()` to append `export_status` as final column name.
- Verify positional assertions in tests: `assert_eq!(row[2], ...)`, `assert_eq!(row[6], ...)` remain unchanged (status is added at end).

### 3. Failure Thresholding (crates/rdw-core/src/config.rs or env parsing)

- Add env vars: `FUEL_FAILURE_FLOOR` (default 3), `FUEL_FAILURE_RATIO` (default 0.10).
- Parse at startup; pass to pipeline as config.
- Implement threshold check: `fn should_abort_fuel_failures(failures: usize, attempted: usize, floor: usize, ratio: f64) -> bool { failures > floor || (attempted > 0 && (failures as f64 / attempted as f64) > ratio) }`.

### 4. HTTP Response Headers & Filename (crates/rdw-api/src/handlers.rs)

**Changes to handler response assembly:**
- After pipeline completes (success or partial), check failure metadata.
- If failures > 0:
  - Set `Content-Disposition: attachment; filename="fuel-export-PARTIAL.csv"` (or `.zip` if zipped).
  - Set `X-Export-Warnings: fuel_failures=<count> vehicles_affected=<count>`.
- Else:
  - Set `Content-Disposition: attachment; filename="fuel-export.csv"`.
  - Omit `X-Export-Warnings` (or set to zero).

### 5. ZIP Report File (crates/rdw-core/src/csv_writer.rs)

**Changes to Assembler:**
- Add method `finish_with_report(failure_metadata)` that, if output is ZIP, appends entry `_EXPORT_REPORT.txt` with summary:
  ```
  Export Report
  Generated: 2026-09-07T14:32:15Z
  
  Fuel Data Fetch Results:
  - Successful ranges: 498 of 500
  - Failed ranges: 2
  - Vehicles affected by failures: 47
  
  Failed Ranges:
  - Range 0001VH to 5000VH: fetch failed, 47 vehicles (cause: HTTP 502)
  - Range ...
  
  Status column in CSV: export_status (values: ok, no_fuel_data, fuel_unavailable)
  ```
- If zero failures, report states: "All fuel data successfully fetched."
- For CSV-only output (no ZIP), no report file needed (user relies on filename + header).

### 6. Rate-Limit Quota Logic (crates/rdw-core/src/rate_limit.rs)

**Changes to handler quota release:**
- On 200 (partial or full): DO NOT release quota; count as used.
- On 502/504 (vehicle-page abort or fuel-failure threshold exceeded): release quota (do not count).
- Existing 429 (quota exhausted) path unchanged.

### 7. Test Updates (crates/rdw-api/tests/fuel_endpoint.rs)

**Update existing criterion 5 test:**
- Rename `failure_persistent_upstream_500_returns_502_not_partial_csv` to clarify: "vehicle_page_failure_returns_502_not_partial_csv".
- Add new test: "fuel_page_failure_below_threshold_returns_200_partial_csv".
- Add test: "fuel_page_failure_above_threshold_returns_502".
- Add test: "multiple_fuel_pages_with_varied_failures_status_column_marked".

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

# Build release binary
cargo build --release
```

### Test — Criterion 1 (Pagination Bug Fix)

```bash
# Run fuel pagination tests
cargo test -p rdw-client test_fuel_range_pagination -- --nocapture

# Run merge-join with full fuel data
cargo test -p rdw-core test_merge_join_full_fuel_range -- --nocapture
```

### Test — Criterion 2–4 (Status Column & Proportional Threshold)

```bash
# Status column position and vocabulary
cargo test -p rdw-core test_widen_exports_status_column -- --nocapture
cargo test -p rdw-core test_status_vocabulary_ok_no_fuel_unavailable -- --nocapture

# Threshold logic
cargo test -p rdw-core test_proportional_failure_threshold_floor -- --nocapture
cargo test -p rdw-core test_proportional_failure_threshold_ratio -- --nocapture
```

### Test — Criterion 5–9 (HTTP 200 Partial, Headers, Filename, Report)

```bash
# Partial CSV on fuel failure below threshold
cargo test -p rdw-api test_fuel_page_failure_below_threshold_returns_200_partial_csv -- --nocapture

# Response headers
cargo test -p rdw-api test_response_header_x_export_warnings_counts_failures -- --nocapture

# Content-Disposition filename
cargo test -p rdw-api test_content_disposition_partial_filename_when_failure -- --nocapture

# ZIP report
cargo test -p rdw-api test_zip_export_report_lists_failed_ranges -- --nocapture

# Quota consumption
cargo test -p rdw-api test_rate_limit_quota_consumed_on_200_partial -- --nocapture
cargo test -p rdw-api test_rate_limit_quota_released_on_502 -- --nocapture
```

### Test — Criterion 10–11 (Retry & Criterion 5 Update)

```bash
# Fuel range retry logic
cargo test -p rdw-client test_fuel_range_retry_on_5xx_timeout_429 -- --nocapture

# Updated criterion 5 tests
cargo test -p rdw-api test_vehicle_page_failure_returns_502_not_partial_csv -- --nocapture
cargo test fuel_endpoint -- --nocapture
```

### Test — All Criteria

```bash
# Run full test suite with no early exit
cargo test --workspace -- --nocapture

# Or test specific module
cargo test -p rdw-api -p rdw-core -p rdw-client -- --nocapture
```

### Integration & Smoke

```bash
# Start service locally
cargo run -p rdw-api

# In another terminal, test a request with fuel failures (requires mock or live RDW)
export VALID_API_KEYS="test-key-123"
export RDW_APP_TOKEN="test-app-token"
export FUEL_FAILURE_FLOOR="3"
export FUEL_FAILURE_RATIO="0.10"

# Test partial CSV scenario (mocked by integration test)
curl -v "http://localhost:3000/api/v1/fuel?brands=toyota&limit=100&api_key=test-key-123" \
  | head -5 | cut -d, -f1-10,203

# Check response headers
curl -i "http://localhost:3000/api/v1/fuel?brands=toyota&limit=100&api_key=test-key-123" \
  | grep -E "X-Export-Warnings|Content-Disposition"
```

---

## Tasks

**Execution order:** Complete tasks 1–10 in sequence. All tasks are required for full plan completion.

### 1. Fix Fuel Pagination Bug (crates/rdw-client/src/lib.rs)

Implement keyset pagination for `fetch_fuel_range()`:
- Modify `fuel_range_url()` to accept `after_kenteken` and `after_volgnummer` parameters.
- Add loop in `fetch_fuel_range()`: fetch page, check if page is full (50,000 rows), if so, extract last kenteken/volgnummer and request next page.
- Update `$where` clause to conditionally include cursor.
- Validate sort order in returned rows; fail if inversion detected.
- Return complete Vec<FuelRow> with all rows across all pages.
- Unit test: mock Socrata returning full-limit pages followed by short page; verify all rows collected and boundaries at kenteken changes.

### 2. Extend merge_join Signature (crates/rdw-core/src/merge.rs)

- Add `fuel_fetch_failed: bool` parameter to `merge_join()`.
- Implement status logic: if `fuel_fetch_failed`, all vehicles in page get `fuel_unavailable`; else use existing orphan + rows logic to set `ok` or `no_fuel_data`.
- Unit test: cover all three status paths with mock data.

### 3. Add Status Column to WidenedRow & Header (crates/rdw-core/src/widen.rs)

- Add `export_status: String` field to WidenedRow.
- Modify `widen()` to populate status (from merge_join caller).
- Modify `header()` to append `export_status` column name.
- Verify that column is placed at final position (index 203) and does not break positional assertions in existing tests.
- Unit test: assert row[203] is status value; assert header includes new column.

### 4. Implement Proportional Failure Threshold (crates/rdw-core/src/config.rs or new module)

- Add struct `FailureConfig { floor: usize, ratio: f64 }`.
- Implement `FailureConfig::from_env()` reading `FUEL_FAILURE_FLOOR` and `FUEL_FAILURE_RATIO` with defaults (3, 0.10).
- Implement `should_abort(failures: usize, attempted: usize, config: &FailureConfig) -> bool`.
- Unit test: verify floor-only, ratio-only, and combined logic with multiple scenarios.

### 5. Update Pipeline to Track & Check Failure Threshold (crates/rdw-api/src/pipeline.rs)

- Modify `fetch_and_widen` loop to:
  - Initialize `fuel_fetch_failures = 0`, `fuel_fetches_attempted = 0`, `failed_ranges: Vec<...>` (boundaries and counts).
  - For each kenteken range: increment `fuel_fetches_attempted`; call `fetch_fuel_range()` with retry logic.
  - On fetch failure (all 5 retries exhausted): increment `fuel_fetch_failures`, record range boundaries and affected vehicle count, call `merge_join(page, &[], fuel_fetch_failed=true)`.
  - On fetch success: call `merge_join(page, fuel_rows, fuel_fetch_failed=false)`.
  - After each merge_join, check threshold: `if should_abort(failures, attempted, config) { return Err(...) }`.
  - Accumulate partial rows and failure metadata.
- Return `(rows, failure_metadata)` on success.
- On abort: return error; handler will return 502 and release quota.
- Integration test: mock multiple fuel-page failures and verify abort vs. partial decision.

### 6. Add Failure Metadata to Response (crates/rdw-api/src/handlers.rs)

- Extend handler to receive failure metadata from pipeline.
- If failures > 0:
  - Set `Content-Disposition: attachment; filename="fuel-export-PARTIAL.csv"` (or `.zip`).
  - Set `X-Export-Warnings: fuel_failures=<count> vehicles_affected=<count>`.
- Else:
  - Set `Content-Disposition: attachment; filename="fuel-export.csv"`.
  - Omit warning header.
- Integration test: verify headers are set correctly.

### 7. Implement ZIP Report (_EXPORT_REPORT.txt) (crates/rdw-core/src/csv_writer.rs)

- Extend Assembler with method `finish_with_report(failure_metadata)`.
- If output is ZIP and failure_metadata is present:
  - Generate report text with timestamp, summary counts, failed ranges list, and status column explanation.
  - Append report as `_EXPORT_REPORT.txt` entry to ZIP.
- If zero failures: report states "All fuel data successfully fetched."
- For CSV-only exports (not zipped): no report (user relies on filename + header).
- Integration test: mock ZIP-trigger scenario with failures; extract ZIP; verify report content.

### 8. Update Rate-Limit Quota Logic (crates/rdw-api/src/handlers.rs)

- On HTTP 200 (partial or full): do NOT release quota; count as used.
- On HTTP 502/504: release quota (do not count).
- Existing 429 path unchanged.
- Integration test: verify quota consumed/released correctly for each status code.

### 9. Update & Add Tests (crates/rdw-api/tests/fuel_endpoint.rs, crates/rdw-core/tests/*)

**Updates to existing tests:**
- Rename and clarify `failure_persistent_upstream_500_returns_502_not_partial_csv` to emphasize vehicle-page abort rule.
- Update assertion to confirm status is still 502 (not 200 with partial CSV).

**New tests:**
- `test_fuel_page_failure_below_threshold_returns_200_partial_csv`: mock 1 fuel-page failure out of 5 attempts; assert HTTP 200, CSV sent, status column shows `fuel_unavailable` for that range.
- `test_fuel_page_failure_above_threshold_returns_502`: mock multiple fuel-page failures exceeding ratio; assert HTTP 502, no CSV sent.
- `test_multiple_fuel_pages_with_varied_failures_status_column_marked`: mixed success/failure/no-data scenarios; verify all status values present.
- `test_response_header_x_export_warnings_counts_failures`: assert header format and count accuracy.
- `test_content_disposition_partial_filename_when_failure`: assert filename contains PARTIAL when failures > 0.
- `test_zip_export_report_lists_failed_ranges`: mock ZIP scenario with failures; extract and verify report content.
- `test_rate_limit_quota_consumed_on_200_partial`: submit request that returns 200 with partial CSV; verify quota consumed.
- `test_rate_limit_quota_released_on_502`: submit request that returns 502; verify quota released.
- `test_proportional_threshold_floor_and_ratio`: unit test covering all threshold scenarios.
- `test_fuel_pagination_full_large_range`: integration test with >50k-row fuel range; verify all rows fetched.

### 10. Verify Criterion 5 in Existing Plan (docs/plans/rdw-fuel-csv-api.md)

- Open existing plan at criterion 5.
- Update criterion text to: "Partial CSV delivered when fuel enrichment fails below threshold; status markers clarify unavailable data. Vehicle-page failure still returns 502."
- Update test name in assertion to point to new tests.
- Verify all 12 criteria still pass; re-run `cargo test --workspace`.
- No new plan document needed; modification is in-place in existing plan.

---

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `FUEL_FAILURE_FLOOR` | 3 | Minimum absolute failure count before abort |
| `FUEL_FAILURE_RATIO` | 0.10 | Maximum failure ratio (0.10 = 10%) before abort |
| `VALID_API_KEYS` | (required) | Comma-separated API keys for authentication |
| `RDW_APP_TOKEN` | (optional) | Server-side Socrata app token for higher rate limit |
| `RUST_LOG` | (optional) | Tracing filter; e.g., `debug`, `rdw_api=debug` |

---

## Assumptions

- Existing rdw-fuel-csv-api.md plan remains valid; this plan adds/modifies criteria and tasks, not replacing entire foundation.
- Fuel dataset (8ys7-d773) continues to have all kenteken entries sorted by (kenteken, brandstof_volgnummer); keyset pagination relies on this invariant.
- Merge-join FuelInversion validation already exists and will catch order violations.
- Rate-limit counter (fixed-window per key) is already implemented and can be extended to track partial success vs. abort.
- Temp file cleanup logic is already in place; temp files persist until handler finishes.
- RowWidener tests already use positional assertions (row[2], row[6], etc.); adding column at end does not break them.

---

## Verification Checklist (Post-Implementation)

- [ ] Pagination test: mock returns full limit on pages 1–2, short page on 3; verify 3 calls made and all rows collected.
- [ ] Status column appears at index 203 in all export rows.
- [ ] Status values are exactly `ok`, `no_fuel_data`, `fuel_unavailable` (no typos, no variations).
- [ ] Threshold floor of 3: single page (5 fetches) with 3+ failures returns 502; 2 failures returns 200 partial.
- [ ] Threshold ratio 0.10: 500 fetches with 30 failures (0.06) returns 200; 50 failures (0.10) border case; 51 failures (0.102) returns 502.
- [ ] Partial CSV has Content-Disposition filename `fuel-export-PARTIAL.csv`.
- [ ] ZIP export includes `_EXPORT_REPORT.txt` with failed range details.
- [ ] X-Export-Warnings header format matches spec: `fuel_failures=N vehicles_affected=M`.
- [ ] HTTP 200 on partial success; quota consumed (verify rate-limit counter incremented).
- [ ] HTTP 502 on true upstream failure; quota released (verify counter unchanged).
- [ ] All existing tests pass; criterion 5 test updated and passes.
- [ ] `cargo test --workspace` returns zero failures.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` returns zero warnings.

---

## Open Questions

None.

---

## Record: User Acceptance

On 2026-09-07, user reviewed all nine recommendations and explicitly answered: **"Yes all fine."**

This constitutes explicit acceptance of:
1. Fuel pagination bug as PRIORITY 1.
2. Partial CSV allowed on fuel failure (replaces "never partial" rule).
3. Vehicle-page failure still aborts (zero tolerance).
4. Proportional failure threshold (max(3, 0.10 * attempted)).
5. Status vocabulary (ok, no_fuel_data, fuel_unavailable).
6. Three-layer failure marking (status column, X-Export-Warnings, ZIP report).
7. HTTP 200 on partial success; quota consumed; 502 releases quota.
8. Content-Disposition PARTIAL filename marking.
9. All text English; no color; RDW field names remain Dutch.

No further confirmation rounds are required.

---

## Next Steps

1. Implement tasks 1–10 in sequence.
2. Run verification commands after each task.
3. Run final test suite: `cargo test --workspace --no-fail-fast`.
4. Verify all acceptance criteria pass.
5. Push code to git when developer is ready.
6. Deploy to production via Vercel or local Docker as needed.

