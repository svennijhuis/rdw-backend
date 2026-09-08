## /squad — RDW fuel CSV API (round 1-2, 2026-09-07)

- Entrypoint: /squad. Stack: rust. Pack: rust@agentpacks, ready, no install needed.
- Verdict: pass at round 2. 12/12 acceptance criteria verified, 67 tests.
- Round 1 produced 4 high + 1 medium; one fix round cleared all of them.
- Skips that PASSED and can be preferred again: simplifier found nothing on a
  greenfield three-crate workspace, so a first-implementation simplification pass
  is low yield when the layout came straight from a confirmed plan.
- Skip that FAILED and is now a must-run: the planner's own recommendations were
  wrong twice against verifiable facts (a 50,000 $limit cap that does not exist on
  this endpoint, and "$offset paging is fine"). Verify planner-asserted API limits
  with a live call before planning on them.
- Advisor-lite at plan-confirm earned its cost: it caught the $offset instability,
  the missing merge-join strategy, and governor being a leaky-bucket limiter rather
  than a daily quota. All three would have shipped as silent data corruption or a
  wrong rate limit. Keep the plan-confirm consult.
- Reviewers disagreed with the user's stated wishes twice (HTML output, path-vs-query
  params, single crate vs workspace, polars). Surfacing the disagreement to the user
  rather than silently following the agent was correct each time.

## /squad — RDW fuel partial-failure + live pagination bug (2026-09-08)

- Entrypoint: /squad. Stack: rust. Pack ready, no install.
- MUST-RUN, learned the hard way: run the built binary against the REAL upstream
  before calling a change done. Every test here is wiremock-mocked, and wiremock
  answers any query, so it cannot reject a malformed one. Two separate bugs were
  invisible to a green 106-test suite and appeared on the first live request:
  (1) the shipped fuel fetch asked for one 50,000-row page while vehicles page
  50,000 at a time, silently dropping ~89% of fuel rows (472,130 in one page's
  kenteken range); (2) the fix for it compared brandstof_volgnummer numerically,
  but RDW stores that column as TEXT, so Socrata rejected every paginated query
  with query.soql.type-mismatch.
- Corollary: a mocked suite proves logic, never protocol. Any change that alters
  a query sent to a third party needs one real call.
- The Advisor-lite consult again earned its cost: it found the fuel-truncation
  bug that four agents (implementer, verifier, two reviewers) had passed over.
  Keep the plan-confirm consult.
- Reviewing the implementer's own work paid off twice: its threshold used
  `failures > floor OR ratio` where the plan said `> max(floor, ratio*attempted)`,
  and its own test asserted 1-failure-of-5 aborts the export — precisely the
  all-or-nothing behaviour the change existed to remove. Check that a returned
  test suite encodes the PLAN's rule, not the implementer's reinterpretation.
- Deploy trap worth remembering: Vercel CLI with no .vercelignore uploads the
  whole directory. After a Rust release build target/ is ~3.2 GB and deploys
  stall in UNKNOWN with no build logs and a 0ms build. Add .vercelignore before
  the first deploy, not after.
- A verifier subagent was lost mid-run to a session rate limit; re-running the
  commands directly in the main agent was cheaper and sufficient.

## /squad — RDW fuel export perf + Vercel cost (2026-09-08)

- Entrypoint: /squad. Stack: rust. Pack ready, no install.
- Result: full TOYOTA export 239.4s -> 40.6s (5.9x) and ~2.6 GB upstream ingress
  -> ~90 MB, on real calls against opendata.rdw.nl. Levers: reqwest `gzip`
  feature, Socrata `.csv` instead of `.json`, and concurrent unordered kenteken
  ranges replacing the single sequential cursor.
- MEASURE BEFORE BELIEVING A LEVER. gzip was assumed to be the speed fix and is
  not: a 5,000-row page was 1.37s uncompressed and 1.40s gzipped, and a 50,000-row
  page took 6.06s either way. Per-page latency is Socrata's query time, not
  transfer. gzip is a pure COST lever. The wall-time win came entirely from
  parallelism. Stating this plainly changed the user's scope decision — they
  reopened parallel ranges after initially declining them.
- The cheapest and most convincing verification in this whole run: build the
  pre-change commit in a throwaway `git worktree`, run BOTH binaries against the
  real upstream, and byte-diff a small export. `?limit=2000&brands=lexus` came
  back byte-for-byte identical, which closed the entire JSON-vs-CSV row-shape
  risk class (empty-string vs absent key, type inference, column ordering) in one
  command. Do this on any change that alters an upstream wire format.
- Advisor-lite at plan-confirm earned its cost for the third run running. It found
  that `merge_join` never actually required global kenteken ordering (it is called
  per page and carries no cross-page state), which turned the expensive "ordered
  parallel ranges with a reorder buffer" design into a much simpler unordered one.
  Keep the plan-confirm consult.
- Verify planner-asserted Socrata capabilities with curl, again. This time the
  planner's claims held: `substring(kenteken,1,2)` with `$group` works, the `.csv`
  endpoint accepts the existing `$where`/`$order`, there is no BOM, and there are
  no url-typed columns. Cheap to check, and two prior runs were burned by not
  checking.
- Range tiling that cannot gap or overlap BY CONSTRUCTION beats a tiling that is
  merely tested: `fixed_two_char_bands` shares each boundary string between
  adjacent ranges, leaves the first unbounded below and the last unbounded above,
  and lets Socrata evaluate `kenteken > lo AND kenteken <= hi`. The alphabet then
  only affects load balance, never coverage — so a wrong or stale distribution is
  a performance bug, never a data bug.
- The row-count reconciliation (one `$select=count(kenteken)` up front, compared
  with rows emitted) was initially skipped by the implementer and is the single
  highest-value guard in the change. It is the ONLY detector for a tiling gap, and
  it also turned out to be the net that catches a silently swallowed worker panic
  (see below). Implement it with a TOLERANCE, not strict equality: the export takes
  ~40s and RDW mutates continuously, so strict equality produces flaky 502s. Used
  max(50 rows, 0.05%).
- Bug found by reading, not by tests: `join_set.join_next().await;` discards its
  result, so a panicking range worker is swallowed entirely — no error, no abort.
  The export just emits fewer rows. Only the V.1 count check turns that into a 502,
  and the message says "row count mismatch" rather than "worker panicked".
- `FUEL_RANGE_PAGE_SIZE` is not clamped the way `worker_count` is (`.max(1)`), so 0
  is accepted. It fails loudly rather than looping forever only because
  `fetch_one_range` breaks on an empty page. Clamp env-derived sizes at the parse
  site, not incidentally downstream.
- Cost trap worth remembering: a client that omits `Accept-Encoding: gzip` made the
  server gunzip and stream the full 736 MB CSV. Plain `curl` does that; browsers
  never do, since they always send the header and decompress transparently. The
  expensive case was our own test client.
- Session rate limits killed the correctness and security reviewers mid-run, twice.
  As in the previous run, doing the high-risk checks directly in the main agent was
  cheaper and worked. Budget for reviewers dying and have a fallback.

### Security pass on the CSV parser (same run, later)

- The `csv` crate treats end-of-input INSIDE A QUOTED FIELD as a valid end of
  that field. A body cut mid-quoted-cell therefore parses perfectly cleanly into
  a short page, and both pipelines read a short page as "this range is
  exhausted". Criterion B.4 was written to prevent exactly this and the
  implementation did not actually achieve it: the guard was assumed to come from
  gzip's CRC, which only helps when the response is gzipped. Fixed with a framing
  check — every Socrata CSV body ends with a newline (verified live on the
  vehicle dataset, the fuel dataset, and a header-only zero-row response), so a
  body that does not is incomplete whatever the parser makes of it.
  GENERAL LESSON: a lenient parser is not a truncation detector. Check framing
  explicitly against a property the real endpoint actually guarantees.
- `headers.iter().enumerate().map(|(i,h)| (h,i)).collect::<HashMap<_,_>>()` keeps
  the LAST duplicate. A header of `kenteken,merk,kenteken` passed the
  required-column check and then read the join key from the wrong column. Now
  rejected. Collecting into a map silently resolves duplicates — never do it for
  a key that a join depends on.
- Enabling gzip added a decompression-bomb surface that did not exist before:
  `resp.bytes()` buffers the DECOMPRESSED body, whose size nothing in the
  response declares, so a Content-Length check would not have helped. Now read in
  chunks against a 256 MB budget. Worth remembering that turning on transparent
  decompression is a security change, not only a performance one.
- The upstream error body was echoed to the API caller unbounded, inside the 502
  message. Now truncated to 512 bytes on a char boundary.
- `neutralize_formula` was checked and is still applied to every cell on the new
  path; the parser deliberately passes payloads through untouched and a test now
  pins that contract so neither layer starts assuming the other sanitizes.
