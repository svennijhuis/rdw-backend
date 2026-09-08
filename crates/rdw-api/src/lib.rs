//! RDW fuel CSV API library: route wiring, extracted so integration tests
//! can build the same `Router` the binary serves without duplicating it.

pub mod errors;
pub mod extractors;
pub mod handlers;
pub mod pipeline;
pub mod state;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use state::AppState;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/fuel", get(handlers::fuel_handler))
        // CSV of this shape compresses about 13x, measured on real RDW data,
        // so gzip turns a ~700MB brand export into ~53MB on the wire. That is
        // most of the transfer time and most of the bandwidth bill. Clients
        // that do not send Accept-Encoding still get the plain bytes.
        .layer(tower_http::compression::CompressionLayer::new().gzip(true))
        .with_state(state)
}
