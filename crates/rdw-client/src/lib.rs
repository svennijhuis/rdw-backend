//! HTTP client for the RDW open-data Socrata API.
//!
//! Provides keyset-paginated fetches of the vehicle and fuel datasets, with
//! bounded retry-with-backoff on transient failures. Never uses `$offset`
//! pagination: Socrata does not guarantee stable ordering across offset
//! pages, so all fetches page by `kenteken` (the vehicle plate) instead.

use std::time::Duration;

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
}

const DEFAULT_METADATA_BASE: &str = "https://opendata.rdw.nl/api/views";

/// Where the next fuel-range page resumes.
///
/// `inclusive` is true when the previous page's trailing kenteken group was
/// dropped because it may have been cut in half by the page limit: that whole
/// group still has to be fetched, so the next page must include it rather than
/// step past it.
#[derive(Debug, Clone, Copy)]
pub struct FuelCursor<'a> {
    pub kenteken: &'a str,
    pub inclusive: bool,
}

impl RdwClient {
    pub fn new(app_token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client with static config must build");
        Self {
            http,
            app_token,
            resource_base: SOCRATA_BASE.to_string(),
            metadata_base: DEFAULT_METADATA_BASE.to_string(),
            retry_config: RetryConfig::default(),
            fuel_page_limit: DEFAULT_FUEL_PAGE_LIMIT,
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
            "{}/{VEHICLE_DATASET_ID}.json?$where={}&$order=kenteken&$limit={limit}",
            self.resource_base,
            urlencoding_soql(&where_clause)
        )
    }

    /// Build the SODA URL for one keyset page of the fuel rows covering a
    /// vehicle batch's kenteken range (inclusive), ordered so the merge-join
    /// can validate per-vehicle sequencing. `after` is the `(kenteken,
    /// brandstof_volgnummer)` cursor from the previous page's last row, so a
    /// range larger than one page is fetched by resuming exactly where the
    /// prior page ended rather than restarting from `lo`.
    /// Build one fuel-range page URL.
    ///
    /// `after` is a kenteken, and the cursor clause is a plain text
    /// comparison on that column alone. It deliberately does NOT reference
    /// `brandstof_volgnummer`: RDW stores that column as TEXT, so comparing
    /// it against a number makes Socrata reject the whole query with
    /// `query.soql.type-mismatch` (verified live against opendata.rdw.nl),
    /// and comparing it as text would order "10" before "2". Whole kenteken
    /// groups are therefore the pagination unit; `fetch_fuel_range` never
    /// splits one group across pages.
    pub fn fuel_range_url(&self, lo: &str, hi: &str, after: Option<FuelCursor<'_>>) -> String {
        let mut where_clause = format!(
            "kenteken >= '{}' AND kenteken <= '{}'",
            escape_soql(lo),
            escape_soql(hi)
        );
        if let Some(cursor) = after {
            let op = if cursor.inclusive { ">=" } else { ">" };
            where_clause.push_str(&format!(
                " AND kenteken {op} '{}'",
                escape_soql(cursor.kenteken)
            ));
        }
        format!(
            "{}/{FUEL_DATASET_ID}.json?$where={}&$order=kenteken,brandstof_volgnummer&$limit={}",
            self.resource_base,
            urlencoding_soql(&where_clause),
            self.fuel_page_limit
        )
    }

    /// Fetch one keyset page of vehicles with retry-with-backoff.
    pub async fn fetch_vehicle_page(
        &self,
        merken: &[String],
        after_kenteken: Option<&str>,
        limit: u32,
    ) -> Result<Vec<VehicleRow>, ClientError> {
        let url = self.vehicle_page_url(merken, after_kenteken, limit);
        let rows = self.get_json_array(&url).await?;
        Ok(rows.into_iter().map(VehicleRow).collect())
    }

    /// Fetch every fuel row in a kenteken range, keyset-paginating past
    /// Socrata's per-request row cap. A response at exactly the configured
    /// page limit is treated as "there is more": the next page resumes from
    /// the last row's `(kenteken, brandstof_volgnummer)` cursor, splitting
    /// only between pages (never fabricating a boundary inside one
    /// kenteken's volgnummer group, since the cursor always resumes at the
    /// exact next row). Paging stops once a short page (fewer rows than the
    /// limit) is returned. Each page fetch retries with backoff via
    /// `get_json_array`; a page that exhausts its retries fails the whole
    /// range fetch.
    pub async fn fetch_fuel_range(&self, lo: &str, hi: &str) -> Result<Vec<FuelRow>, ClientError> {
        let mut all: Vec<FuelRow> = Vec::new();
        let mut cursor: Option<(String, bool)> = None;
        // A misbehaving upstream that ignores the cursor would otherwise spin
        // this loop forever and grow `all` without bound. Refusing to page
        // more times than the range could possibly contain turns that into a
        // clean error, so the export fails loudly instead of hanging or
        // emitting a silently wrong CSV.
        let mut pages_fetched: u32 = 0;
        const MAX_FUEL_PAGES: u32 = 10_000;

        loop {
            pages_fetched += 1;
            if pages_fetched > MAX_FUEL_PAGES {
                return Err(ClientError::Decode(format!(
                    "fuel range {lo}..{hi} exceeded {MAX_FUEL_PAGES} pages; \
                     upstream is not honouring the kenteken cursor"
                )));
            }
            let url = self.fuel_range_url(
                lo,
                hi,
                cursor.as_ref().map(|(k, inclusive)| FuelCursor {
                    kenteken: k.as_str(),
                    inclusive: *inclusive,
                }),
            );
            let mut page: Vec<FuelRow> = self
                .get_json_array(&url)
                .await?
                .into_iter()
                .map(FuelRow)
                .collect();
            let page_len = page.len() as u32;

            if page.is_empty() {
                break;
            }

            let last_kenteken = page
                .last()
                .and_then(|r| r.kenteken())
                .ok_or_else(|| ClientError::Decode("fuel row missing kenteken".to_string()))?
                .to_string();

            if page_len < self.fuel_page_limit {
                // Short page: the range is exhausted, so the trailing group is
                // whole and every row is kept.
                all.extend(page);
                break;
            }

            let first_kenteken = page
                .first()
                .and_then(|r| r.kenteken())
                .ok_or_else(|| ClientError::Decode("fuel row missing kenteken".to_string()))?
                .to_string();

            if first_kenteken == last_kenteken {
                // The page holds a single kenteken, so trimming it would leave
                // nothing and stall. Keep it and step strictly past it; one
                // plate has at most a handful of fuel rows, far below any sane
                // page limit, so this branch means the whole group is present.
                all.extend(page);
                cursor = Some((last_kenteken, false));
            } else {
                // A full page's trailing group may be cut in half. Drop it and
                // re-request it INCLUSIVELY on the next page, so it is fetched
                // whole rather than skipped.
                if cursor.as_ref().is_some_and(|(k, inclusive)| {
                    *inclusive && k.as_str() == last_kenteken.as_str()
                }) {
                    // The previous page already resumed inclusively at this
                    // same kenteken, so trimming again would make no progress.
                    return Err(ClientError::Decode(format!(
                        "fuel range {lo}..{hi} stalled at kenteken {last_kenteken}"
                    )));
                }
                page.retain(|r| r.kenteken() != Some(last_kenteken.as_str()));
                all.extend(page);
                cursor = Some((last_kenteken, true));
            }
        }

        Ok(all)
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

    fn fuel_json(kenteken: &str, volgnummer: u32) -> Value {
        json!({ "kenteken": kenteken, "brandstof_volgnummer": volgnummer.to_string() })
    }

    #[test]
    fn fuel_range_url_first_page_has_no_cursor_clause() {
        let url = RdwClient::new(None).fuel_range_url("AA001A", "ZZ999Z", None);
        assert!(!url.contains("kenteken+%3E") || !url.contains("brandstof_volgnummer+%3E"));
        assert!(url.contains("kenteken"));
    }

    #[test]
    fn fuel_range_url_with_cursor_encodes_after_clause() {
        let url = RdwClient::new(None).fuel_range_url(
            "AA001A",
            "ZZ999Z",
            Some(FuelCursor {
                kenteken: "BB002B",
                inclusive: false,
            }),
        );
        // The cursor clause resumes after a whole kenteken group and must
        // never compare brandstof_volgnummer: RDW stores that column as text,
        // so a numeric comparison makes Socrata reject the query outright.
        assert!(url.contains("BB002B"));
        assert!(!url.contains("brandstof_volgnummer+%3E"));
        assert!(!url.contains("brandstof_volgnummer >"));
    }

    /// Criterion 1, happy path: a fuel range under one page's limit is
    /// fetched in a single request and returned complete.
    #[tokio::test]
    async fn fetch_fuel_range_happy_path_single_page_under_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(vec![fuel_json("AA001A", 1), fuel_json("AA001A", 2)]),
            )
            .mount(&server)
            .await;

        let client = fast_client()
            .with_resource_base(format!("{}/resource", server.uri()))
            .with_fuel_page_limit(50_000);
        let rows = client.fetch_fuel_range("AA001A", "ZZ999Z").await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    /// Criterion 1, edge case: the fuel range boundary between two pages
    /// falls in the middle of one kenteken's volgnummer group (AA001A has 3
    /// entries but the first page ends after its 2nd). The cursor must
    /// resume with AA001A's 3rd entry rather than skipping or duplicating
    /// it, so the concatenated result never splits a group incorrectly.
    #[tokio::test]
    async fn fetch_fuel_range_edge_full_page_retries_the_trailing_group() {
        let server = MockServer::start().await;
        // Page 1 comes back at the full limit, so its trailing group (BB002B)
        // may be cut in half and must be dropped, not kept.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(vec![fuel_json("AA001A", 1), fuel_json("BB002B", 1)]),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Page 2 resumes INCLUSIVELY at BB002B and returns its complete group.
        // It fills the page, so the client asks once more.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(vec![fuel_json("BB002B", 1), fuel_json("BB002B", 2)]),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Page 3: nothing remains past BB002B, which ends the walk.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<Value>::new()))
            .mount(&server)
            .await;

        let client = fast_client()
            .with_resource_base(format!("{}/resource", server.uri()))
            .with_fuel_page_limit(2);
        let rows = client.fetch_fuel_range("AA001A", "ZZ999Z").await.unwrap();

        // BB002B's whole group appears exactly once: the truncated copy from
        // page 1 was dropped and the group re-fetched, not skipped or doubled.
        let kentekens: Vec<_> = rows.iter().filter_map(|r| r.kenteken()).collect();
        assert_eq!(kentekens, vec!["AA001A", "BB002B", "BB002B"]);
    }

    /// Criterion 1, failure/regression case: this is the exact live bug.
    /// Two full-limit pages followed by a short page must all be fetched;
    /// the previous implementation stopped after the first `$limit=50000`
    /// request and silently discarded the rest.
    #[tokio::test]
    async fn fetch_fuel_range_regression_keeps_paging_until_short_page() {
        let server = MockServer::start().await;

        // Seven kentekens, one fuel row each, fetched three at a time. Each
        // full page has its trailing group trimmed and re-requested
        // inclusively, so the mock pages mirror what Socrata would return for
        // those cursors.
        let k: Vec<String> = (0..7).map(|i| format!("K{i:05}")).collect();
        let page =
            |names: &[&String]| -> Vec<Value> { names.iter().map(|n| fuel_json(n, 1)).collect() };

        // Page 1: K0,K1,K2 (full) -> keep K0,K1, resume at >= K2.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(&[&k[0], &k[1], &k[2]])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Page 2 (>= K2): K2,K3,K4 (full) -> keep K2,K3, resume at >= K4.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(&[&k[2], &k[3], &k[4]])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Page 3 (>= K4): K4,K5,K6 (full) -> keep K4,K5, resume at >= K6.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(&[&k[4], &k[5], &k[6]])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Page 4 (>= K6): K6 alone, short -> keep and stop.
        Mock::given(method("GET"))
            .and(path(format!("/resource/{FUEL_DATASET_ID}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(&[&k[6]])))
            .mount(&server)
            .await;

        let client = fast_client()
            .with_resource_base(format!("{}/resource", server.uri()))
            .with_fuel_page_limit(3);
        let rows = client.fetch_fuel_range(&k[0], &k[6]).await.unwrap();

        let got: Vec<&str> = rows.iter().filter_map(|r| r.kenteken()).collect();
        let want: Vec<&str> = k.iter().map(String::as_str).collect();
        assert_eq!(
            got, want,
            "every kenteken must appear exactly once: no group dropped at a page boundary and none duplicated"
        );
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
