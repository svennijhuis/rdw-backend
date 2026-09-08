//! Orchestrates the fetch -> merge-join -> widen pipeline across keyset
//! pages. Never accumulates a batch-hash-join of the whole dataset: each
//! vehicle page's kenteken range drives one bounded fuel range fetch.

use std::time::{SystemTime, UNIX_EPOCH};

use rdw_client::{ClientError, RdwClient};
use rdw_core::{
    merge_join, Assembler, FailedRange, FailureConfig, FuelFailureSummary, MergeJoinError,
    RowWidener,
};

/// One SODA page of vehicles per RDW call, capped at Socrata's own limit.
const VEHICLE_PAGE_LIMIT: u32 = 50_000;

#[derive(Debug)]
pub enum PipelineError {
    Timeout,
    Upstream(String),
    DataIntegrity(String),
    Assembly(String),
    /// The proportional fuel-failure threshold was exceeded: too many fuel
    /// ranges failed to keep degrading further. Zero tolerance, like a
    /// vehicle-page failure: the whole export aborts, no CSV is sent.
    FuelFailureThresholdExceeded(String),
}

impl From<ClientError> for PipelineError {
    fn from(err: ClientError) -> Self {
        match err {
            ClientError::Timeout => PipelineError::Timeout,
            other => PipelineError::Upstream(other.to_string()),
        }
    }
}

impl From<MergeJoinError> for PipelineError {
    fn from(err: MergeJoinError) -> Self {
        PipelineError::DataIntegrity(err.to_string())
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Fetch every vehicle page for `merken`, merge-join each page against its
/// fuel range, widen the result, and stage each page's rows to `assembler`
/// immediately, stopping early once `limit` output rows have been produced
/// (when given). Uses keyset pagination throughout: never `$offset`.
///
/// Never buffers more than one page's widened rows in memory: this is the
/// bounded-memory property the merge-join design relies on for a full,
/// multi-page RDW export. On any failure, the caller is responsible for
/// calling `assembler.abort()`; nothing is sent to a client until the
/// caller separately calls `assembler.finish()` after this returns `Ok`.
pub async fn fetch_and_widen(
    client: &RdwClient,
    merken: &[String],
    limit: Option<u64>,
    widener: &RowWidener,
    assembler: &mut Assembler,
    failure_config: &FailureConfig,
) -> Result<FuelFailureSummary, PipelineError> {
    if limit == Some(0) {
        return Ok(FuelFailureSummary::default());
    }

    let mut produced: u64 = 0;
    let mut cursor: Option<String> = None;
    let mut summary = FuelFailureSummary::default();

    loop {
        let page = client
            .fetch_vehicle_page(merken, cursor.as_deref(), VEHICLE_PAGE_LIMIT)
            .await?;
        if page.is_empty() {
            break;
        }

        let lo = page
            .first()
            .and_then(|v| v.kenteken())
            .unwrap_or_default()
            .to_string();
        let hi = page
            .last()
            .and_then(|v| v.kenteken())
            .unwrap_or_default()
            .to_string();

        summary.attempted += 1;
        let (fuel, fuel_fetch_failed) = match client.fetch_fuel_range(&lo, &hi).await {
            Ok(rows) => (rows, false),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    lo = %lo,
                    hi = %hi,
                    "fuel range fetch failed after retries; vehicles in this range will be marked fuel_unavailable"
                );
                summary.failures += 1;
                summary.vehicles_affected += page.len();
                summary.failed_ranges.push(FailedRange {
                    lo: lo.clone(),
                    hi: hi.clone(),
                    vehicle_count: page.len(),
                    failed_at_unix: now_unix(),
                });
                (Vec::new(), true)
            }
        };

        let widened = merge_join(&page, &fuel, fuel_fetch_failed)?;

        if failure_config.should_abort(summary.failures, summary.attempted) {
            return Err(PipelineError::FuelFailureThresholdExceeded(format!(
                "{} of {} fuel range fetches failed",
                summary.failures, summary.attempted
            )));
        }

        let mut batch: Vec<Vec<String>> = Vec::with_capacity(widened.len());
        let mut hit_limit = false;
        for w in &widened {
            batch.push(widener.widen(w));
            produced += 1;
            if let Some(lim) = limit {
                if produced >= lim {
                    hit_limit = true;
                    break;
                }
            }
        }
        assembler
            .write_rows(&batch)
            .map_err(|e| PipelineError::Assembly(e.to_string()))?;
        drop(batch);
        if hit_limit {
            return Ok(summary);
        }

        let page_len = page.len() as u32;
        cursor = page.last().and_then(|v| v.kenteken()).map(str::to_string);
        if page_len < VEHICLE_PAGE_LIMIT {
            break;
        }
    }

    Ok(summary)
}
