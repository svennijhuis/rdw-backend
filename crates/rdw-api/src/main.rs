//! RDW fuel CSV API: Axum server exposing `GET /api/v1/fuel`.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use rdw_api::state::AppState;
use rdw_client::RdwClient;
use rdw_core::load_column_metadata;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let app_token = std::env::var("RDW_APP_TOKEN").ok();
    if app_token.is_none() {
        tracing::warn!("RDW_APP_TOKEN not set; requests will use Socrata's unauthenticated tier");
    }

    let valid_api_keys: HashSet<String> = std::env::var("VALID_API_KEYS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if valid_api_keys.is_empty() {
        tracing::warn!("VALID_API_KEYS is empty; every request will be rejected with 401");
    }

    let client = RdwClient::new(app_token);
    let metadata = load_column_metadata(&client).await;
    if metadata.used_fallback {
        tracing::warn!(
            "using compiled-in fallback column metadata; RDW metadata fetch failed at startup"
        );
    }

    let state = Arc::new(AppState::new(client, metadata, valid_api_keys));

    // Hosting platforms (Vercel container runtime among them) inject the
    // listening port as PORT; SERVER_PORT stays supported for local runs.
    let port: u16 = std::env::var("PORT")
        .or_else(|_| std::env::var("SERVER_PORT"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let app = rdw_api::build_router(state);

    tracing::info!(%addr, "starting rdw-api");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind server address");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server error");
}
