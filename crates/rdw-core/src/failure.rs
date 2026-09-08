//! Fuel-range failure accounting and the proportional abort threshold.
//!
//! A vehicle-page fetch failure still aborts the whole export unconditionally
//! (no row to mark, only a silent gap). A fuel-range fetch failure degrades
//! instead: the vehicles in that range are already in hand and are emitted
//! with `export_status = fuel_unavailable`. This module tracks how many fuel
//! ranges failed and decides, after each one, whether the failure rate is
//! high enough that degrading further would produce a mostly-useless export.

/// The proportional abort threshold: `FUEL_FAILURE_FLOOR` (default 3) and
/// `FUEL_FAILURE_RATIO` (default 0.10), both overridable by environment
/// variable so operators can tune tolerance without a code change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FailureConfig {
    pub floor: usize,
    pub ratio: f64,
}

impl Default for FailureConfig {
    fn default() -> Self {
        Self {
            floor: 3,
            ratio: 0.10,
        }
    }
}

impl FailureConfig {
    /// Read `FUEL_FAILURE_FLOOR` and `FUEL_FAILURE_RATIO`, falling back to
    /// the default for either that is unset or fails to parse.
    pub fn from_env() -> Self {
        let default = Self::default();
        let floor = std::env::var("FUEL_FAILURE_FLOOR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default.floor);
        let ratio = std::env::var("FUEL_FAILURE_RATIO")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default.ratio);
        Self { floor, ratio }
    }

    /// Abort when `failures > max(floor, ratio * attempted)`.
    ///
    /// The two clauses are combined with `max`, not `or`. The floor exists
    /// precisely so that a small export is not destroyed by one transient
    /// failure: with an `or`, a single failure out of five attempts already
    /// exceeds a 10% ratio and would abort the whole export, which is the
    /// all-or-nothing behaviour this feature replaced. The floor therefore
    /// wins on small exports and the ratio takes over once the export is
    /// large enough for a percentage to be meaningful.
    ///
    /// Consequence worth knowing: an export whose every fuel fetch failed but
    /// which attempted no more than `floor` fetches is still delivered, fully
    /// marked `fuel_unavailable`. That is deliberate — `floor` failures are
    /// tolerated unconditionally, and the rows say so.
    pub fn should_abort(&self, failures: usize, attempted: usize) -> bool {
        let ratio_allowance = self.ratio * attempted as f64;
        let allowance = if ratio_allowance > self.floor as f64 {
            ratio_allowance
        } else {
            self.floor as f64
        };
        failures as f64 > allowance
    }
}

/// One fuel-range fetch that failed after exhausting retries: its kenteken
/// boundaries, how many vehicles in that range are affected, and when the
/// failure was recorded (Unix seconds), for the `_EXPORT_REPORT.txt` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedRange {
    pub lo: String,
    pub hi: String,
    pub vehicle_count: usize,
    pub failed_at_unix: i64,
}

/// Accumulated fuel-fetch outcome across every range in one export: how many
/// ranges were attempted, how many failed, how many vehicles were affected,
/// and the failed ranges themselves (for the ZIP report). Threaded from the
/// pipeline through to the HTTP response (headers, filename) and the ZIP
/// report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FuelFailureSummary {
    pub attempted: usize,
    pub failures: usize,
    pub vehicles_affected: usize,
    pub failed_ranges: Vec<FailedRange>,
}

impl FuelFailureSummary {
    pub fn has_failures(&self) -> bool {
        self.failures > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_default_thresholds_are_floor_3_ratio_0_10() {
        let config = FailureConfig::default();
        assert_eq!(config.floor, 3);
        assert_eq!(config.ratio, 0.10);
    }

    #[test]
    fn edge_small_export_is_protected_by_the_floor() {
        // 5 attempted, 1 failed. The ratio alone (0.20) exceeds 0.10, but the
        // floor is the larger allowance on an export this small, so one
        // transient failure must NOT destroy the whole export.
        let config = FailureConfig::default();
        assert!(!config.should_abort(1, 5));
    }

    #[test]
    fn edge_large_export_tolerates_up_to_the_ratio() {
        // 500 attempted, 30 failed: allowance is max(3, 50) = 50, and 30 does
        // not exceed it, so a 6% failure rate on a large export continues.
        let config = FailureConfig::default();
        assert!(!config.should_abort(30, 500));
    }

    #[test]
    fn failure_large_export_aborts_once_the_ratio_is_exceeded() {
        // 500 attempted, 60 failed: allowance is max(3, 50) = 50, and 60 > 50.
        let config = FailureConfig::default();
        assert!(config.should_abort(60, 500));
    }

    #[test]
    fn happy_path_below_both_floor_and_ratio_continues() {
        // 500 attempted, 2 failed: 2 <= floor(3) and 0.004 <= ratio(0.10).
        let config = FailureConfig::default();
        assert!(!config.should_abort(2, 500));
    }

    #[test]
    fn edge_all_attempts_failed_but_within_floor_is_still_delivered() {
        // 3 attempted, all 3 failed: the allowance is max(3, 0.3) = 3 and
        // 3 does not exceed 3, so this tiny export is still delivered with
        // every row marked fuel_unavailable. Tolerating `floor` failures
        // unconditionally is the documented trade-off for not destroying
        // small exports; the rows themselves state the degradation.
        let config = FailureConfig::default();
        assert!(!config.should_abort(3, 3));
    }

    #[test]
    fn failure_one_past_the_floor_on_a_tiny_export_aborts() {
        // 4 attempted, all 4 failed: allowance max(3, 0.4) = 3, and 4 > 3.
        let config = FailureConfig::default();
        assert!(config.should_abort(4, 4));
    }

    #[test]
    fn edge_zero_attempted_never_divides_by_zero() {
        let config = FailureConfig::default();
        assert!(!config.should_abort(0, 0));
    }

    #[test]
    fn fuel_failure_summary_has_failures_reflects_failure_count() {
        let mut summary = FuelFailureSummary::default();
        assert!(!summary.has_failures());
        summary.failures = 1;
        assert!(summary.has_failures());
    }

    // `from_env` mutates process-global environment variables, which race
    // against every other test in this binary if run in parallel. Serialize
    // just this one test's env access via a dedicated lock rather than
    // disabling parallelism for the whole crate.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn happy_path_from_env_reads_overrides_and_restores_defaults_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("FUEL_FAILURE_FLOOR");
        std::env::remove_var("FUEL_FAILURE_RATIO");
        assert_eq!(FailureConfig::from_env(), FailureConfig::default());

        std::env::set_var("FUEL_FAILURE_FLOOR", "7");
        std::env::set_var("FUEL_FAILURE_RATIO", "0.25");
        assert_eq!(
            FailureConfig::from_env(),
            FailureConfig {
                floor: 7,
                ratio: 0.25
            }
        );
        std::env::remove_var("FUEL_FAILURE_FLOOR");
        std::env::remove_var("FUEL_FAILURE_RATIO");
    }

    #[test]
    fn edge_from_env_unparsable_value_falls_back_to_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("FUEL_FAILURE_FLOOR", "not-a-number");
        assert_eq!(
            FailureConfig::from_env().floor,
            FailureConfig::default().floor
        );
        std::env::remove_var("FUEL_FAILURE_FLOOR");
    }
}
