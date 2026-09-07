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
        .with_state(state)
}
