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
