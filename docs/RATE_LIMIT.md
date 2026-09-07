# Rate limiting

## Why fixed-window, not `governor` (GCRA)

The `governor` crate implements GCRA, a leaky-bucket-style algorithm that grants one token every
`period / capacity`. Configured naively for "3 requests/day", that becomes roughly one allowed
request every 8 hours rather than an actual daily quota with three uses. A fixed window matches the
product intent directly: exactly 3 requests are usable between UTC midnight and the next UTC
midnight, and the counter resets cleanly at the boundary. This is why `governor` is intentionally
**not** a dependency of this workspace.

## How it works

Each API key gets two independent fixed-window counters, both checked on every request:

- **Daily window**: resets at 00:00:00 UTC (calendar day boundary). Limit: 3 requests.
- **Weekly window**: resets at Monday 00:00:00 UTC (calendar week boundary). Limit: 5 requests.

A request is allowed only when both counters are under their limit; it then increments both. If a
request ultimately fails with a 502 or 504 (an RDW upstream failure), its quota consumption is
reversed — upstream failures do not cost the client a request.

## Storage and eviction

State lives entirely in memory (a `DashMap<String, WindowState>` inside `rdw-core::RateLimiter`),
keyed by API key. Entries idle for more than 8 days are evicted on each request (via
`evict_stale`), so the map does not grow without bound.

## No persistence

Rate-limit state resets whenever the `rdw-api` process restarts — there is no Redis or database
backing it, by design (see the plan's settled decisions: no database, no Redis, local-first,
cheap to run). This is an accepted prototype-stage tradeoff. A production deployment that must
survive restarts without granting a fresh quota would need an external store (e.g. Redis) instead
of this in-memory counter.
