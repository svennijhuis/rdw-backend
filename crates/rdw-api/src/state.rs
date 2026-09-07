//! Shared application state: the RDW client, cached column metadata, the
//! rate limiter, the accepted client API keys, and the single-export
//! concurrency guard.

use std::collections::HashSet;

use rdw_client::RdwClient;
use rdw_core::{ColumnMetadata, RateLimiter};
use tokio::sync::Semaphore;

pub struct AppState {
    pub client: RdwClient,
    pub metadata: ColumnMetadata,
    pub rate_limiter: RateLimiter,
    pub valid_api_keys: HashSet<String>,
    /// Exactly one export runs at a time; a second concurrent request that
    /// cannot acquire this permit is rejected with 429 rather than queued.
    pub export_lock: Semaphore,
}

impl AppState {
    pub fn new(
        client: RdwClient,
        metadata: ColumnMetadata,
        valid_api_keys: HashSet<String>,
    ) -> Self {
        Self {
            client,
            metadata,
            rate_limiter: RateLimiter::new(),
            valid_api_keys,
            export_lock: Semaphore::new(1),
        }
    }
}
