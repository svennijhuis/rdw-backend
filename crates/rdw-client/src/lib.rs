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
}

const DEFAULT_METADATA_BASE: &str = "https://opendata.rdw.nl/api/views";

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

    /// Build the SODA URL for the fuel rows covering a vehicle batch's
    /// kenteken range (inclusive), ordered so the merge-join can validate
    /// per-vehicle sequencing.
    pub fn fuel_range_url(&self, lo: &str, hi: &str) -> String {
        let where_clause = format!(
            "kenteken >= '{}' AND kenteken <= '{}'",
            escape_soql(lo),
            escape_soql(hi)
        );
        format!(
            "{}/{FUEL_DATASET_ID}.json?$where={}&$order=kenteken,brandstof_volgnummer&$limit=50000",
            self.resource_base,
            urlencoding_soql(&where_clause)
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

    /// Fetch all fuel rows in a kenteken range with retry-with-backoff.
    pub async fn fetch_fuel_range(&self, lo: &str, hi: &str) -> Result<Vec<FuelRow>, ClientError> {
        let url = self.fuel_range_url(lo, hi);
        let rows = self.get_json_array(&url).await?;
        Ok(rows.into_iter().map(FuelRow).collect())
    }

    /// Fetch and parse the RDW dataset metadata used for CSV column headers.
    /// Callers apply their own fallback when this fails; this function only
    /// reports the raw error.
    pub async fn fetch_column_names(&self, dataset_id: &str) -> Result<Vec<String>, ClientError> {
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
                c.get("fieldName")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| ClientError::Decode("column missing fieldName".to_string()))
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
                    { "fieldName": "kenteken" },
                    { "fieldName": "merk" }
                ]
            })))
            .mount(&server)
            .await;

        let client = fast_client().with_metadata_base(server.uri());
        let columns = client.fetch_column_names("m9d7-ebf2").await.unwrap();
        assert_eq!(columns, vec!["kenteken".to_string(), "merk".to_string()]);
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
