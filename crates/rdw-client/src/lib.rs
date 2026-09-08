//! HTTP client for the RDW open-data Socrata API.
//!
//! Provides keyset-paginated fetches of the vehicle and fuel datasets, with
//! bounded retry-with-backoff on transient failures. Never uses `$offset`
//! pagination: Socrata does not guarantee stable ordering across offset
//! pages, so all fetches page by `kenteken` (the vehicle plate) instead.

use std::time::Duration;

mod csv_parse;

use rand::Rng;
use serde_json::{Map, Value};

/// Vehicle dataset id (`gekentekende_voertuigen`).
pub const VEHICLE_DATASET_ID: &str = "m9d7-ebf2";
/// Fuel dataset id (`brandstof`).
pub const FUEL_DATASET_ID: &str = "8ys7-d773";

/// Socrata's own per-request row cap. A fuel kenteken range spanning three
/// whole brands regularly holds far more rows than this, so
/// `fetch_fuel_range` keyset-paginates by `(kenteken, brandstof_volgnummer)`
/// rather than trusting a single `$limit` request to return everything.
pub const DEFAULT_FUEL_PAGE_LIMIT: u32 = 50_000;

const SOCRATA_BASE: &str = "https://opendata.rdw.nl/resource";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Retry policy: up to `max_attempts` tries, exponential backoff from
/// `initial_backoff` capped at `max_backoff`, plus jitter. Injectable so
/// tests can exercise the retry *decision* without real wall-clock sleeps.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_millis(4000),
        }
    }
}

/// A single vehicle row from the Socrata `m9d7-ebf2` dataset, kept as a raw
/// JSON object so the client does not need to hardcode all ~98 columns.
#[derive(Debug, Clone, PartialEq)]
pub struct VehicleRow(pub Map<String, Value>);

impl VehicleRow {
    pub fn kenteken(&self) -> Option<&str> {
        self.0.get("kenteken").and_then(Value::as_str)
    }
}

/// A single fuel row from the Socrata `8ys7-d773` dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct FuelRow(pub Map<String, Value>);

impl FuelRow {
    pub fn kenteken(&self) -> Option<&str> {
        self.0.get("kenteken").and_then(Value::as_str)
    }

    /// The 1-based sequence number of this fuel entry for its vehicle.
    /// Socrata returns this as a numeric string (`brandstof_volgnummer`).
    pub fn volgnummer(&self) -> Option<u32> {
        self.0
            .get("brandstof_volgnummer")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
    }
}

/// Errors returned by the RDW client. Retryable variants are the ones that
/// `with_retry` will attempt again: request timeout, 5xx, and 429.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request to RDW timed out")]
    Timeout,
    #[error("RDW returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("failed to decode RDW response: {0}")]
    Decode(String),
    #[error("RDW request failed: {0}")]
    Transport(String),
    #[error("retries exhausted after {attempts} attempts: {last}")]
    RetriesExhausted { attempts: u32, last: String },
}

impl ClientError {
    fn is_retryable(&self) -> bool {
        match self {
            ClientError::Timeout => true,
            ClientError::Http { status, .. } => is_retryable_status(*status),
            // A body-read/decode failure while streaming or ungzipping the
            // response — including a CSV record that fails to parse because
            // the body was cut off mid-record — must be retried rather than
            // silently treated as a short-but-complete page. Before this,
            // these landed in `Transport` and were NOT retried, which is
            // exactly the truncated-body-becomes-a-silent-short-page bug
            // this client must never repeat.
            ClientError::Transport(_) => true,
            _ => false,
        }
    }
}

fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Thin wrapper around `reqwest::Client` configured for RDW: a fixed
/// timeout and an optional `X-App-Token` header taken from the server-side
/// `RDW_APP_TOKEN` environment variable. Absent token is allowed.
#[derive(Clone)]
pub struct RdwClient {
    http: reqwest::Client,
    app_token: Option<String>,
    resource_base: String,
    metadata_base: String,
    retry_config: RetryConfig,
    fuel_page_limit: u32,
    fuel_concurrency: usize,
    fuel_kenteken_batch: usize,
}

const DEFAULT_METADATA_BASE: &str = "https://opendata.rdw.nl/api/views";

/// How many kentekens go into one fuel query.
///
/// Measured against opendata.rdw.nl: 1,000 plates makes a 15KB URL and answers
/// in about 0.4s, while 2,000 is rejected with HTTP 414 Request-URI Too Large.
/// 800 keeps clear headroom under that ceiling.
pub const FUEL_KENTEKEN_BATCH: usize = 800;

/// This crate cannot depend on `rdw-core` (which depends on this crate), so
/// `rdw_core::merge::MAX_FUEL_ENTRIES` (currently 3) is duplicated here only
/// for this compile-time sanity check. If that constant ever changes, this
/// assertion (and the one below) must be updated to match.
const ASSUMED_MAX_FUEL_ENTRIES_PER_VEHICLE: usize = 3;

// A batch's worst-case fuel row count (every plate at the maximum fuel
// entries) must stay comfortably under `DEFAULT_FUEL_PAGE_LIMIT`, or a
// single kenteken batch could legitimately hit the page's `$limit` and the
// runtime check in `fetch_fuel_for_kentekens` would reject a correct
// response as a false-positive truncation.
const _: () = assert!(
    FUEL_KENTEKEN_BATCH * ASSUMED_MAX_FUEL_ENTRIES_PER_VEHICLE < DEFAULT_FUEL_PAGE_LIMIT as usize
);

/// How many fuel batches are in flight at once.
///
/// The batches are independent, so this is close to a linear speed-up on the
/// part of an export that dominates its runtime. It is capped because Socrata
/// throttles, and an unauthenticated caller harder still, so past some point
/// extra concurrency buys contention rather than rows.
///
/// Measured on a full Lexus export (31,512 vehicles) against opendata.rdw.nl:
/// 1 -> 30.4s, 8 -> 12.9s, 16 -> 7.7s, 24 -> 7.7s, 32 -> 14.9s. Sixteen is the
/// knee of that curve: 24 gains nothing and 32 is worse than 8. Override with
/// `FUEL_CONCURRENCY`.
pub const DEFAULT_FUEL_CONCURRENCY: usize = 16;

/// Hard ceiling on a single decompressed response body.
///
/// Enabling reqwest's `gzip` feature made transparent decompression a new
/// attack surface: a small compressed body can expand without bound, and
/// `resp.bytes()` would happily buffer all of it. The largest legitimate
/// response is one vehicle page, which at the 50,000-row Socrata cap measures
/// about 40 MB of CSV, so 256 MB leaves a very wide margin while still
/// bounding the damage a malformed or hostile response can do to a container
/// that also has an export staged in memory.
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// How much of an upstream error body is kept. The body is surfaced in
/// `ClientError::Http`, which reaches the API caller inside a 502 message, so
/// an unbounded upstream error page would be both an unbounded allocation and
/// a needlessly large disclosure of upstream internals.
const MAX_ERROR_BODY_BYTES: usize = 512;

impl RdwClient {
    pub fn new(app_token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // Explicit even though the `gzip` feature enables this by
            // default: a future `default-features = false` on reqwest must
            // not silently turn off transparent response decompression,
            // which is the primary cost lever of this client.
            .gzip(true)
            .build()
            .expect("reqwest client with static config must build");
        Self {
            http,
            app_token,
            resource_base: SOCRATA_BASE.to_string(),
            metadata_base: DEFAULT_METADATA_BASE.to_string(),
            retry_config: RetryConfig::default(),
            fuel_page_limit: DEFAULT_FUEL_PAGE_LIMIT,
            fuel_concurrency: DEFAULT_FUEL_CONCURRENCY,
            fuel_kenteken_batch: FUEL_KENTEKEN_BATCH,
        }
    }

    /// Override the resource (dataset) endpoint base URL. Test-only hook so
    /// vehicle/fuel fetches can be pointed at a mock server.
    pub fn with_resource_base(mut self, base: impl Into<String>) -> Self {
        self.resource_base = base.into();
        self
    }

    /// Override the metadata endpoint base URL. Test-only hook so
    /// `fetch_column_names` can be pointed at a mock server.
    pub fn with_metadata_base(mut self, base: impl Into<String>) -> Self {
        self.metadata_base = base.into();
        self
    }

    /// Override the retry policy. Test-only hook to remove real backoff
    /// delays while still exercising the retry decision logic.
    pub fn with_retry_config(mut self, retry_config: RetryConfig) -> Self {
        self.retry_config = retry_config;
        self
    }

    /// Override the per-page row limit used by `fetch_fuel_range`'s keyset
    /// pagination. Test-only hook so pagination itself can be exercised
    /// without constructing tens of thousands of mock rows; production
    /// always uses `DEFAULT_FUEL_PAGE_LIMIT`, matching Socrata's cap.
    pub fn with_fuel_page_limit(mut self, limit: u32) -> Self {
        self.fuel_page_limit = limit;
        self
    }

    /// Override how many fuel batches are fetched concurrently. A value of 0 is
    /// treated as 1, since a stream buffered at 0 would never make progress.
    pub fn with_fuel_concurrency(mut self, concurrency: usize) -> Self {
        self.fuel_concurrency = concurrency.max(1);
        self
    }

    /// Override how many kentekens go into one fuel query. Production
    /// defaults to `FUEL_KENTEKEN_BATCH`; overridable via the
    /// `FUEL_KENTEKEN_BATCH` environment variable (read by the `rdw-api`
    /// binary at startup) so re-tuning never requires a code change. A
    /// value of 0 is treated as 1 for the same reason as
    /// `with_fuel_concurrency`.
    pub fn with_fuel_kenteken_batch(mut self, batch: usize) -> Self {
        self.fuel_kenteken_batch = batch.max(1);
        self
    }

    /// Build the SODA URL for one keyset page of vehicles, filtered by an
    /// allow-listed set of `merk` values and, when present, a `kenteken`
    /// cursor from the previous page's last row.
    pub fn vehicle_page_url(
        &self,
        merken: &[String],
        after_kenteken: Option<&str>,
        limit: u32,
    ) -> String {
        let merk_list = merken
            .iter()
            .map(|m| format!("'{}'", escape_soql(m)))
            .collect::<Vec<_>>()
            .join(",");
        let mut where_clause = format!("merk in({merk_list})");
        if let Some(after) = after_kenteken {
            where_clause.push_str(&format!(" AND kenteken > '{}'", escape_soql(after)));
        }
        format!(
            "{}/{VEHICLE_DATASET_ID}.csv?$where={}&$order=kenteken&$limit={limit}",
            self.resource_base,
            urlencoding_soql(&where_clause)
        )
    }

    /// Build the SODA URL for one keyset page of vehicles constrained to a
    /// kenteken RANGE (Scope C, concurrent range fetching): `kenteken >
    /// range_lo AND kenteken <= range_hi`. `range_lo`/`range_hi` are the
    /// range's own fixed boundaries (`None` = unbounded on that side, for
    /// the first/last range); `cursor` narrows the lower bound further as
    /// pages within the range advance, exactly like `after_kenteken` in
    /// `vehicle_page_url`. The range boundary is expressed SOLELY in this
    /// `$where` clause — Socrata evaluates it, never the client.
    pub fn vehicle_range_page_url(
        &self,
        merken: &[String],
        range_lo: Option<&str>,
        range_hi: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> String {
        let merk_list = merken
            .iter()
            .map(|m| format!("'{}'", escape_soql(m)))
            .collect::<Vec<_>>()
            .join(",");
        let mut where_clause = format!("merk in({merk_list})");
        // The cursor only ever narrows forward from the range's own lower
        // bound, so once a cursor exists it is strictly the tighter bound.
        if let Some(lo) = cursor.or(range_lo) {
            where_clause.push_str(&format!(" AND kenteken > '{}'", escape_soql(lo)));
        }
        if let Some(hi) = range_hi {
            where_clause.push_str(&format!(" AND kenteken <= '{}'", escape_soql(hi)));
        }
        format!(
            "{}/{VEHICLE_DATASET_ID}.csv?$where={}&$order=kenteken&$limit={limit}",
            self.resource_base,
            urlencoding_soql(&where_clause)
        )
    }

    /// Build the SODA URL for the total row count of the SAME population one
    /// `vehicle_page_url`/`vehicle_range_page_url` call would page through
    /// (`merk in(...)`, no cursor/range narrowing). Used once per export, up
    /// front, so the end-to-end row-count reconciliation (criterion V.1)
    /// compares against a real aggregate rather than an assumed one.
    pub fn vehicle_count_url(&self, merken: &[String]) -> String {
        let merk_list = merken
            .iter()
            .map(|m| format!("'{}'", escape_soql(m)))
            .collect::<Vec<_>>()
            .join(",");
        let where_clause = format!("merk in({merk_list})");
        format!(
            "{}/{VEHICLE_DATASET_ID}.csv?$select=count(kenteken)&$where={}",
            self.resource_base,
            urlencoding_soql(&where_clause)
        )
    }

    /// Column a vehicle-count CSV response's header must contain. Socrata
    /// names a `count(x)` aggregate column `count_x`.
    const VEHICLE_COUNT_REQUIRED_COLUMNS: &'static [&'static str] = &["count_kenteken"];

    /// Fetch the total vehicle row count for `merken` — the same population
    /// `fetch_and_widen_concurrent` pages through — used once, up front, as
    /// the expected side of the end-to-end row-count reconciliation
    /// (criterion V.1). Goes through the same retry path and CSV parser as
    /// every other fetch, so an upstream failure here is reported exactly
    /// like any other upstream failure, never silently skipped.
    pub async fn fetch_vehicle_count(&self, merken: &[String]) -> Result<u64, ClientError> {
        let url = self.vehicle_count_url(merken);
        let rows = self
            .get_csv_rows(&url, Self::VEHICLE_COUNT_REQUIRED_COLUMNS)
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| ClientError::Decode("count response returned no rows".to_string()))?;
        let raw = row
            .get("count_kenteken")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClientError::Decode("count response missing count_kenteken column".to_string())
            })?;
        raw.parse::<u64>()
            .map_err(|e| ClientError::Decode(format!("failed to parse vehicle count '{raw}': {e}")))
    }

    /// Fetch one keyset page of vehicles within a fixed kenteken range. See
    /// `vehicle_range_page_url`.
    pub async fn fetch_vehicle_range_page(
        &self,
        merken: &[String],
        range_lo: Option<&str>,
        range_hi: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Vec<VehicleRow>, ClientError> {
        let url = self.vehicle_range_page_url(merken, range_lo, range_hi, cursor, limit);
        let rows = self
            .get_csv_rows(&url, Self::VEHICLE_REQUIRED_COLUMNS)
            .await?;
        Ok(rows.into_iter().map(VehicleRow).collect())
    }

    /// Build the SODA URL for one keyset page of the fuel rows covering a
    /// vehicle batch's kenteken range (inclusive), ordered so the merge-join
    /// can validate per-vehicle sequencing. `after` is the `(kenteken,
    /// brandstof_volgnummer)` cursor from the previous page's last row, so a
    /// range larger than one page is fetched by resuming exactly where the
    /// prior page ended rather than restarting from `lo`.
    /// Build a fuel query for an explicit list of kentekens.
    ///
    /// Fuel rows used to be fetched by kenteken RANGE, but a brand's plates are
    /// scattered across the whole kenteken space: every Lexus lies between
    /// 00GDF5 and ZV939H, a range holding 16,962,192 of the dataset's
    /// 16,966,705 fuel rows. Fetching a range therefore meant walking almost
    /// the entire dataset to extract a few thousand rows. Naming the plates
    /// fetches only what the export actually needs.
    ///
    /// The caller batches; see `FUEL_KENTEKEN_BATCH`.
    pub fn fuel_by_kentekens_url(&self, kentekens: &[String]) -> String {
        let list = kentekens
            .iter()
            .map(|k| format!("'{}'", escape_soql(k)))
            .collect::<Vec<_>>()
            .join(",");
        let where_clause = format!("kenteken in ({list})");
        format!(
            "{}/{FUEL_DATASET_ID}.csv?$where={}&$order=kenteken,brandstof_volgnummer&$limit={}",
            self.resource_base,
            urlencoding_soql(&where_clause),
            self.fuel_page_limit
        )
    }

    /// Columns a vehicle CSV page's header must contain. Its absence means
    /// the response is not a real vehicle page — most likely a Socrata error
    /// page served with HTTP 200 — and must be rejected as an upstream error
    /// before the empty-page check, or it silently becomes an empty export.
    const VEHICLE_REQUIRED_COLUMNS: &'static [&'static str] = &["kenteken"];
    /// Columns a fuel CSV page's header must contain; see
    /// `VEHICLE_REQUIRED_COLUMNS`. `brandstof_volgnummer` is required too,
    /// since `merge_join` cannot validate per-vehicle sequencing without it.
    const FUEL_REQUIRED_COLUMNS: &'static [&'static str] = &["kenteken", "brandstof_volgnummer"];

    /// Fetch one keyset page of vehicles with retry-with-backoff. Requests
    /// and parses Socrata's CSV format rather than JSON: about 1.5x smaller
    /// even before gzip, and every cell is read as a plain string (never
    /// numeric/boolean-inferred), matching the JSON path's dynamic-object
    /// shape via `parse_csv_rows`.
    pub async fn fetch_vehicle_page(
        &self,
        merken: &[String],
        after_kenteken: Option<&str>,
        limit: u32,
    ) -> Result<Vec<VehicleRow>, ClientError> {
        let url = self.vehicle_page_url(merken, after_kenteken, limit);
        let rows = self
            .get_csv_rows(&url, Self::VEHICLE_REQUIRED_COLUMNS)
            .await?;
        Ok(rows.into_iter().map(VehicleRow).collect())
    }

    /// Fetch every fuel row for an explicit list of kentekens, in batches of
    /// `fuel_kenteken_batch` names, fetched concurrently up to
    /// `fuel_concurrency` at a time. Each batch is a single CSV page, not
    /// itself paginated: the compile-time assertion on `FUEL_KENTEKEN_BATCH`
    /// keeps a batch's worst-case row count (batch size * `MAX_FUEL_ENTRIES`)
    /// well under `DEFAULT_FUEL_PAGE_LIMIT`, and a response landing exactly
    /// on the configured `$limit` is treated as a truncation (more rows exist
    /// than were returned) rather than silently accepted as complete.
    pub async fn fetch_fuel_for_kentekens(
        &self,
        kentekens: &[String],
    ) -> Result<Vec<FuelRow>, ClientError> {
        use futures::stream::{StreamExt, TryStreamExt};

        // Batches are independent, so they are fetched concurrently. `buffered`
        // rather than `buffer_unordered`: results must stay in the order the
        // batches were issued, because the plates arrive kenteken-sorted and
        // `merge_join` relies on the concatenated fuel rows being sorted too.
        //
        // Concurrency is capped rather than unbounded. Socrata throttles, and
        // an unauthenticated caller is throttled harder, so firing hundreds of
        // requests at once buys 429s and retries instead of speed.
        let urls: Vec<String> = kentekens
            .chunks(self.fuel_kenteken_batch)
            .map(|batch| self.fuel_by_kentekens_url(batch))
            .collect();
        let page_limit = self.fuel_page_limit;

        let pages: Vec<Vec<FuelRow>> = futures::stream::iter(urls)
            .map(|url| async move {
                let raw_rows = self.get_csv_rows(&url, Self::FUEL_REQUIRED_COLUMNS).await?;
                if raw_rows.len() as u32 == page_limit {
                    // A batch is never paginated further, so a response
                    // landing exactly on `$limit` means rows beyond it exist
                    // and were silently cut off. The compile-time assertion
                    // above keeps this from happening in normal operation;
                    // this is the runtime backstop.
                    return Err(ClientError::Decode(format!(
                        "fuel page returned exactly the configured limit ({page_limit}) rows; \
                         more rows may exist beyond it"
                    )));
                }
                let rows: Vec<FuelRow> = raw_rows.into_iter().map(FuelRow).collect();
                Ok::<_, ClientError>(rows)
            })
            .buffered(self.fuel_concurrency)
            .try_collect()
            .await?;

        Ok(pages.into_iter().flatten().collect())
    }

    /// Fetch and parse the RDW dataset metadata used for CSV column headers.
    ///
    /// Returns each column as `(field_name, display_name)`. `fieldName` is the
    /// machine key used to read values out of a data row; `name` is RDW's own
    /// human-readable label, which is what the CSV header shows, so a reader
    /// sees "Gemiddelde Lading Waarde" instead of `gem_lading_wrde`. A column
    /// without a `name` falls back to its `fieldName` rather than failing.
    ///
    /// Callers apply their own fallback when this fails; this function only
    /// reports the raw error.
    pub async fn fetch_column_names(
        &self,
        dataset_id: &str,
    ) -> Result<Vec<(String, String)>, ClientError> {
        let url = format!("{}/{dataset_id}.json", self.metadata_base);
        let body = with_retry(&self.retry_config, || self.get(&url)).await?;
        let value: Value =
            serde_json::from_str(&body).map_err(|e| ClientError::Decode(e.to_string()))?;
        let columns = value
            .get("columns")
            .and_then(Value::as_array)
            .ok_or_else(|| ClientError::Decode("missing columns array".to_string()))?;
        columns
            .iter()
            .map(|c| {
                let field = c
                    .get("fieldName")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ClientError::Decode("column missing fieldName".to_string()))?;
                let display = c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(field)
                    .to_string();
                Ok((field.to_string(), display))
            })
            .collect()
    }

    /// Fetch one CSV page with retry-with-backoff and parse it into row
    /// maps. The CSV parse happens INSIDE the retried closure (not after
    /// `with_retry` returns), so a body that read fine at the HTTP layer but
    /// fails to parse as CSV (e.g. cut off mid-record, or a gzip stream cut
    /// short and failing its CRC check on decode) is retried exactly like a
    /// timeout or 5xx, rather than surfacing as a one-shot failure.
    async fn get_csv_rows(
        &self,
        url: &str,
        required_columns: &'static [&'static str],
    ) -> Result<Vec<Map<String, Value>>, ClientError> {
        with_retry(&self.retry_config, || async {
            let bytes = self.get_bytes(url).await?;
            csv_parse::parse_csv_rows(&bytes, required_columns)
        })
        .await
    }

    // Retained only for tests exercising the generic HTTP retry/timeout
    // machinery against a plain JSON body; production fetches now go through
    // `get_csv_rows`.
    #[cfg(test)]
    async fn get_json_array(&self, url: &str) -> Result<Vec<Map<String, Value>>, ClientError> {
        let body = with_retry(&self.retry_config, || self.get(url)).await?;
        let value: Value =
            serde_json::from_str(&body).map_err(|e| ClientError::Decode(e.to_string()))?;
        match value {
            Value::Array(items) => items
                .into_iter()
                .map(|v| {
                    v.as_object()
                        .cloned()
                        .ok_or_else(|| ClientError::Decode("row is not a JSON object".to_string()))
                })
                .collect(),
            _ => Err(ClientError::Decode("expected a JSON array".to_string())),
        }
    }

    /// Like `get`, but returns the raw response bytes rather than decoding
    /// them as UTF-8 text, for callers (CSV parsing) that want to feed the
    /// byte stream straight to a parser instead of splitting on '\n'
    /// themselves. A read failure here — including reqwest's automatic gzip
    /// decoder rejecting a truncated compressed stream (bad CRC/ISIZE) — is
    /// `ClientError::Transport`, which `is_retryable` treats as retryable.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, ClientError> {
        let mut req = self.http.get(url);
        if let Some(token) = &self.app_token {
            req = req.header("X-App-Token", token);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                ClientError::Timeout
            } else {
                ClientError::Transport(e.to_string())
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            let mut body = resp.text().await.unwrap_or_default();
            // Truncate on a char boundary: this string is echoed into a 502.
            if body.len() > MAX_ERROR_BODY_BYTES {
                let mut end = MAX_ERROR_BODY_BYTES;
                while end > 0 && !body.is_char_boundary(end) {
                    end -= 1;
                }
                body.truncate(end);
                body.push_str("… (truncated)");
            }
            return Err(ClientError::Http {
                status: status.as_u16(),
                body,
            });
        }

        // Read the body in chunks against a byte budget rather than calling
        // `resp.bytes()`, which buffers whatever arrives. With gzip enabled the
        // decompressed size is not bounded by anything the response declares,
        // so a `Content-Length` check would not help.
        let mut out: Vec<u8> = Vec::new();
        let mut resp = resp;
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| ClientError::Transport(e.to_string()))?;
            let Some(chunk) = chunk else { break };
            if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(ClientError::Decode(format!(
                    "RDW response exceeded the {MAX_RESPONSE_BYTES}-byte ceiling"
                )));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    async fn get(&self, url: &str) -> Result<String, ClientError> {
        let mut req = self.http.get(url);
        if let Some(token) = &self.app_token {
            req = req.header("X-App-Token", token);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                ClientError::Timeout
            } else {
                ClientError::Transport(e.to_string())
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Http {
                status: status.as_u16(),
                body,
            });
        }
        resp.text()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))
    }
}

/// Retry a fallible async operation up to `config.max_attempts` times with
/// exponential backoff (`initial_backoff` -> `max_backoff`) plus jitter,
/// retrying only on timeout, 5xx, or 429. Any other error, or exhaustion of
/// attempts, returns immediately. Tests inject a zero-duration `RetryConfig`
/// to exercise the retry decision without real wall-clock sleeps.
pub async fn with_retry<F, Fut, T>(config: &RetryConfig, mut op: F) -> Result<T, ClientError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>>,
{
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if err.is_retryable() && attempt < config.max_attempts => {
                let backoff = (config.initial_backoff.saturating_mul(1u32 << (attempt - 1)))
                    .min(config.max_backoff);
                let jitter_ms =
                    rand::thread_rng().gen_range(0..=backoff.as_millis() as u64 / 4 + 1);
                tokio::time::sleep(backoff + Duration::from_millis(jitter_ms)).await;
                attempt += 1;
            }
            Err(err) if err.is_retryable() => {
                return Err(ClientError::RetriesExhausted {
                    attempts: attempt,
                    last: err.to_string(),
                });
            }
            Err(err) => return Err(err),
        }
    }
}

/// Escape single quotes for a SoQL string literal.
fn escape_soql(s: &str) -> String {
    s.replace('\'', "''")
}

/// Percent-encode a SoQL `$where` clause for use in a query string.
fn urlencoding_soql(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn vehicle_json(kenteken: &str) -> Value {
        json!({ "kenteken": kenteken, "merk": "TOYOTA" })
    }

    /// A client with zero backoff, so retry-exercising tests stay fast and
    /// deterministic instead of sleeping through real exponential delays.
    fn fast_client() -> RdwClient {
        RdwClient::new(None).with_retry_config(RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(0),
            max_backoff: Duration::from_millis(0),
        })
    }

    #[test]
    fn vehicle_page_url_has_no_offset_param() {
        let url =
            RdwClient::new(None).vehicle_page_url(&["TOYOTA".to_string()], Some("AA123B"), 50000);
        assert!(
            !url.contains("$offset"),
            "must never use $offset pagination"
        );
        assert!(url.contains("%24order=kenteken") || url.contains("$order=kenteken"));
        assert!(url.contains("kenteken"));
    }

    #[test]
    fn vehicle_page_url_omits_cursor_on_first_page() {
        let url = RdwClient::new(None).vehicle_page_url(&["TOYOTA".to_string()], None, 50000);
        assert!(
            !url.contains("kenteken+%3E"),
            "first page should not filter by cursor"
        );
    }

    // --- Scope C: kenteken-range vehicle queries ---

    #[test]
    fn happy_path_range_query_filters_both_lower_and_upper_bound() {
        let url = RdwClient::new(None).vehicle_range_page_url(
            &["TOYOTA".to_string()],
            Some("0001VH"),
            Some("5001VH"),
            None,
            3000,
        );
        assert!(url.contains("0001VH") && url.contains("5001VH"));
        assert!(url.contains("kenteken+%3E") || url.contains("kenteken%20%3E"));
        assert!(url.contains("kenteken+%3C%3D") || url.contains("kenteken%20%3C%3D"));
    }

    #[test]
    fn edge_first_range_omits_lower_bound_when_unbounded() {
        let url = RdwClient::new(None).vehicle_range_page_url(
            &["TOYOTA".to_string()],
            None,
            Some("5001VH"),
            None,
            3000,
        );
        assert!(
            !url.contains("kenteken+%3E") && !url.contains("kenteken%20%3E"),
            "an unbounded-below range must not filter by a lower bound: {url}"
        );
        assert!(url.contains("5001VH"));
    }

    #[test]
    fn edge_last_range_omits_upper_bound_when_unbounded() {
        let url = RdwClient::new(None).vehicle_range_page_url(
            &["TOYOTA".to_string()],
            Some("5001VH"),
            None,
            None,
            3000,
        );
        assert!(
            !url.contains("kenteken+%3C%3D") && !url.contains("kenteken%20%3C%3D"),
            "an unbounded-above range must not filter by an upper bound: {url}"
        );
        assert!(url.contains("5001VH"));
    }

    #[test]
    fn happy_path_cursor_overrides_the_range_lower_bound_as_pages_advance() {
        let url = RdwClient::new(None).vehicle_range_page_url(
            &["TOYOTA".to_string()],
            Some("0001VH"),
            Some("5001VH"),
            Some("3000VH"),
            3000,
        );
        // The cursor, not the range's own lower bound, must be the filter,
        // since pages within a range advance past the range's start.
        assert!(url.contains("3000VH"));
        assert!(!url.contains("0001VH"));
    }

    #[tokio::test]
    async fn happy_path_fetch_vehicle_range_page_returns_rows_within_the_range() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.csv")))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,merk\nAA001A,TOYOTA\n"),
            )
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let rows = client
            .fetch_vehicle_range_page(&["TOYOTA".to_string()], None, Some("ZZ999Z"), None, 3000)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kenteken(), Some("AA001A"));
    }

    /// Criterion A.2: the client must explicitly opt into gzip decoding
    /// (`.gzip(true)`) at construction, so a future `default-features =
    /// false` on reqwest cannot silently disable transparent decompression.
    /// This exercises the *behaviour* that call protects: a gzip-encoded
    /// response body must come back decoded.
    #[tokio::test]
    async fn fetch_vehicle_page_gzip_encoded_response_is_transparently_decoded() {
        use std::io::Write;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let csv_body = "kenteken,merk\nAA001A,TOYOTA\n";
        encoder.write_all(csv_body.as_bytes()).unwrap();
        let gzipped = encoder.finish().unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.csv")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_raw(gzipped, "text/csv"),
            )
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let rows = client
            .fetch_vehicle_page(&["TOYOTA".to_string()], None, 50000)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kenteken(), Some("AA001A"));
    }

    #[tokio::test]
    async fn fetch_vehicle_page_happy_path_returns_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![vehicle_json("AA001A")]))
            .mount(&server)
            .await;

        // We cannot easily point the client at the mock server since the
        // base URL is a constant; exercise get_json_array indirectly via a
        // client built with a custom base by calling `get` through a real
        // request against the mock server URL directly.
        let client = fast_client();
        let rows = client
            .get_json_array(&format!(
                "{}/resource/{VEHICLE_DATASET_ID}.json",
                server.uri()
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("kenteken").unwrap().as_str().unwrap(), "AA001A");
    }

    #[tokio::test]
    async fn fetch_vehicle_page_retries_on_500_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![vehicle_json("AA002A")]))
            .mount(&server)
            .await;

        let client = fast_client();
        let rows = client
            .get_json_array(&format!(
                "{}/resource/{VEHICLE_DATASET_ID}.json",
                server.uri()
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn fetch_vehicle_page_exhausts_retries_on_persistent_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = fast_client();
        let err = client
            .get_json_array(&format!(
                "{}/resource/{VEHICLE_DATASET_ID}.json",
                server.uri()
            ))
            .await
            .unwrap_err();
        // 404 is not retryable at all: it should fail immediately, not
        // after exhausting five attempts.
        match err {
            ClientError::Http { status, .. } => assert_eq!(status, 404),
            other => panic!("expected Http(404), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_vehicle_page_exhausts_retries_on_persistent_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = fast_client();
        let err = client
            .get_json_array(&format!(
                "{}/resource/{VEHICLE_DATASET_ID}.json",
                server.uri()
            ))
            .await
            .unwrap_err();
        match err {
            ClientError::RetriesExhausted { attempts, .. } => assert_eq!(attempts, 5),
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
    }

    /// Build a fuel CSV response body (header + rows) from `(kenteken,
    /// volgnummer)` pairs, matching what a real Socrata `.csv` fuel page
    /// looks like.
    fn fuel_csv(rows: &[(&str, u32)]) -> String {
        let mut body = "kenteken,brandstof_volgnummer\n".to_string();
        for (kenteken, volgnummer) in rows {
            body.push_str(&format!("{kenteken},{volgnummer}\n"));
        }
        body
    }

    #[test]
    fn fuel_row_parses_volgnummer_and_kenteken() {
        let row = FuelRow(
            json!({ "kenteken": "AA001A", "brandstof_volgnummer": "2" })
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(row.kenteken(), Some("AA001A"));
        assert_eq!(row.volgnummer(), Some(2));
    }

    #[test]
    fn fuel_row_missing_volgnummer_returns_none() {
        let row = FuelRow(json!({ "kenteken": "AA001A" }).as_object().unwrap().clone());
        assert_eq!(row.volgnummer(), None);
    }

    #[test]
    fn fuel_by_kentekens_url_names_every_plate_and_never_uses_a_range() {
        let client = RdwClient::new(None);
        let url = client.fuel_by_kentekens_url(&["AA001A".to_string(), "BB002B".to_string()]);
        assert!(url.contains("AA001A") && url.contains("BB002B"));
        assert!(url.contains("in+%28") || url.contains("in%20%28") || url.contains("in+("));
        // A range query would drag in nearly the whole fuel dataset.
        assert!(!url.contains("%3E%3D"), "must not use a >= range: {url}");
        assert!(!url.contains("brandstof_volgnummer+%3E"));
    }

    #[test]
    fn fuel_by_kentekens_url_escapes_quotes_in_a_plate() {
        let client = RdwClient::new(None);
        let url = client.fuel_by_kentekens_url(&["A'B".to_string()]);
        assert!(
            url.contains("A%27%27B"),
            "single quote must be doubled: {url}"
        );
    }

    #[tokio::test]
    async fn fetch_fuel_for_kentekens_happy_path_returns_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.csv")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fuel_csv(&[("AA001A", 1), ("AA001A", 2)])),
            )
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let rows = client
            .fetch_fuel_for_kentekens(&["AA001A".to_string()])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn fetch_fuel_for_kentekens_edge_splits_into_batches_and_keeps_every_row() {
        // More plates than one batch holds must still come back complete: a
        // dropped batch would silently blank the fuel columns for those cars.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.csv")))
            .respond_with(ResponseTemplate::new(200).set_body_string(fuel_csv(&[("AA001A", 1)])))
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let plates: Vec<String> = (0..FUEL_KENTEKEN_BATCH * 2 + 1)
            .map(|i| format!("K{i:05}"))
            .collect();
        let rows = client.fetch_fuel_for_kentekens(&plates).await.unwrap();

        // Three batches for 2N+1 plates, one mocked row each.
        assert_eq!(rows.len(), 3, "every batch's rows must be kept");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            3,
            "plates must be split into ceil(2N+1 / N) = 3 requests"
        );
    }

    #[tokio::test]
    async fn fetch_fuel_for_kentekens_keeps_batch_order_when_fetched_concurrently() {
        // Concurrency must not reorder results: merge_join requires the fuel
        // rows to arrive kenteken-sorted, and the plates are handed in sorted,
        // so batch N's rows must still precede batch N+1's. A slow first batch
        // would overtake under buffer_unordered.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.csv")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fuel_csv(&[("AA001A", 1)]))
                    .set_delay(std::time::Duration::from_millis(120)),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.csv")))
            .respond_with(ResponseTemplate::new(200).set_body_string(fuel_csv(&[("ZZ999Z", 1)])))
            .mount(&server)
            .await;

        let client = fast_client()
            .with_resource_base(format!("{}/resource", server.uri()))
            .with_fuel_concurrency(8);
        let plates: Vec<String> = (0..FUEL_KENTEKEN_BATCH + 1)
            .map(|i| format!("K{i:05}"))
            .collect();
        let rows = client.fetch_fuel_for_kentekens(&plates).await.unwrap();

        let order: Vec<_> = rows.iter().filter_map(|r| r.kenteken()).collect();
        assert_eq!(
            order,
            vec!["AA001A", "ZZ999Z"],
            "the slow first batch must still come first"
        );
    }

    #[tokio::test]
    async fn fetch_fuel_for_kentekens_failure_propagates_upstream_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.csv")))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let err = client
            .fetch_fuel_for_kentekens(&["AA001A".to_string()])
            .await
            .expect_err("a persistent 500 must surface, not be silently empty");
        assert!(err.to_string().contains("500"), "got: {err}");
    }

    /// Criterion B.4: a body that reads fully at the HTTP layer but is cut
    /// off mid-CSV-record (an unterminated quoted field, here) must be an
    /// error retried like any other transient failure, never mistaken for a
    /// short-but-complete page.
    #[tokio::test]
    async fn fetch_fuel_for_kentekens_truncated_body_is_retried_then_raised_not_silently_short() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.csv")))
            .respond_with(
                ResponseTemplate::new(200)
                    // An opening quote with no closing quote: the record
                    // never terminates, exactly what a body cut off
                    // mid-record looks like to the CSV reader.
                    .set_body_string("kenteken,brandstof_volgnummer\n\"AA001A,1\n"),
            )
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let err = client
            .fetch_fuel_for_kentekens(&["AA001A".to_string()])
            .await
            .expect_err("a truncated body must be an error, not a silently short page");
        assert!(
            matches!(err, ClientError::RetriesExhausted { .. }),
            "a truncated-body error must be retried, not returned as a one-shot failure: {err:?}"
        );
    }

    /// Criterion 8: `fetch_fuel_for_kentekens` never paginates a batch
    /// further, so a response landing exactly on the configured `$limit`
    /// means rows beyond it were silently cut off, and must be rejected
    /// rather than accepted as a complete (if suspiciously round) page.
    #[tokio::test]
    async fn fetch_fuel_for_kentekens_response_exactly_at_limit_is_rejected_as_truncated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.csv")))
            .respond_with(ResponseTemplate::new(200).set_body_string(fuel_csv(&[("AA001A", 1)])))
            .mount(&server)
            .await;

        let client = fast_client()
            .with_resource_base(format!("{}/resource", server.uri()))
            .with_fuel_page_limit(1);
        let err = client
            .fetch_fuel_for_kentekens(&["AA001A".to_string()])
            .await
            .expect_err("a page landing exactly on $limit must be rejected as truncated");
        assert!(
            err.to_string().contains("limit"),
            "error should explain the exact-limit truncation: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_column_names_happy_path_parses_field_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/m9d7-ebf2.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [
                    { "fieldName": "kenteken", "name": "Kenteken" },
                    { "fieldName": "merk", "name": "Merk" },
                    { "fieldName": "gem_lading_wrde" }
                ]
            })))
            .mount(&server)
            .await;

        let client = fast_client().with_metadata_base(server.uri());
        let columns = client.fetch_column_names("m9d7-ebf2").await.unwrap();
        assert_eq!(
            columns,
            vec![
                ("kenteken".to_string(), "Kenteken".to_string()),
                ("merk".to_string(), "Merk".to_string()),
                // No `name` in the metadata: the fieldName stands in rather
                // than the column being dropped or the fetch failing.
                ("gem_lading_wrde".to_string(), "gem_lading_wrde".to_string()),
            ]
        );
    }

    // --- Criterion V.1: end-to-end row-count reconciliation ---

    #[test]
    fn vehicle_count_url_uses_select_count_and_the_same_where_clause_as_the_page_url() {
        let client = RdwClient::new(None);
        let merken = vec!["TOYOTA".to_string(), "LEXUS".to_string()];
        let count_url = client.vehicle_count_url(&merken);
        let page_url = client.vehicle_page_url(&merken, None, 50000);

        assert!(
            count_url.contains("%24select=count%28kenteken%29")
                || count_url.contains("$select=count(kenteken)"),
            "count url must select count(kenteken): {count_url}"
        );
        // Both URLs must filter the exact same population: extract the
        // `$where` value from each and compare, rather than the whole URL
        // (which differs in $select/$order/$limit).
        fn where_of(url: &str) -> &str {
            url.split("$where=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap()
        }
        assert_eq!(
            where_of(&count_url),
            where_of(&page_url),
            "count query must filter the same merk population as the page query"
        );
    }

    #[tokio::test]
    async fn fetch_vehicle_count_happy_path_parses_the_count_column() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.csv")))
            .respond_with(ResponseTemplate::new(200).set_body_string("count_kenteken\n824620\n"))
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let count = client
            .fetch_vehicle_count(&["TOYOTA".to_string()])
            .await
            .unwrap();
        assert_eq!(count, 824620);
    }

    /// Edge: an upstream failure on the count query must surface as an
    /// error, never be silently treated as "no reconciliation possible".
    #[tokio::test]
    async fn fetch_vehicle_count_persistent_upstream_failure_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.csv")))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let err = client
            .fetch_vehicle_count(&["TOYOTA".to_string()])
            .await
            .expect_err("a persistent 500 on the count query must not be swallowed");
        assert!(matches!(err, ClientError::RetriesExhausted { .. }));
    }

    /// An upstream error body reaches the API caller inside a 502 message, so
    /// it must not be echoed back without bound.
    #[tokio::test]
    async fn failure_oversized_upstream_error_body_is_truncated_before_it_is_surfaced() {
        let server = MockServer::start().await;
        let huge = "E".repeat(50_000);
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.csv")))
            .respond_with(ResponseTemplate::new(500).set_body_string(huge))
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let err = client
            .fetch_vehicle_page(&["TOYOTA".to_string()], None, 10)
            .await
            .unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.len() < 2_000,
            "an upstream error body must be truncated before it is surfaced, got {} bytes",
            rendered.len()
        );
        assert!(rendered.contains("truncated"), "got: {rendered}");
    }

    /// A response body without a trailing newline is a truncated body, and the
    /// client must surface that rather than handing a short page upwards.
    #[tokio::test]
    async fn failure_truncated_response_body_never_becomes_a_short_page() {
        let server = MockServer::start().await;
        // Two complete records, then a cut before the final newline.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{VEHICLE_DATASET_ID}.csv")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("kenteken,merk\nAA001A,TOYOTA\nBB002B,TOYOTA"),
            )
            .mount(&server)
            .await;

        let client = fast_client().with_resource_base(format!("{}/resource", server.uri()));
        let err = client
            .fetch_vehicle_page(&["TOYOTA".to_string()], None, 50_000)
            .await
            .expect_err("a truncated body must not parse into a short page");
        // Retried first (truncation is usually transient), then surfaced.
        assert!(
            matches!(err, ClientError::RetriesExhausted { .. }),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_column_names_failure_on_persistent_500_reports_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/m9d7-ebf2.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = fast_client().with_metadata_base(server.uri());
        let err = client.fetch_column_names("m9d7-ebf2").await.unwrap_err();
        assert!(matches!(err, ClientError::RetriesExhausted { .. }));
    }
}
