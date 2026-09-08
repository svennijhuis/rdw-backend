//! Orchestrates the fetch -> merge-join -> widen pipeline across keyset
//! pages. Never accumulates a batch-hash-join of the whole dataset: each
//! vehicle page's kenteken range drives one bounded fuel range fetch.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex as AsyncMutex;

use rdw_client::{ClientError, RdwClient};
use rdw_core::{
    fixed_two_char_bands, merge_join, Assembler, FailedRange, FailureConfig, FuelFailureSummary,
    GlobalAbort, KentekenRange, MergeJoinError, RowWidener,
};

/// One SODA page of vehicles per RDW call, capped at Socrata's own limit.
pub const VEHICLE_PAGE_LIMIT: u32 = 50_000;

/// End-to-end row-count reconciliation tolerance (criterion V.1).
///
/// The plan as written calls for strict equality — 502 on ANY mismatch
/// between the pre-export `$select=count(kenteken)` aggregate and the rows
/// actually emitted. That is deliberately NOT what is implemented here: a
/// full brand export takes on the order of 40 seconds against a dataset that
/// mutates continuously (vehicles get registered and de-registered), so the
/// two numbers legitimately differ by a small amount even when nothing is
/// broken. Strict equality would make every large export flaky. A mismatch
/// bigger than this tolerance, on the other hand, is what a gap or overlap
/// in the concurrent kenteken-range tiling (`fixed_two_char_bands`) would
/// actually look like, and that must still fail loudly.
///
/// The absolute floor: below this many rows of difference, always treat it
/// as ordinary data drift regardless of how small `expected` is.
const ROW_COUNT_TOLERANCE_FLOOR: u64 = 50;
/// The relative tolerance is `expected / ROW_COUNT_TOLERANCE_DIVISOR`, i.e.
/// one part in 2000 (0.05%) of the expected count. The effective tolerance
/// is whichever of the floor and this ratio is larger.
const ROW_COUNT_TOLERANCE_DIVISOR: u64 = 2000;

/// The allowed absolute difference between the expected and emitted row
/// counts before criterion V.1's reconciliation fails the export: 50 rows or
/// 0.05% of the expected count, whichever is larger. See
/// `ROW_COUNT_TOLERANCE_FLOOR` for why this is a tolerance and not strict
/// equality.
fn row_count_tolerance(expected: u64) -> u64 {
    ROW_COUNT_TOLERANCE_FLOOR.max(expected / ROW_COUNT_TOLERANCE_DIVISOR)
}

/// Lock a `std::sync::Mutex` without propagating poisoning. Every one of
/// these mutexes guards plain data that stays structurally valid even if a
/// worker panicked while holding it, so recovering the inner value is
/// strictly better than turning one panic into a panic in every other
/// worker. The panic itself is not swallowed: it surfaces as
/// `PipelineError::WorkerPanic` from the join loop.
fn lock_poison_tolerant<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    /// A concurrent range worker panicked. Reported explicitly rather than
    /// being inferred from a short export by the row-count reconciliation.
    WorkerPanic(String),
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
        // Ask for no more vehicles than the caller can still use. Without this
        // a `?limit=200` request fetched a full 50,000-vehicle page and then
        // every fuel row in that page's kenteken range — hundreds of thousands
        // of rows over a dozen RDW calls — only to emit 200 rows and stop. The
        // page size is the remaining limit, so a small export stays small.
        let page_size = match limit {
            Some(lim) => {
                let remaining = lim.saturating_sub(produced);
                if remaining == 0 {
                    break;
                }
                remaining.min(VEHICLE_PAGE_LIMIT as u64) as u32
            }
            None => VEHICLE_PAGE_LIMIT,
        };

        let page = client
            .fetch_vehicle_page(merken, cursor.as_deref(), page_size)
            .await?;
        if page.is_empty() {
            break;
        }

        // Ask for exactly this page's plates. A brand's kentekens are spread
        // across the whole kenteken space, so a range query would drag in
        // nearly the entire fuel dataset to find them.
        let kentekens: Vec<String> = page
            .iter()
            .filter_map(|v| v.kenteken())
            .map(str::to_string)
            .collect();
        let lo = kentekens.first().cloned().unwrap_or_default();
        let hi = kentekens.last().cloned().unwrap_or_default();

        summary.attempted += 1;
        let (fuel, fuel_fetch_failed) = match client.fetch_fuel_for_kentekens(&kentekens).await {
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

        // A short page means the brand filter is exhausted. Compare against the
        // size actually requested, not the constant, or a limit-shrunk page
        // would be mistaken for the end of the data.
        let page_len = page.len() as u32;
        cursor = page.last().and_then(|v| v.kenteken()).map(str::to_string);
        if page_len < page_size {
            break;
        }
    }

    Ok(summary)
}

/// Tuning knobs for the concurrent range pipeline (Scope C). Kept together
/// so the memory-bounded invariant (`page_size` scaling inversely with
/// `worker_count`) is documented at the one place both numbers are chosen.
#[derive(Debug, Clone, Copy)]
pub struct ConcurrentConfig {
    /// How many fixed kenteken ranges the kenteken space is split into. Much
    /// larger than `worker_count` (a work queue, not one range per worker),
    /// so an uneven fixed split only shows up as some workers finishing
    /// sooner, never as a correctness or memory risk.
    pub range_count: usize,
    /// How many ranges are actively being fetched at once.
    pub worker_count: usize,
    /// Vehicle rows requested per page WITHIN a range. Callers must scale
    /// this down as `worker_count` scales up, so `page_size * worker_count`
    /// does not exceed the memory footprint of one sequential-path page.
    pub page_size: u32,
}

impl Default for ConcurrentConfig {
    fn default() -> Self {
        Self {
            range_count: 64,
            worker_count: 16,
            page_size: 3_000,
        }
    }
}

/// Fetch every vehicle across `merken` using concurrent, unordered kenteken
/// ranges (Scope C), rather than the single sequential cursor `fetch_and_widen`
/// uses. Used only when the caller has no `limit` — a `limit` keeps the
/// existing sequential path, which naturally stops early once enough rows
/// are produced; unordered concurrent ranges cannot honor "stop after N"
/// without either overshooting or coordinating a global row counter, which
/// this plan does not add.
///
/// Every range is evaluated by Socrata's own `kenteken > 'lo' AND kenteken
/// <= 'hi'` `$where` clause; nothing here filters or assigns a vehicle row
/// to a range client-side. A `GlobalAbort` counter is shared (not per-range)
/// across every worker, so the proportional fuel-failure threshold applies
/// export-wide, not per range. Once it trips, in-flight workers stop
/// claiming new ranges and already-spawned tasks are cancelled via
/// `JoinSet::abort_all`, so the export stops burning further requests
/// instead of continuing to completion after already deciding to fail.
pub async fn fetch_and_widen_concurrent(
    client: &RdwClient,
    merken: &[String],
    widener: Arc<RowWidener>,
    assembler: Arc<AsyncMutex<Assembler>>,
    failure_config: &FailureConfig,
    config: &ConcurrentConfig,
) -> Result<FuelFailureSummary, PipelineError> {
    // Issued exactly once, up front, before any worker is spawned, using the
    // SAME `merk in(...)` `$where` clause the range pages use, so this really
    // counts the population the ranges are about to page through. A failure
    // here is an upstream failure like any other and must not be swallowed:
    // a reconciliation that silently disables itself when the count is
    // unavailable is not a guard at all.
    let expected_count = client.fetch_vehicle_count(merken).await?;
    let emitted_count = Arc::new(AtomicU64::new(0));

    let ranges = fixed_two_char_bands(config.range_count);
    let queue = Arc::new(AsyncMutex::new(VecDeque::from(ranges)));
    let abort = GlobalAbort::new();
    let summary = Arc::new(StdMutex::new(FuelFailureSummary::default()));
    let hard_error: Arc<StdMutex<Option<PipelineError>>> = Arc::new(StdMutex::new(None));
    let hard_error_flag = Arc::new(AtomicBool::new(false));

    let worker_count = config.worker_count.max(1).min(config.range_count.max(1));
    let mut join_set = tokio::task::JoinSet::new();

    for _ in 0..worker_count {
        let queue = queue.clone();
        let abort = abort.clone();
        let assembler = assembler.clone();
        let summary = summary.clone();
        let hard_error = hard_error.clone();
        let hard_error_flag = hard_error_flag.clone();
        let client = client.clone();
        let merken = merken.to_vec();
        let widener = widener.clone();
        let failure_config = *failure_config;
        let page_size = config.page_size;
        let emitted_count = emitted_count.clone();

        join_set.spawn(async move {
            loop {
                if abort.is_aborted() || hard_error_flag.load(Ordering::SeqCst) {
                    break;
                }
                let range = { queue.lock().await.pop_front() };
                let Some(range) = range else {
                    break;
                };
                let outcome = fetch_one_range(
                    &client,
                    &merken,
                    &range,
                    page_size,
                    &widener,
                    &assembler,
                    &abort,
                    &failure_config,
                    &summary,
                    &emitted_count,
                )
                .await;
                if let Err(e) = outcome {
                    *lock_poison_tolerant(&hard_error) = Some(e);
                    hard_error_flag.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
    }

    // Drain every worker, but cancel the rest the moment either an abort or
    // a hard error has been decided, rather than waiting for tasks already
    // mid-flight on other ranges to finish on their own.
    while !join_set.is_empty() {
        if abort.is_aborted() || hard_error_flag.load(Ordering::SeqCst) {
            join_set.abort_all();
        }
        // The JoinError must NOT be discarded. A panicking range worker used to
        // be swallowed here: no error, no abort, the export simply emitted
        // fewer rows and reported success, and only the V.1 row-count
        // reconciliation turned that into a confusing "row count mismatch".
        // A cancellation is expected (it is how `abort_all` above stops the
        // remaining workers) and is not an error; a panic is.
        if let Some(Err(join_err)) = join_set.join_next().await {
            if join_err.is_panic() {
                let mut slot = lock_poison_tolerant(&hard_error);
                if slot.is_none() {
                    *slot = Some(PipelineError::WorkerPanic(join_err.to_string()));
                }
                drop(slot);
                hard_error_flag.store(true, Ordering::SeqCst);
            }
        }
    }

    if let Some(e) = lock_poison_tolerant(&hard_error).take() {
        return Err(e);
    }
    if abort.is_aborted() {
        let attempted = abort.attempted();
        let failures = abort.failures();
        return Err(PipelineError::FuelFailureThresholdExceeded(format!(
            "{failures} of {attempted} fuel range fetches failed"
        )));
    }

    // Row-count reconciliation (criterion V.1). Counted as rows are written,
    // never accumulated, so this stays within the pipeline's bounded-memory
    // design.
    let emitted = emitted_count.load(Ordering::SeqCst);
    let tolerance = row_count_tolerance(expected_count);
    let diff = expected_count.abs_diff(emitted);
    tracing::info!(
        expected = expected_count,
        emitted,
        "row count reconciliation: pre-export count vs rows emitted"
    );
    if diff > 0 {
        tracing::warn!(
            expected = expected_count,
            emitted,
            diff,
            tolerance,
            "row count mismatch between the pre-export count and rows emitted"
        );
    }
    if diff > tolerance {
        return Err(PipelineError::DataIntegrity(format!(
            "expected {expected_count} vehicles (pre-export count) but emitted {emitted} rows \
             (diff {diff} exceeds tolerance {tolerance}); a kenteken-range boundary may have \
             gapped or overlapped"
        )));
    }

    Ok(Arc::try_unwrap(summary)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| lock_poison_tolerant(&arc).clone()))
}

/// Sequentially page through one fixed kenteken range's vehicles (and their
/// fuel), writing rows into the shared `assembler` as each vehicle page
/// completes. Mirrors `fetch_and_widen`'s per-page loop body, but scoped to
/// a range's boundaries instead of the whole brand, and checks the shared
/// `GlobalAbort` (not a local counter) after every fuel fetch.
#[allow(clippy::too_many_arguments)]
async fn fetch_one_range(
    client: &RdwClient,
    merken: &[String],
    range: &KentekenRange,
    page_size: u32,
    widener: &RowWidener,
    assembler: &AsyncMutex<Assembler>,
    abort: &GlobalAbort,
    failure_config: &FailureConfig,
    summary: &StdMutex<FuelFailureSummary>,
    emitted_count: &AtomicU64,
) -> Result<(), PipelineError> {
    let mut cursor: Option<String> = None;

    loop {
        if abort.is_aborted() {
            return Ok(());
        }

        let page = client
            .fetch_vehicle_range_page(
                merken,
                range.lo.as_deref(),
                range.hi.as_deref(),
                cursor.as_deref(),
                page_size,
            )
            .await?;
        if page.is_empty() {
            break;
        }

        let kentekens: Vec<String> = page
            .iter()
            .filter_map(|v| v.kenteken())
            .map(str::to_string)
            .collect();
        let lo = kentekens.first().cloned().unwrap_or_default();
        let hi = kentekens.last().cloned().unwrap_or_default();

        let (fuel, fuel_fetch_failed) = match client.fetch_fuel_for_kentekens(&kentekens).await {
            Ok(rows) => (rows, false),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    lo = %lo,
                    hi = %hi,
                    "fuel range fetch failed after retries; vehicles in this range will be marked fuel_unavailable"
                );
                {
                    let mut summary = lock_poison_tolerant(summary);
                    summary.failures += 1;
                    summary.vehicles_affected += page.len();
                    summary.failed_ranges.push(FailedRange {
                        lo: lo.clone(),
                        hi: hi.clone(),
                        vehicle_count: page.len(),
                        failed_at_unix: now_unix(),
                    });
                }
                (Vec::new(), true)
            }
        };

        {
            let mut summary = lock_poison_tolerant(summary);
            summary.attempted += 1;
        }

        // Checked after EVERY fuel fetch from ANY range, against the ONE
        // shared counter: a per-range counter would multiply the abort
        // floor by the number of ranges.
        if abort.record_and_check(fuel_fetch_failed, failure_config) {
            return Ok(());
        }

        let widened = merge_join(&page, &fuel, fuel_fetch_failed)?;
        let batch: Vec<Vec<String>> = widened.iter().map(|w| widener.widen(w)).collect();
        let rows_in_batch = batch.len() as u64;
        {
            let mut assembler = assembler.lock().await;
            assembler
                .write_rows(&batch)
                .map_err(|e| PipelineError::Assembly(e.to_string()))?;
        }
        // Count rows as they are written rather than accumulating them, so
        // the row-count reconciliation (criterion V.1) stays within the
        // pipeline's bounded-memory design.
        emitted_count.fetch_add(rows_in_batch, Ordering::SeqCst);

        let page_len = page.len() as u32;
        cursor = page.last().and_then(|v| v.kenteken()).map(str::to_string);
        if page_len < page_size {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod concurrent_tests {
    //! Wiremock answers ANY query regardless of its `$where` clause, so
    //! these tests prove the concurrent pipeline's LOGIC — the work queue is
    //! drained, rows are merged and staged, a fuel failure degrades a range,
    //! the global abort trips and stops further ranges — never that the
    //! range boundaries Socrata actually receives are correct (that is
    //! `rdw_client`'s `vehicle_range_page_url` unit tests, and ultimately a
    //! real call against opendata.rdw.nl).

    use super::*;
    use rdw_client::RetryConfig;
    use rdw_core::{Column, RowWidener};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A panic while a worker holds one of the shared std mutexes must not
    /// cascade: recovering the guarded value is strictly better than turning
    /// one panicking worker into a panic in every other worker. The panic
    /// itself is still reported, by the join loop, as `WorkerPanic`.
    #[test]
    fn edge_poisoned_mutex_is_recovered_rather_than_cascading() {
        let m = Arc::new(StdMutex::new(vec![1_u32, 2, 3]));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("worker died holding the lock");
        })
        .join();
        assert!(m.lock().is_err(), "the mutex must genuinely be poisoned");
        assert_eq!(
            *lock_poison_tolerant(&m),
            vec![1, 2, 3],
            "the guarded data is still intact and must be recoverable"
        );
    }

    /// A cancelled task is how `abort_all` stops the remaining workers, so it
    /// must never be mistaken for a panic and reported as one.
    #[tokio::test]
    async fn edge_cancelled_task_is_not_reported_as_a_panic() {
        let mut js = tokio::task::JoinSet::new();
        js.spawn(async {
            std::future::pending::<()>().await;
        });
        js.abort_all();
        let joined = js.join_next().await.expect("one task was spawned");
        let err = joined.expect_err("an aborted task returns a JoinError");
        assert!(err.is_cancelled());
        assert!(
            !err.is_panic(),
            "cancellation must not be classified as a panic"
        );
    }

    /// A panicking task's JoinError must be classified as a panic, which is
    /// the condition the join loop uses to raise `WorkerPanic` instead of
    /// silently emitting a short export.
    #[tokio::test]
    async fn failure_panicking_task_is_classified_as_a_panic() {
        let mut js = tokio::task::JoinSet::new();
        js.spawn(async {
            panic!("range worker blew up");
        });
        let joined = js.join_next().await.expect("one task was spawned");
        let err = joined.expect_err("a panicking task returns a JoinError");
        assert!(
            err.is_panic(),
            "must be classified as a panic, not cancelled"
        );
        // This is exactly what the join loop stores.
        let mapped = PipelineError::WorkerPanic(err.to_string());
        assert!(matches!(mapped, PipelineError::WorkerPanic(_)));
    }

    fn widener() -> Arc<RowWidener> {
        Arc::new(RowWidener::new(
            vec![
                Column::new("kenteken", "Kenteken"),
                Column::new("merk", "Merk"),
            ],
            vec![Column::new("kenteken", "Kenteken")],
        ))
    }

    fn fast_client(server: &MockServer) -> RdwClient {
        RdwClient::new(None)
            .with_resource_base(format!("{}/resource", server.uri()))
            .with_retry_config(RetryConfig {
                max_attempts: 3,
                initial_backoff: std::time::Duration::ZERO,
                max_backoff: std::time::Duration::ZERO,
            })
    }

    /// Every worker keeps pulling ranges from the shared queue until it is
    /// empty: with more ranges than workers, every range must still
    /// eventually be visited (the work-queue design in criterion C.4).
    #[tokio::test]
    async fn happy_path_work_queue_drains_every_range_with_fewer_workers_than_ranges() {
        let server = MockServer::start().await;
        // 8 ranges, 1 vehicle emitted per range: the up-front count query
        // (issued once, before any worker) must match this exactly, since
        // `mount_vehicle_count` is defined further below in this module.
        mount_vehicle_count(&server, 8).await;
        // First page returns one vehicle, second page (the cursor having
        // advanced) returns empty, ending that range's pagination.
        // Every range's first page comes back with exactly one row. With
        // `page_size` (100) far larger than that, each page is "short" and
        // that range's pagination ends after this single request — so
        // wiremock's inability to tell ranges apart does not matter here:
        // every range only ever needs this one response.
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,merk\nAA001A,TOYOTA\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/resource/8ys7-d773.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,brandstof_volgnummer\n"),
            )
            .mount(&server)
            .await;

        let client = fast_client(&server);
        let assembler = Arc::new(AsyncMutex::new(Assembler::new(widener().header())));
        let config = ConcurrentConfig {
            range_count: 8,
            worker_count: 2,
            page_size: 100,
        };
        let summary = fetch_and_widen_concurrent(
            &client,
            &["TOYOTA".to_string()],
            widener(),
            assembler.clone(),
            &FailureConfig::default(),
            &config,
        )
        .await
        .unwrap();

        // Every one of the 8 ranges made at least its first-page request,
        // proving the 2-worker queue drained the full 8-range work list
        // rather than stopping after 2.
        assert_eq!(summary.attempted, 8, "every range must have been visited");
        let requests = server.received_requests().await.unwrap();
        let vehicle_requests = requests
            .iter()
            .filter(|r| r.url.path().contains("m9d7-ebf2"))
            .count();
        assert!(
            vehicle_requests >= 8,
            "expected at least one vehicle request per range, got {vehicle_requests}"
        );
    }

    /// A fuel-range failure below the abort threshold still degrades that
    /// range's vehicles to `fuel_unavailable` and continues the rest of the
    /// export, exactly like the sequential path.
    #[tokio::test]
    async fn edge_one_failed_range_below_threshold_degrades_but_continues() {
        let server = MockServer::start().await;
        // 4 ranges, 1 vehicle emitted per range regardless of the fuel
        // fetch failure (merge_join still emits a `fuel_unavailable` row).
        mount_vehicle_count(&server, 4).await;
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,merk\nAA001A,TOYOTA\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/resource/8ys7-d773.csv"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = fast_client(&server);
        let assembler = Arc::new(AsyncMutex::new(Assembler::new(widener().header())));
        // A generous floor tolerates every one of these small ranges failing.
        let config = ConcurrentConfig {
            range_count: 4,
            worker_count: 2,
            page_size: 100,
        };
        let summary = fetch_and_widen_concurrent(
            &client,
            &["TOYOTA".to_string()],
            widener(),
            assembler,
            &FailureConfig {
                floor: 100,
                ratio: 1.0,
            },
            &config,
        )
        .await
        .unwrap();

        assert_eq!(summary.attempted, 4);
        assert_eq!(summary.failures, 4, "every range's fuel fetch failed");
        assert!(summary.has_failures());
    }

    /// Once the GLOBAL abort threshold trips (from any range), workers must
    /// stop claiming further ranges: with a floor of 0 the very first
    /// failure trips it, so strictly fewer than `range_count` ranges should
    /// ever be attempted.
    #[tokio::test]
    async fn failure_global_abort_stops_workers_from_claiming_further_ranges() {
        let server = MockServer::start().await;
        // The abort trips before reconciliation is ever reached, so the
        // exact count value here is unused; it only needs to succeed.
        mount_vehicle_count(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,merk\nAA001A,TOYOTA\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/resource/8ys7-d773.csv"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = fast_client(&server);
        let assembler = Arc::new(AsyncMutex::new(Assembler::new(widener().header())));
        let config = ConcurrentConfig {
            range_count: 64,
            worker_count: 1,
            page_size: 100,
        };
        let err = fetch_and_widen_concurrent(
            &client,
            &["TOYOTA".to_string()],
            widener(),
            assembler,
            &FailureConfig {
                floor: 0,
                ratio: 0.0,
            },
            &config,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            err,
            PipelineError::FuelFailureThresholdExceeded(_)
        ));
        let requests = server.received_requests().await.unwrap();
        let vehicle_requests = requests
            .iter()
            .filter(|r| r.url.path().contains("m9d7-ebf2"))
            .count();
        assert!(
            vehicle_requests < 64,
            "the abort must stop the single worker from claiming every one of the 64 ranges; got {vehicle_requests} vehicle requests"
        );
    }

    // --- Criterion V.1: end-to-end row-count reconciliation ---
    //
    // These mount a SEPARATE mock for the `$select=count(kenteken)` request
    // (matched by query param, since it shares the vehicle dataset's path
    // with the paged fetch) so the expected count can be set independently
    // of how many rows the ranges actually emit.

    use wiremock::matchers::query_param;

    async fn mount_vehicle_count(server: &MockServer, count: u64) {
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .and(query_param("$select", "count(kenteken)"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!("count_kenteken\n{count}\n")),
            )
            .mount(server)
            .await;
    }

    /// One range, one vehicle row emitted: expected count of 1 matches
    /// exactly.
    #[tokio::test]
    async fn happy_path_exact_row_count_match_succeeds() {
        let server = MockServer::start().await;
        mount_vehicle_count(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,merk\nAA001A,TOYOTA\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/resource/8ys7-d773.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,brandstof_volgnummer\n"),
            )
            .mount(&server)
            .await;

        let client = fast_client(&server);
        let assembler = Arc::new(AsyncMutex::new(Assembler::new(widener().header())));
        let config = ConcurrentConfig {
            range_count: 1,
            worker_count: 1,
            page_size: 100,
        };
        fetch_and_widen_concurrent(
            &client,
            &["TOYOTA".to_string()],
            widener(),
            assembler,
            &FailureConfig::default(),
            &config,
        )
        .await
        .expect("an exact row-count match must succeed");
    }

    /// A difference inside tolerance (well under 50 rows, and under 0.05% of
    /// a large expected count) is data drift, not a bug: the export must
    /// still succeed.
    #[tokio::test]
    async fn edge_row_count_mismatch_inside_tolerance_still_succeeds() {
        let server = MockServer::start().await;
        // Expected far larger than emitted (1), but the tolerance floor of
        // 50 covers this gap.
        mount_vehicle_count(&server, 40).await;
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,merk\nAA001A,TOYOTA\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/resource/8ys7-d773.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,brandstof_volgnummer\n"),
            )
            .mount(&server)
            .await;

        let client = fast_client(&server);
        let assembler = Arc::new(AsyncMutex::new(Assembler::new(widener().header())));
        let config = ConcurrentConfig {
            range_count: 1,
            worker_count: 1,
            page_size: 100,
        };
        fetch_and_widen_concurrent(
            &client,
            &["TOYOTA".to_string()],
            widener(),
            assembler,
            &FailureConfig::default(),
            &config,
        )
        .await
        .expect("a mismatch inside tolerance must still deliver the export");
    }

    /// A difference far outside tolerance (a boundary gap/overlap) must fail
    /// the export with `DataIntegrity`, which maps to a 502 upstream.
    #[tokio::test]
    async fn failure_row_count_mismatch_outside_tolerance_returns_data_integrity_error() {
        let server = MockServer::start().await;
        // Expected 10000 but only 1 row will ever be emitted: far outside
        // both the floor (50) and the 0.05% ratio (5) tolerance.
        mount_vehicle_count(&server, 10_000).await;
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,merk\nAA001A,TOYOTA\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/resource/8ys7-d773.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,brandstof_volgnummer\n"),
            )
            .mount(&server)
            .await;

        let client = fast_client(&server);
        let assembler = Arc::new(AsyncMutex::new(Assembler::new(widener().header())));
        let config = ConcurrentConfig {
            range_count: 1,
            worker_count: 1,
            page_size: 100,
        };
        let err = fetch_and_widen_concurrent(
            &client,
            &["TOYOTA".to_string()],
            widener(),
            assembler,
            &FailureConfig::default(),
            &config,
        )
        .await
        .expect_err("a mismatch far outside tolerance must fail the export");

        assert!(
            matches!(err, PipelineError::DataIntegrity(_)),
            "expected DataIntegrity, got {err:?}"
        );
    }

    /// The sequential `?limit=N` path never issues a count query at all, so
    /// a would-be mismatch (an intentionally truncated export emits fewer
    /// rows than the brand's real total) never trips any reconciliation.
    #[tokio::test]
    async fn edge_sequential_limited_path_is_unaffected_by_any_count_mismatch() {
        let server = MockServer::start().await;
        // If the sequential path ever queried this, it would see a huge
        // expected count that could never match the limited output — but it
        // must not query it at all.
        mount_vehicle_count(&server, 999_999).await;
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("kenteken,merk\nAA001A,TOYOTA\nAA002A,TOYOTA\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/resource/8ys7-d773.csv"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("kenteken,brandstof_volgnummer\n"),
            )
            .mount(&server)
            .await;

        let client = fast_client(&server);
        let mut assembler = Assembler::new(widener().header());
        fetch_and_widen(
            &client,
            &["TOYOTA".to_string()],
            Some(1),
            &widener(),
            &mut assembler,
            &FailureConfig::default(),
        )
        .await
        .expect("a limited export must succeed even though it emits far fewer rows than the brand total");

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .all(|r| r.url.query().is_none_or(|q| !q.contains("count"))),
            "the sequential limited path must never issue a count query"
        );
    }

    /// If the count query itself fails upstream, that is treated like any
    /// other upstream failure: the export fails rather than silently
    /// skipping reconciliation.
    #[tokio::test]
    async fn failure_count_query_failure_fails_the_export() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/resource/m9d7-ebf2.csv"))
            .and(query_param("$select", "count(kenteken)"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        // No vehicle-page mock: if a worker were ever spawned before the
        // count query resolves, this test would still catch it, since no
        // page mock exists to satisfy it silently.

        let client = fast_client(&server);
        let assembler = Arc::new(AsyncMutex::new(Assembler::new(widener().header())));
        let config = ConcurrentConfig {
            range_count: 4,
            worker_count: 2,
            page_size: 100,
        };
        let err = fetch_and_widen_concurrent(
            &client,
            &["TOYOTA".to_string()],
            widener(),
            assembler,
            &FailureConfig::default(),
            &config,
        )
        .await
        .expect_err("a failing count query must fail the export, not degrade to a warning");

        assert!(
            !matches!(err, PipelineError::DataIntegrity(_)),
            "a failed count query is an upstream failure, not a data-integrity mismatch: {err:?}"
        );
    }
}
