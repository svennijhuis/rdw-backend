//! Integration tests for `GET /api/v1/fuel`, driven through the real Axum
//! router with RDW mocked via wiremock. No live network calls.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rdw_api::pipeline::ConcurrentConfig;
use rdw_api::state::AppState;
use rdw_client::{RdwClient, RetryConfig};
use rdw_core::{Column, ColumnMetadata, FailureConfig};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "test-key-123";

fn test_metadata() -> ColumnMetadata {
    ColumnMetadata {
        vehicle_columns: vec![
            Column::new("kenteken", "Kenteken"),
            Column::new("merk", "Merk"),
        ],
        fuel_columns: vec![
            Column::new("kenteken", "Kenteken"),
            Column::new("brandstof_volgnummer", "Brandstof volgnummer"),
            Column::new("brandstof_omschrijving", "Brandstof omschrijving"),
        ],
        used_fallback: false,
    }
}

async fn build_app(server: &MockServer) -> axum::Router {
    build_app_with_state(server).await.0
}

/// Like `build_app`, but also returns the shared `AppState` so tests can
/// inspect the rate limiter after the request completes.
async fn build_app_with_state(server: &MockServer) -> (axum::Router, Arc<AppState>) {
    build_app_with_failure_config(server, FailureConfig::default()).await
}

/// Build the router with a custom fuel-failure threshold, so tests can
/// exercise both sides of the abort/continue boundary deterministically
/// instead of racing on process-global `FUEL_FAILURE_FLOOR`/`FUEL_FAILURE_RATIO`
/// environment variables.
async fn build_app_with_failure_config(
    server: &MockServer,
    failure_config: FailureConfig,
) -> (axum::Router, Arc<AppState>) {
    let client = RdwClient::new(None)
        .with_resource_base(format!("{}/resource", server.uri()))
        .with_retry_config(RetryConfig {
            max_attempts: 5,
            initial_backoff: std::time::Duration::ZERO,
            max_backoff: std::time::Duration::ZERO,
        });
    let mut valid_keys = HashSet::new();
    valid_keys.insert(API_KEY.to_string());
    let state = Arc::new(
        AppState::new(client, test_metadata(), valid_keys)
            .with_failure_config(failure_config)
            // A single range (rather than the production default of 64)
            // keeps wiremock's "any query gets the same fixture" behaviour
            // from replicating a mounted vehicle page across every range,
            // so the up-front `$select=count(kenteken)` mock (see
            // `mount_vehicle_count`) can name an exact, predictable expected
            // row count for the V.1 reconciliation. Scope C's own
            // multi-range behaviour is covered separately in
            // `pipeline::concurrent_tests`.
            .with_concurrent_config(ConcurrentConfig {
                range_count: 1,
                worker_count: 1,
                page_size: 50_000,
            }),
    );
    let app = rdw_api::build_router(state.clone()).layer(
        axum::extract::connect_info::MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))),
    );
    (app, state)
}

/// Mount a fuel-range mock that always fails with a persistent 500, so the
/// range's retries exhaust and the whole range is recorded as a fuel-fetch
/// failure.
async fn mount_fuel_range_failing(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/resource/8ys7-d773.csv"))
        .respond_with(ResponseTemplate::new(500))
        .mount(server)
        .await;
}

/// Build a CSV response body from a JSON array of objects, using `header` as
/// both the column order and the set of fields read from each object. Lets
/// call sites keep writing `json!([{ ... }])` fixtures while the wire format
/// underneath is Socrata's CSV, not JSON.
fn csv_body(header: &[&str], rows: &serde_json::Value) -> String {
    let mut body = header.join(",");
    body.push('\n');
    for row in rows.as_array().cloned().unwrap_or_default() {
        let obj = row.as_object().cloned().unwrap_or_default();
        let cells: Vec<String> = header
            .iter()
            .map(|h| {
                obj.get(*h)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        body.push_str(&cells.join(","));
        body.push('\n');
    }
    body
}

async fn mount_vehicle_page(server: &MockServer, vehicles: serde_json::Value) {
    // The up-front `$select=count(kenteken)` reconciliation query (V.1) is
    // issued once before any vehicle page is fetched, so every test that
    // mounts a vehicle page must also mount a matching count — otherwise the
    // export would fail before ever reaching the page fetch this function
    // sets up. The count matches this fixture's own row count exactly,
    // since `build_app_with_failure_config` pins `range_count: 1`.
    let expected_count = vehicles.as_array().map(|a| a.len()).unwrap_or(0) as u64;
    mount_vehicle_count(server, expected_count).await;
    Mock::given(method("GET"))
        .and(path("/resource/m9d7-ebf2.csv"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(csv_body(&["kenteken", "merk"], &vehicles)),
        )
        .mount(server)
        .await;
}

/// Mount the `$select=count(kenteken)` aggregate query (criterion V.1),
/// matched by query param since it shares the vehicle dataset's path with
/// the paged fetch `mount_vehicle_page` sets up.
async fn mount_vehicle_count(server: &MockServer, count: u64) {
    Mock::given(method("GET"))
        .and(path("/resource/m9d7-ebf2.csv"))
        .and(wiremock::matchers::query_param(
            "$select",
            "count(kenteken)",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("count_kenteken\n{count}\n")),
        )
        .mount(server)
        .await;
}

async fn mount_fuel_range(server: &MockServer, fuel: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/resource/8ys7-d773.csv"))
        .respond_with(ResponseTemplate::new(200).set_body_string(csv_body(
            &["kenteken", "brandstof_volgnummer", "brandstof_omschrijving"],
            &fuel,
        )))
        .mount(server)
        .await;
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn happy_path_valid_request_returns_csv() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range(
        &server,
        json!([{ "kenteken": "AA001A", "brandstof_volgnummer": "1", "brandstof_omschrijving": "Benzine" }]),
    )
    .await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("content-type").unwrap(), "text/csv");
    let body = body_string(response).await;
    assert!(body.contains("Kenteken,Merk"));
    assert!(body.contains("AA001A,TOYOTA,1,Benzine"));
}

#[tokio::test]
async fn edge_omitted_brands_fetches_all_three() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([])).await;
    mount_fuel_range(&server, json!([])).await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn failure_invalid_brand_returns_400() {
    let server = MockServer::start().await;
    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=ford&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn failure_missing_api_key_returns_401() {
    let server = MockServer::start().await;
    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/fuel?brands=toyota")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn failure_invalid_api_key_returns_401() {
    let server = MockServer::start().await;
    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/fuel?brands=toyota&api_key=wrong-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn edge_header_api_key_takes_precedence_over_query() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([])).await;
    mount_fuel_range(&server, json!([])).await;
    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/fuel?brands=toyota&api_key=wrong-key")
                .header("X-Api-Key", API_KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn failure_fourth_fuel_entry_causes_502_and_sends_no_csv() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range(
        &server,
        json!([
            { "kenteken": "AA001A", "brandstof_volgnummer": "1" },
            { "kenteken": "AA001A", "brandstof_volgnummer": "2" },
            { "kenteken": "AA001A", "brandstof_volgnummer": "3" },
            { "kenteken": "AA001A", "brandstof_volgnummer": "4" },
        ]),
    )
    .await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = body_string(response).await;
    assert!(
        !body.contains("Kenteken,Merk"),
        "no CSV bytes must be sent on a data-integrity failure"
    );
}

/// Criterion 5 (rdw-fuel-csv-api.md, amended by rdw-fuel-partial-failure.md):
/// a *vehicle*-page failure is unconditionally an abort, unlike a fuel-range
/// failure (which now degrades to a partial CSV below the failure
/// threshold). There is no row to mark for missing vehicles: up to 50,000
/// vehicles were never fetched at all, so the whole export still returns
/// 502 with zero CSV bytes and no quota consumed.
#[tokio::test]
async fn failure_vehicle_page_failure_returns_502_not_partial_csv() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/resource/m9d7-ebf2.csv"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = body_string(response).await;
    assert!(
        !body.contains("Kenteken,Merk"),
        "no CSV bytes must be sent when the vehicle page itself fails"
    );
}

// --- rdw-fuel-partial-failure.md: fuel-range failure degradation ---

/// Criterion 5: a single fuel-range failure with a permissive threshold
/// (ratio 1.0, so a single failed-out-of-one-attempted range never exceeds
/// it) degrades to a 200 with a partial CSV, rather than aborting.
#[tokio::test]
async fn fuel_page_failure_below_threshold_returns_200_partial_csv() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range_failing(&server).await;

    let (app, _state) = build_app_with_failure_config(
        &server,
        FailureConfig {
            floor: 3,
            ratio: 1.0,
        },
    )
    .await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("Export status"));
    assert!(body.contains("fuel_unavailable"));
}

/// Criterion 4/5: once fuel failures exceed `max(floor, ratio * attempted)`
/// the whole export aborts with 502 and no CSV bytes. A zero floor makes one
/// failed-out-of-one-attempted range exceed the allowance; under the default
/// floor of 3 the same scenario is deliberately tolerated and degrades to a
/// 200 instead, which `fuel_page_failure_below_threshold_returns_200_partial_csv`
/// covers.
#[tokio::test]
async fn fuel_page_failure_above_threshold_returns_502() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range_failing(&server).await;

    let (app, _state) = build_app_with_failure_config(
        &server,
        FailureConfig {
            floor: 0,
            ratio: 0.10,
        },
    )
    .await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = body_string(response).await;
    assert!(
        !body.contains("Kenteken,Merk"),
        "no CSV bytes must be sent once the fuel-failure threshold is exceeded"
    );
}

/// Criterion 2/3: within one successful fuel-range fetch, vehicles with
/// matching fuel rows and vehicles with none are marked `ok` and
/// `no_fuel_data` respectively in the same export (the `fuel_unavailable`
/// value is exercised by the failure-scenario tests above, since it
/// requires the whole range's fetch to fail).
#[tokio::test]
async fn multiple_fuel_pages_with_varied_failures_status_column_marked() {
    let server = MockServer::start().await;
    mount_vehicle_page(
        &server,
        json!([
            { "kenteken": "AA001A", "merk": "TOYOTA" },
            { "kenteken": "BB002B", "merk": "TOYOTA" },
        ]),
    )
    .await;
    mount_fuel_range(
        &server,
        json!([{ "kenteken": "AA001A", "brandstof_volgnummer": "1", "brandstof_omschrijving": "Benzine" }]),
    )
    .await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let ok_line = body
        .lines()
        .find(|l| l.starts_with("AA001A"))
        .expect("AA001A row present");
    assert!(ok_line.ends_with(",ok"));
    let no_fuel_line = body
        .lines()
        .find(|l| l.starts_with("BB002B"))
        .expect("BB002B row present");
    assert!(no_fuel_line.ends_with(",no_fuel_data"));
}

/// Criterion 6: the `X-Export-Warnings` header names both the fuel-failure
/// count and the number of vehicles affected.
#[tokio::test]
async fn response_header_x_export_warnings_counts_failures() {
    let server = MockServer::start().await;
    mount_vehicle_page(
        &server,
        json!([
            { "kenteken": "AA001A", "merk": "TOYOTA" },
            { "kenteken": "BB002B", "merk": "TOYOTA" },
        ]),
    )
    .await;
    mount_fuel_range_failing(&server).await;

    let (app, _state) = build_app_with_failure_config(
        &server,
        FailureConfig {
            floor: 3,
            ratio: 1.0,
        },
    )
    .await;
    // A `limit` keeps this on the sequential single-range path (Scope C's
    // concurrent ranges are only used for unlimited exports), so the two
    // vehicles are fetched, and their fuel fetched, as exactly one range —
    // matching this test's "one failed range" assertion below.
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/fuel?brands=toyota&limit=1000&api_key={API_KEY}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let warnings = response
        .headers()
        .get("x-export-warnings")
        .expect("X-Export-Warnings header present on a partial export")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(warnings, "fuel_failures=1 vehicles_affected=2");
}

/// Criterion 6, edge case: a fully successful export has no warnings header
/// at all.
#[tokio::test]
async fn edge_no_fuel_failures_omits_x_export_warnings_header() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range(&server, json!([])).await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-export-warnings").is_none());
}

/// Criterion 7: a degraded export's `Content-Disposition` filename is
/// marked `-PARTIAL`, since a browser download never surfaces response
/// headers to the user.
#[tokio::test]
async fn content_disposition_partial_filename_when_failure() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range_failing(&server).await;

    let (app, _state) = build_app_with_failure_config(
        &server,
        FailureConfig {
            floor: 3,
            ratio: 1.0,
        },
    )
    .await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let disposition = response
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.contains("fuel-export-PARTIAL.csv"));
}

/// Criterion 7, edge case: a clean export keeps the plain filename.
#[tokio::test]
async fn content_disposition_clean_filename_when_no_failure() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range(&server, json!([])).await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let disposition = response
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.contains("fuel-export.csv"));
    assert!(!disposition.contains("PARTIAL"));
}

/// Criterion 9: a partial (200) export still consumes rate-limit quota, the
/// same as a fully successful export. Proven end-to-end through real HTTP
/// requests (like `failure_rate_limit_exceeded_returns_429_on_fourth_request_same_day`)
/// rather than by inspecting the limiter with a mismatched clock value.
#[tokio::test]
async fn rate_limit_quota_consumed_on_200_partial() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range_failing(&server).await;

    let (app, _state) = build_app_with_failure_config(
        &server,
        FailureConfig {
            floor: 3,
            ratio: 1.0,
        },
    )
    .await;

    for i in 0..3 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "partial-success request {i} should still count against quota and succeed"
        );
    }

    let fourth = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        fourth.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "3 partial-success (200) requests must have exhausted the daily quota"
    );
}

/// Criterion 9: a 502 from the fuel-failure threshold being exceeded
/// releases quota, exactly like any other 502/504 upstream failure: many
/// more than 3 failed requests in a day must still all return 502, never
/// 429, because none of them consumed quota.
#[tokio::test]
async fn rate_limit_quota_released_on_fuel_threshold_502() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }])).await;
    mount_fuel_range_failing(&server).await;

    // A zero floor makes the single failed range exceed the allowance, so
    // every request ends in a threshold 502 and must release its quota.
    let (app, _state) = build_app_with_failure_config(
        &server,
        FailureConfig {
            floor: 0,
            ratio: 0.10,
        },
    )
    .await;

    for i in 0..5 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "request {i}: a released-quota 502 must never turn into 429"
        );
    }
}

#[tokio::test]
async fn failure_rate_limit_exceeded_returns_429_on_fourth_request_same_day() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([])).await;
    mount_fuel_range(&server, json!([])).await;
    let app = build_app(&server).await;

    for i in 0..3 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "request {i} should succeed within the daily limit"
        );
    }

    let fourth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fourth.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(fourth.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn failure_second_concurrent_export_returns_429_while_first_is_in_flight() {
    // Criterion 11: only one export runs at a time. The vehicle page mock
    // is delayed so the first request is still holding the export permit
    // when the second request arrives; we poll the semaphore's permit
    // count (rather than sleep-and-hope) so the assertion is deterministic.
    let server = MockServer::start().await;
    // Matched by query param, so it does not compete with the delayed
    // empty-page mock below: the up-front V.1 count query must resolve
    // (with a matching empty count) before this delay would otherwise apply.
    mount_vehicle_count(&server, 0).await;
    Mock::given(method("GET"))
        .and(path("/resource/m9d7-ebf2.csv"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(csv_body(&["kenteken", "merk"], &json!([])))
                .set_delay(std::time::Duration::from_millis(200)),
        )
        .mount(&server)
        .await;
    mount_fuel_range(&server, json!([])).await;

    let client = RdwClient::new(None)
        .with_resource_base(format!("{}/resource", server.uri()))
        .with_retry_config(RetryConfig {
            max_attempts: 5,
            initial_backoff: std::time::Duration::ZERO,
            max_backoff: std::time::Duration::ZERO,
        });
    let mut valid_keys = HashSet::new();
    valid_keys.insert(API_KEY.to_string());
    let state = Arc::new(AppState::new(client, test_metadata(), valid_keys));
    let app = rdw_api::build_router(state.clone()).layer(
        axum::extract::connect_info::MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))),
    );

    let first_app = app.clone();
    let first = tokio::spawn(async move {
        first_app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    });

    // Deterministically wait until the first request holds the export
    // permit, bounded by a short timeout rather than a fixed sleep.
    let mut waited = 0;
    while state.export_lock.available_permits() != 0 && waited < 500 {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        waited += 1;
    }
    assert_eq!(
        state.export_lock.available_permits(),
        0,
        "first export should be holding the single export permit"
    );

    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a concurrent second export must be rejected while the first is in flight"
    );

    let first_response = first.await.unwrap();
    assert_eq!(
        first_response.status(),
        StatusCode::OK,
        "the first export should still complete successfully once the mock responds"
    );

    // The guard serializes exports: once the first finishes, the permit is
    // released and a subsequent request is allowed through again.
    assert_eq!(
        state.export_lock.available_permits(),
        1,
        "the permit must be released once the first export completes"
    );
    let third = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=toyota&api_key={API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        third.status(),
        StatusCode::OK,
        "a request after the first export completes should be allowed through"
    );
}

#[tokio::test]
async fn edge_accept_html_error_renders_html_page() {
    let server = MockServer::start().await;
    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/fuel?brands=ford&api_key={API_KEY}"))
                .header("Accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    let body = body_string(response).await;
    assert!(body.starts_with("<!DOCTYPE html>"));
}

/// Performance regression guard: a small `?limit=` must not make the service
/// request a full 50,000-vehicle page. It previously did, then fetched every
/// fuel row in that page's kenteken range, so `?limit=200` moved hundreds of
/// thousands of rows to return 200 and took about 31 seconds in production.
/// The vehicle page must be requested at the size the caller can actually use.
#[tokio::test]
async fn small_limit_requests_a_small_vehicle_page_not_the_full_50000() {
    let server = MockServer::start().await;
    mount_vehicle_page(
        &server,
        json!([{ "kenteken": "AA001A", "merk": "TOYOTA" }, { "kenteken": "AA002B", "merk": "TOYOTA" }]),
    )
    .await;
    mount_fuel_range(
        &server,
        json!([{ "kenteken": "AA001A", "brandstof_volgnummer": "1", "brandstof_omschrijving": "Benzine" }]),
    )
    .await;

    let app = build_app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/fuel?brands=toyota&limit=2&api_key={API_KEY}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let vehicle_requests: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().contains("m9d7-ebf2"))
        .collect();
    assert!(
        !vehicle_requests.is_empty(),
        "the vehicle dataset must have been queried"
    );
    for request in &vehicle_requests {
        let url = request.url.as_str();
        assert!(
            url.contains("limit=2") || url.contains("limit%3D2") || url.contains("%242limit"),
            "vehicle page must be requested at the caller's limit, not 50000; got: {url}"
        );
        assert!(
            !url.contains("50000"),
            "a limit=2 export must never request a 50,000-row vehicle page; got: {url}"
        );
    }
}

/// The daily budget is per brand, not per request: spending Toyota's allowance
/// must leave Lexus untouched.
#[tokio::test]
async fn rate_limit_is_counted_per_brand_not_per_request() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([])).await;
    mount_fuel_range(&server, json!([])).await;
    let app = build_app(&server).await;

    let get = |brands: &str| {
        let app = app.clone();
        let uri = format!("/api/v1/fuel?brands={brands}&api_key={API_KEY}");
        async move {
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };

    // Exhaust Toyota's day budget.
    for i in 0..3 {
        assert_eq!(get("toyota").await, StatusCode::OK, "toyota request {i}");
    }
    assert_eq!(
        get("toyota").await,
        StatusCode::TOO_MANY_REQUESTS,
        "the fourth toyota request must be refused"
    );

    // Lexus has its own budget and is unaffected.
    assert_eq!(
        get("lexus").await,
        StatusCode::OK,
        "lexus must not be charged for toyota's requests"
    );
}

/// A request naming several brands draws from each of their budgets, and when
/// one of them is exhausted the others must not be charged for the refusal.
#[tokio::test]
async fn rate_limit_refusal_does_not_charge_the_other_brands() {
    let server = MockServer::start().await;
    mount_vehicle_page(&server, json!([])).await;
    mount_fuel_range(&server, json!([])).await;
    let app = build_app(&server).await;

    let get = |brands: &str| {
        let app = app.clone();
        let uri = format!("/api/v1/fuel?brands={brands}&api_key={API_KEY}");
        async move {
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };

    // Use up Suzuki only.
    for _ in 0..3 {
        assert_eq!(get("suzuki").await, StatusCode::OK);
    }

    // Asking for lexus+suzuki is refused because suzuki is exhausted.
    assert_eq!(get("lexus,suzuki").await, StatusCode::TOO_MANY_REQUESTS);

    // Lexus must still have its full budget: the refused request checked lexus
    // first, and that reservation has to be given back.
    for i in 0..3 {
        assert_eq!(
            get("lexus").await,
            StatusCode::OK,
            "lexus request {i} must still be allowed after the refused combined request"
        );
    }
}
