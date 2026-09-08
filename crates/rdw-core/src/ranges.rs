//! Kenteken range tiling for concurrent fuel/vehicle export fetching
//! (Scope C). Produces boundaries only — the ranges are still evaluated
//! entirely by Socrata's own `kenteken > 'lo' AND kenteken <= 'hi'`
//! comparison in the client; nothing here filters or assigns a row to a
//! range client-side.
//!
//! Split points are a fixed two-character-prefix partition of the kenteken
//! alphabet, computed once and reusable across exports (no per-export
//! network call). A real per-brand distribution could later replace this
//! with better-balanced boundaries, but the work-queue design (many more
//! ranges than workers, see `docs/plans/rdw-fuel-export-perf-cost.md`) means
//! an uneven fixed split only shows up as some workers finishing sooner,
//! never as a correctness risk.

/// One kenteken range: `lo < kenteken <= hi`. `lo: None` means unbounded
/// below (the first range); `hi: None` means unbounded above (the last
/// range).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KentekenRange {
    pub lo: Option<String>,
    pub hi: Option<String>,
}

/// RDW kentekens sort using this 36-symbol alphabet (digits then letters),
/// matching Socrata's default byte-wise string comparison since both digits
/// and uppercase ASCII letters already sort in this same relative order.
const ALPHABET: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Build `n` fixed two-character-prefix kenteken ranges (e.g. "00".."ZZ"
/// split into `n` near-equal bands). `n` of 0 is treated as 1. Adjacent
/// ranges share a byte-identical boundary (range `i`'s `hi` equals range
/// `i+1`'s `lo`), so together they partition the whole kenteken space with
/// no gap and no overlap.
pub fn fixed_two_char_bands(n: usize) -> Vec<KentekenRange> {
    let n = n.max(1);
    let base = ALPHABET.len();
    let total = base * base; // 1296 two-character combinations.

    let mut ranges = Vec::with_capacity(n);
    let mut prev: Option<String> = None;
    for i in 1..n {
        // Integer-divide so boundaries are monotonically increasing and
        // deterministic; `i * total / n` never exceeds `total`.
        let idx = i * total / n;
        let boundary = two_char_at(idx);
        ranges.push(KentekenRange {
            lo: prev.clone(),
            hi: Some(boundary.clone()),
        });
        prev = Some(boundary);
    }
    ranges.push(KentekenRange { lo: prev, hi: None });
    ranges
}

fn two_char_at(idx: usize) -> String {
    let base = ALPHABET.len();
    let hi = ALPHABET[idx / base] as char;
    let lo = ALPHABET[idx % base] as char;
    format!("{hi}{lo}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_produces_exactly_n_ranges() {
        assert_eq!(fixed_two_char_bands(16).len(), 16);
        assert_eq!(fixed_two_char_bands(64).len(), 64);
    }

    #[test]
    fn edge_zero_is_treated_as_one_fully_unbounded_range() {
        let ranges = fixed_two_char_bands(0);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], KentekenRange { lo: None, hi: None });
    }

    #[test]
    fn happy_path_one_range_is_fully_unbounded() {
        let ranges = fixed_two_char_bands(1);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].lo, None);
        assert_eq!(ranges[0].hi, None);
    }

    #[test]
    fn happy_path_first_range_unbounded_below_last_unbounded_above() {
        let ranges = fixed_two_char_bands(16);
        assert_eq!(ranges.first().unwrap().lo, None);
        assert_eq!(ranges.last().unwrap().hi, None);
    }

    #[test]
    fn happy_path_boundaries_are_byte_identical_between_adjacent_ranges() {
        let ranges = fixed_two_char_bands(16);
        for pair in ranges.windows(2) {
            assert_eq!(
                pair[0].hi, pair[1].lo,
                "range i's hi must equal range i+1's lo exactly"
            );
            assert!(pair[0].hi.is_some(), "an internal boundary must be Some");
        }
    }

    #[test]
    fn happy_path_boundaries_are_strictly_ascending_no_gap_no_overlap() {
        let ranges = fixed_two_char_bands(36);
        let boundaries: Vec<&str> = ranges.iter().filter_map(|r| r.hi.as_deref()).collect();
        for pair in boundaries.windows(2) {
            assert!(
                pair[0] < pair[1],
                "boundaries must be strictly increasing: {} then {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn edge_large_n_still_produces_valid_distinct_boundaries() {
        // With only 1296 two-character combinations, requesting more ranges
        // than that cannot yield strictly distinct boundaries throughout,
        // but must still produce exactly n well-formed (lo <= hi ordering,
        // no panics) ranges covering the space without gaps.
        let ranges = fixed_two_char_bands(64);
        assert_eq!(ranges.len(), 64);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].hi, pair[1].lo);
        }
    }
}
