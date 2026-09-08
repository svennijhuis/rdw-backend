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
