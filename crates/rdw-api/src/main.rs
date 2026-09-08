//! RDW fuel CSV API: Axum server exposing `GET /api/v1/fuel`.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use rdw_api::pipeline::{ConcurrentConfig, VEHICLE_PAGE_LIMIT};
use rdw_api::state::AppState;
use rdw_client::RdwClient;
use rdw_core::load_column_metadata;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Accepts either credential the RDW portal issues: an App Token
    // (X-App-Token) or an API Key / "API Sleutel" (key id + secret, HTTP
    // Basic). Putting a key secret in RDW_APP_TOKEN fails upstream with
    // "Invalid app_token specified", so both are supported explicitly.
    let credentials = rdw_client::RdwCredentials::from_env();
    match credentials {
        rdw_client::RdwCredentials::None => tracing::warn!(
            "no RDW credential set (RDW_API_KEY_ID + RDW_API_KEY_SECRET, or RDW_APP_TOKEN); \
             requests will use Socrata's unauthenticated tier"
        ),
        // describe() names the mechanism only; a credential is never logged.
        ref c => tracing::info!(credential = c.describe(), "RDW credential configured"),
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

    let fuel_concurrency = env_usize("FUEL_CONCURRENCY", rdw_client::DEFAULT_FUEL_CONCURRENCY);
    // Re-tuning FUEL_KENTEKEN_BATCH is measurement-gated (see
    // docs/plans/rdw-fuel-export-perf-cost.md Scope D): the default itself is
    // unchanged, only made overridable so a future measurement does not
    // require a code change.
    let fuel_kenteken_batch = env_usize("FUEL_KENTEKEN_BATCH", rdw_client::FUEL_KENTEKEN_BATCH);

    let client = RdwClient::with_credentials(credentials)
        .with_fuel_concurrency(fuel_concurrency)
        .with_fuel_kenteken_batch(fuel_kenteken_batch);
    let metadata = load_column_metadata(&client).await;
    if metadata.used_fallback {
        tracing::warn!(
            "using compiled-in fallback column metadata; RDW metadata fetch failed at startup"
        );
    }

    let default_concurrent = ConcurrentConfig::default();
    let concurrent_config = ConcurrentConfig {
        range_count: env_usize("FUEL_RANGE_COUNT", default_concurrent.range_count).max(1),
        worker_count: env_usize("FUEL_RANGE_WORKERS", default_concurrent.worker_count),
        // Clamped at the parse site. `worker_count` is clamped downstream in
        // the pipeline, but `page_size` was not, so a `FUEL_RANGE_PAGE_SIZE=0`
        // parsed cleanly and produced an export that fetched nothing and only
        // failed later, via the row-count reconciliation, as a misleading
        // mismatch. A size of zero is never meaningful.
        page_size: env_usize(
            "FUEL_RANGE_PAGE_SIZE",
            default_concurrent.page_size as usize,
        )
        .clamp(1, VEHICLE_PAGE_LIMIT as usize) as u32,
    };

    let state = Arc::new(
        AppState::new(client, metadata, valid_api_keys).with_concurrent_config(concurrent_config),
    );

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
