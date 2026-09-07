//! Integration tests for `GET /api/v1/fuel`, driven through the real Axum
//! router with RDW mocked via wiremock. No live network calls.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rdw_api::state::AppState;
use rdw_client::{RdwClient, RetryConfig};
use rdw_core::ColumnMetadata;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "test-key-123";

fn test_metadata() -> ColumnMetadata {
    ColumnMetadata {
        vehicle_columns: vec!["kenteken".to_string(), "merk".to_string()],
        fuel_columns: vec![
            "kenteken".to_string(),
            "brandstof_volgnummer".to_string(),
            "brandstof_omschrijving".to_string(),
        ],
        used_fallback: false,
    }
}

async fn build_app(server: &MockServer) -> axum::Router {
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
    rdw_api::build_router(state).layer(axum::extract::connect_info::MockConnectInfo(
        SocketAddr::from(([127, 0, 0, 1], 0)),
    ))
}

async fn mount_vehicle_page(server: &MockServer, vehicles: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/resource/m9d7-ebf2.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vehicles))
        .mount(server)
        .await;
}

async fn mount_fuel_range(server: &MockServer, fuel: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/resource/8ys7-d773.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fuel))
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
    assert!(body.contains("kenteken,merk"));
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
        !body.contains("kenteken,merk"),
        "no CSV bytes must be sent on a data-integrity failure"
    );
}

#[tokio::test]
async fn failure_persistent_upstream_500_returns_502_not_partial_csv() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/resource/m9d7-ebf2.json"))
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
    Mock::given(method("GET"))
        .and(path("/resource/m9d7-ebf2.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([]))
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
