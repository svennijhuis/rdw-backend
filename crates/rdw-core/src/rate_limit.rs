//! Fixed-window rate limiter: 3 requests/day and 5/week per key.
//!
//! Deliberately not the `governor` crate: GCRA leaks tokens continuously,
//! which turns "3/day" into effectively one request every 8 hours. A fixed
//! window resets exactly at the UTC calendar-day and calendar-week (Monday)
//! boundary instead, matching the product intent.
//!
//! Time is passed in explicitly (`now_unix` seconds since epoch) rather than
//! read from the system clock internally, so tests can control it exactly.

use dashmap::DashMap;

pub const DAY_LIMIT: u32 = 3;
pub const WEEK_LIMIT: u32 = 5;
const SECONDS_PER_DAY: i64 = 86_400;
/// Entries idle longer than this are evicted so the map does not grow
/// without bound from one-off IP keys.
pub const STALE_AFTER_SECS: i64 = 8 * SECONDS_PER_DAY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitOutcome {
    Allowed,
    DayExceeded,
    WeekExceeded,
}

#[derive(Debug, Clone, Copy, Default)]
struct WindowState {
    day_key: i64,
    day_count: u32,
    week_key: i64,
    week_count: u32,
    last_seen: i64,
}

/// A day index (days since the Unix epoch, UTC).
fn day_key(now_unix: i64) -> i64 {
    now_unix.div_euclid(SECONDS_PER_DAY)
}

/// A calendar-week index starting Monday 00:00 UTC. 1970-01-01 was a
/// Thursday, i.e. 3 days after that week's Monday, so shifting the day
/// count by 3 before dividing by 7 aligns week boundaries to Monday.
fn week_key(now_unix: i64) -> i64 {
    (day_key(now_unix) + 3).div_euclid(7)
}

#[derive(Default)]
pub struct RateLimiter {
    entries: DashMap<String, WindowState>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check both windows and, only if both are within limits, record this
    /// request against them. Returns which limit blocked the request when
    /// not allowed.
    pub fn check_and_record(&self, key: &str, now_unix: i64) -> RateLimitOutcome {
        let dk = day_key(now_unix);
        let wk = week_key(now_unix);
        let mut entry = self.entries.entry(key.to_string()).or_default();

        if entry.day_key != dk {
            entry.day_key = dk;
            entry.day_count = 0;
        }
        if entry.week_key != wk {
            entry.week_key = wk;
            entry.week_count = 0;
        }

        if entry.day_count >= DAY_LIMIT {
            return RateLimitOutcome::DayExceeded;
        }
        if entry.week_count >= WEEK_LIMIT {
            return RateLimitOutcome::WeekExceeded;
        }

        entry.day_count += 1;
        entry.week_count += 1;
        entry.last_seen = now_unix;
        RateLimitOutcome::Allowed
    }

    /// Undo a previously recorded request for this key's current windows.
    /// Used when the request ultimately failed with 502/504, which must
    /// not consume quota.
    pub fn release(&self, key: &str, now_unix: i64) {
        let dk = day_key(now_unix);
        let wk = week_key(now_unix);
        if let Some(mut entry) = self.entries.get_mut(key) {
            if entry.day_key == dk {
                entry.day_count = entry.day_count.saturating_sub(1);
            }
            if entry.week_key == wk {
                entry.week_count = entry.week_count.saturating_sub(1);
            }
        }
    }

    /// Remove entries that have not been seen in over `STALE_AFTER_SECS`,
    /// preventing unbounded growth from transient IP keys.
    pub fn evict_stale(&self, now_unix: i64) {
        self.entries
            .retain(|_, state| now_unix - state.last_seen <= STALE_AFTER_SECS);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = SECONDS_PER_DAY;

    #[test]
    fn happy_path_three_requests_per_day_allowed() {
        let limiter = RateLimiter::new();
        for _ in 0..3 {
            assert_eq!(limiter.check_and_record("k", 0), RateLimitOutcome::Allowed);
        }
    }

    #[test]
    fn failure_fourth_request_same_day_is_blocked() {
        let limiter = RateLimiter::new();
        for _ in 0..3 {
            limiter.check_and_record("k", 0);
        }
        assert_eq!(
            limiter.check_and_record("k", 100),
            RateLimitOutcome::DayExceeded
        );
    }

    #[test]
    fn edge_day_boundary_crossing_resets_daily_counter() {
        let limiter = RateLimiter::new();
        // 23:59:00 on day 0.
        let day_n_2359 = DAY - 60;
        for _ in 0..3 {
            assert_eq!(
                limiter.check_and_record("k", day_n_2359),
                RateLimitOutcome::Allowed
            );
        }
        assert_eq!(
            limiter.check_and_record("k", day_n_2359 + 30),
            RateLimitOutcome::DayExceeded
        );

        // 00:01:00 on day 1: new calendar day, daily counter resets.
        let day_n1_0001 = DAY + 60;
        assert_eq!(
            limiter.check_and_record("k", day_n1_0001),
            RateLimitOutcome::Allowed
        );
    }

    // 1970-01-01 (day 0) was a Thursday; day 4 is the next Monday, giving a
    // full Monday-Sunday week (days 4..=10) to spend the weekly budget in.
    const MONDAY: i64 = 4 * DAY;

    #[test]
    fn edge_week_boundary_crossing_resets_weekly_counter() {
        let limiter = RateLimiter::new();
        // Spend the weekly budget (5) across several distinct days.
        for day in 0..5 {
            assert_eq!(
                limiter.check_and_record("k", MONDAY + day * DAY),
                RateLimitOutcome::Allowed
            );
        }
        // Still within the same Monday-aligned week: blocked on the weekly limit.
        assert_eq!(
            limiter.check_and_record("k", MONDAY + 5 * DAY),
            RateLimitOutcome::WeekExceeded
        );

        // A full week later: a new Monday-aligned week, counter resets.
        assert_eq!(
            limiter.check_and_record("k", MONDAY + 7 * DAY),
            RateLimitOutcome::Allowed
        );
    }

    #[test]
    fn failure_sixth_request_same_week_is_blocked() {
        let limiter = RateLimiter::new();
        for day in 0..5 {
            limiter.check_and_record("k", MONDAY + day * DAY);
        }
        assert_eq!(
            limiter.check_and_record("k", MONDAY + 6 * DAY),
            RateLimitOutcome::WeekExceeded
        );
    }

    #[test]
    fn release_does_not_consume_quota_on_upstream_failure() {
        let limiter = RateLimiter::new();
        assert_eq!(limiter.check_and_record("k", 0), RateLimitOutcome::Allowed);
        limiter.release("k", 0);
        // The released request should not count toward the daily limit.
        for _ in 0..3 {
            assert_eq!(limiter.check_and_record("k", 1), RateLimitOutcome::Allowed);
        }
    }

    #[test]
    fn eviction_removes_stale_entries_but_keeps_recent_ones() {
        let limiter = RateLimiter::new();
        limiter.check_and_record("stale", 0);
        limiter.check_and_record("fresh", 10 * DAY);
        limiter.evict_stale(10 * DAY);
        assert_eq!(limiter.len(), 1);
        assert_eq!(
            limiter.check_and_record("fresh", 10 * DAY + 1),
            RateLimitOutcome::Allowed
        );
    }
}
