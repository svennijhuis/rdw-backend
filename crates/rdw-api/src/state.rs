//! Shared application state: the RDW client, cached column metadata, the
//! rate limiter, the accepted client API keys, and the single-export
//! concurrency guard.

use std::collections::HashSet;

use rdw_client::RdwClient;
use rdw_core::{ColumnMetadata, FailureConfig, RateLimiter};
use tokio::sync::Semaphore;

use crate::pipeline::ConcurrentConfig;

pub struct AppState {
    pub client: RdwClient,
    pub metadata: ColumnMetadata,
    pub rate_limiter: RateLimiter,
    pub valid_api_keys: HashSet<String>,
    /// Exactly one export runs at a time; a second concurrent request that
    /// cannot acquire this permit is rejected with 429 rather than queued.
    pub export_lock: Semaphore,
    /// The proportional fuel-failure abort threshold (`FUEL_FAILURE_FLOOR`,
    /// `FUEL_FAILURE_RATIO`), read once at startup.
    pub failure_config: FailureConfig,
    /// Above this many staged (gzip-compressed) bytes, a client that did not
    /// send `Accept-Encoding: gzip` is refused with 406 rather than served
    /// an uncompressed stream that could balloon to gigabytes in memory and
    /// wall time. Read once at startup from `UNCOMPRESSED_SIZE_THRESHOLD_MB`
    /// (default 50 MB); see `DEFAULT_UNCOMPRESSED_THRESHOLD_MB`.
    pub uncompressed_threshold_bytes: u64,
    /// Tuning for the concurrent range pipeline (Scope C), used only for an
    /// unlimited export (see `handlers::run`).
    pub concurrent_config: ConcurrentConfig,
}

/// Default `UNCOMPRESSED_SIZE_THRESHOLD_MB`, in MB.
pub const DEFAULT_UNCOMPRESSED_THRESHOLD_MB: u64 = 50;

/// Read `UNCOMPRESSED_SIZE_THRESHOLD_MB`, falling back to the default when
/// unset or unparsable.
pub fn uncompressed_threshold_bytes_from_env() -> u64 {
    std::env::var("UNCOMPRESSED_SIZE_THRESHOLD_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_UNCOMPRESSED_THRESHOLD_MB)
        * 1024
        * 1024
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
            failure_config: FailureConfig::from_env(),
            uncompressed_threshold_bytes: uncompressed_threshold_bytes_from_env(),
            concurrent_config: ConcurrentConfig::default(),
        }
    }

    /// Override the fuel-failure threshold. Test-only hook so integration
    /// tests can exercise both sides of the abort/continue boundary without
    /// racing on process-global environment variables.
    pub fn with_failure_config(mut self, failure_config: FailureConfig) -> Self {
        self.failure_config = failure_config;
        self
    }

    /// Override the uncompressed-response size threshold. Test-only hook so
    /// tests can exercise both sides of the 406 boundary deterministically,
    /// without racing on the process-global `UNCOMPRESSED_SIZE_THRESHOLD_MB`
    /// environment variable.
    pub fn with_uncompressed_threshold_bytes(mut self, bytes: u64) -> Self {
        self.uncompressed_threshold_bytes = bytes;
        self
    }

    /// Override the concurrent range pipeline's tuning. Test-only hook so
    /// tests can use a small `range_count`/`worker_count` deterministically
    /// instead of the production defaults.
    pub fn with_concurrent_config(mut self, concurrent_config: ConcurrentConfig) -> Self {
        self.concurrent_config = concurrent_config;
        self
    }
}
