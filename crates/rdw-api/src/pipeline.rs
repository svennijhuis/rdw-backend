//! Orchestrates the fetch -> merge-join -> widen pipeline across keyset
//! pages. Never accumulates a batch-hash-join of the whole dataset: each
//! vehicle page's kenteken range drives one bounded fuel range fetch.

use rdw_client::{ClientError, RdwClient};
use rdw_core::{merge_join, Assembler, MergeJoinError, RowWidener};

/// One SODA page of vehicles per RDW call, capped at Socrata's own limit.
const VEHICLE_PAGE_LIMIT: u32 = 50_000;

#[derive(Debug)]
pub enum PipelineError {
    Timeout,
    Upstream(String),
    DataIntegrity(String),
    Assembly(String),
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
) -> Result<(), PipelineError> {
    if limit == Some(0) {
        return Ok(());
    }

    let mut produced: u64 = 0;
    let mut cursor: Option<String> = None;

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
        let fuel = client.fetch_fuel_range(&lo, &hi).await?;

        let widened = merge_join(&page, &fuel)?;
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
            return Ok(());
        }

        let page_len = page.len() as u32;
        cursor = page.last().and_then(|v| v.kenteken()).map(str::to_string);
        if page_len < VEHICLE_PAGE_LIMIT {
            break;
        }
    }

    Ok(())
}
