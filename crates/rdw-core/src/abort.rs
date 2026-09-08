//! A single, export-wide fuel-failure counter and abort flag, shared across
//! every concurrently fetched kenteken range (Scope C).
//!
//! Per-range counters would multiply the floor of `FailureConfig` by the
//! number of ranges (e.g. floor 3 * 16 ranges = 48 tolerated failures), which
//! defeats the whole point of the threshold. `GlobalAbort` is cloned (cheap:
//! an `Arc`) into every range-worker task, and every one of them records into
//! and checks the SAME counters after each fuel fetch.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::failure::FailureConfig;

#[derive(Debug, Default)]
struct Counters {
    attempted: AtomicUsize,
    failures: AtomicUsize,
    aborted: AtomicBool,
}

/// Cheaply cloneable (an `Arc` around the shared counters); every clone
/// observes the same state.
#[derive(Debug, Clone, Default)]
pub struct GlobalAbort {
    inner: Arc<Counters>,
}

impl GlobalAbort {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one fuel-range fetch's outcome and re-evaluate the threshold
    /// against the up-to-date totals. Returns whether the export should
    /// abort as of this call (either it just tripped, or had already
    /// tripped from another range). Once tripped, `is_aborted` stays true
    /// for the lifetime of this `GlobalAbort` — an export never un-aborts.
    pub fn record_and_check(&self, failed: bool, config: &FailureConfig) -> bool {
        let attempted = self.inner.attempted.fetch_add(1, Ordering::SeqCst) + 1;
        let failures = if failed {
            self.inner.failures.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            self.inner.failures.load(Ordering::SeqCst)
        };
        if config.should_abort(failures, attempted) {
            self.inner.aborted.store(true, Ordering::SeqCst);
        }
        self.inner.aborted.load(Ordering::SeqCst)
    }

    /// True if the threshold has been exceeded by any range's fetch so far.
    /// Workers should check this before starting further work, not only
    /// after their own `record_and_check` call, so a range that has not yet
    /// failed still stops promptly once another range trips the threshold.
    pub fn is_aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::SeqCst)
    }

    pub fn attempted(&self) -> usize {
        self.inner.attempted.load(Ordering::SeqCst)
    }

    pub fn failures(&self) -> usize {
        self.inner.failures.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permissive() -> FailureConfig {
        FailureConfig {
            floor: 3,
            ratio: 0.10,
        }
    }

    fn strict() -> FailureConfig {
        FailureConfig {
            floor: 0,
            ratio: 0.0,
        }
    }

    #[test]
    fn happy_path_successes_never_trip_abort() {
        let abort = GlobalAbort::new();
        for _ in 0..100 {
            assert!(!abort.record_and_check(false, &permissive()));
        }
        assert!(!abort.is_aborted());
        assert_eq!(abort.attempted(), 100);
        assert_eq!(abort.failures(), 0);
    }

    #[test]
    fn edge_failures_within_the_floor_do_not_abort() {
        let abort = GlobalAbort::new();
        assert!(!abort.record_and_check(true, &permissive()));
        assert!(!abort.record_and_check(true, &permissive()));
        assert!(!abort.record_and_check(true, &permissive()));
        assert!(!abort.is_aborted());
    }

    #[test]
    fn failure_exceeding_the_floor_trips_abort_globally() {
        let abort = GlobalAbort::new();
        for _ in 0..4 {
            abort.record_and_check(true, &permissive());
        }
        assert!(abort.is_aborted());
    }

    #[test]
    fn failure_is_aborted_stays_true_even_after_a_later_success() {
        let abort = GlobalAbort::new();
        assert!(abort.record_and_check(true, &strict()));
        assert!(abort.is_aborted());
        // A later success from another range must not un-trip the abort.
        abort.record_and_check(false, &strict());
        assert!(abort.is_aborted());
    }

    #[test]
    fn happy_path_counters_are_shared_across_clones() {
        // Simulates every range-worker task holding its own clone of the
        // same `GlobalAbort`: a failure recorded through one clone must be
        // visible through every other.
        let abort = GlobalAbort::new();
        let worker_a = abort.clone();
        let worker_b = abort.clone();

        worker_a.record_and_check(true, &permissive());
        worker_b.record_and_check(true, &permissive());

        assert_eq!(abort.attempted(), 2);
        assert_eq!(abort.failures(), 2);
    }

    #[test]
    fn failure_a_single_range_hitting_the_floor_alone_does_not_multiply_by_range_count() {
        // Regression guard for the exact bug this module exists to prevent:
        // with a per-range counter, floor=3 tolerated PER RANGE would let a
        // 16-range export absorb 48 failures. With one shared counter, the
        // same floor=3 applies export-wide.
        let abort = GlobalAbort::new();
        let config = FailureConfig {
            floor: 3,
            ratio: 0.0,
        };
        // Four different "ranges" each contribute one failure; the fourth
        // must trip the shared threshold, not be tolerated as "this range's
        // first failure".
        assert!(!abort.record_and_check(true, &config));
        assert!(!abort.record_and_check(true, &config));
        assert!(!abort.record_and_check(true, &config));
        assert!(abort.record_and_check(true, &config));
    }
}
