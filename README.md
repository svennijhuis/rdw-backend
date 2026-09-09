# rdw-backend

Exports Dutch vehicle registration data as a CSV download, joining the RDW open-data vehicle
register with its fuel dataset for Lexus, Toyota and Suzuki.

One endpoint, no database, no cache. Written in Rust with axum.

## The endpoint

```
GET /api/v1/fuel?brands=<brands>&limit=<n>&api_key=<key>
```

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `brands` | no | all three | Comma-separated, case-insensitive. Only `lexus`, `toyota`, `suzuki`. |
| `limit` | no | everything | Maximum rows to return. `0` is valid and returns only the header row. |
| `api_key` | yes | — | Also accepted as an `X-Api-Key` header, which is preferred. |

```bash
# All three brands, first 100 rows
curl -o fuel.csv "https://<host>/api/v1/fuel?limit=100&api_key=$KEY"

# One brand, everything
curl -o toyota.csv "https://<host>/api/v1/fuel?brands=toyota&api_key=$KEY"

# Key in a header instead of the URL
curl -o fuel.csv -H "X-Api-Key: $KEY" "https://<host>/api/v1/fuel?limit=100"
```

### Responses

| Status | When |
|---|---|
| 200 | CSV download, or a ZIP of numbered CSV parts above Excel's row limit |
| 400 | Unknown brand, or an invalid `limit` |
| 401 | Missing or invalid API key |
| 429 | Rate limit reached, or another export is already running |
| 502 | RDW failed after retries |
| 504 | RDW timed out |

Error bodies render as a small HTML page when the client prefers HTML, and as plain text otherwise.

## The CSV

One row per vehicle: a curated set of vehicle columns, then one `Brandstof` column, then an
export-status column. The full RDW vehicle dataset has ~98 fields; the export keeps the ones that
read well in a spreadsheet (identity, registration, size, type approval) and drops API link
columns, duplicate `_dt` timestamps, trailer/hitch fields, and min/max dimension ranges.

Vehicle headers use RDW's own display names rather than its internal field keys, so a column reads
`Massa ledig voertuig`, not `massa_ledig_voertuig`. The Dutch names come from RDW and are not
translated here.

A vehicle can hold up to three fuel entries — a hybrid has two, for example petrol and electric.
Those types are joined in `Brandstof` with a comma (`Benzine, Elektriciteit`) rather than repeating
the vehicle across several rows or widening every RDW fuel field into extra columns. A fourth entry
has never appeared in the data; if one ever does, the export fails loudly instead of silently
dropping it.

The last column says how complete each row is:

| `Export status` | Meaning |
|---|---|
| `ok` | Fuel data was fetched for this vehicle |
| `no_fuel_data` | The fetch succeeded, but RDW genuinely holds no fuel rows for this plate |
| `fuel_unavailable` | The fetch failed, so `Brandstof` is blank for a reason unrelated to the vehicle |

That distinction matters: without it, a vehicle with no fuel data and a vehicle whose data failed to
load both look like empty cells.

### Partial exports

Losing fuel data for part of an export does not throw the whole export away. Up to
`max(FUEL_FAILURE_FLOOR, FUEL_FAILURE_RATIO × attempts)` fuel fetches may fail; those rows are marked
`fuel_unavailable`, the filename gains a `PARTIAL` marker, and an `X-Export-Warnings` header reports
the counts. Past that threshold the export fails with 502 and sends no bytes at all.

Failing to fetch *vehicles* is different and always aborts, because those rows would simply be
missing with nothing to mark.

### Spreadsheet row limit

Excel stops at 1,048,576 rows. An export larger than that is returned as a ZIP of numbered CSV parts,
each under the limit and each with its own header row, plus an `_EXPORT_REPORT.txt` describing any
failures.

## Rate limits

Three requests per day and five per week, counted **per API key and per brand**. Spending Toyota's
daily allowance leaves Lexus and Suzuki untouched, so the three brands each get their own budget.

A request naming several brands draws one from each of their budgets. If any named brand is already
exhausted the whole request is refused with 429, and the brands checked before it get their
reservation back — a refused request never costs quota.

A request without a valid key is rejected with 401 before the limiter is consulted, so a bad key
cannot burn someone else's quota. One export runs at a time; a second concurrent request gets 429.

Counters live in memory, so they reset when the process restarts — on a serverless host, that means
every cold start. A request that ends in 502 or 504 does not consume quota.

## Running it

```bash
export VALID_API_KEYS="some-key"
cargo run -p rdw-api          # listens on $PORT, else $SERVER_PORT, else 3000
```

```bash
docker build -t rdw-api . && docker run -p 3000:3000 -e VALID_API_KEYS="some-key" rdw-api
```

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `VALID_API_KEYS` | yes | — | Comma-separated accepted keys. Empty means every request is rejected. |
| `RDW_APP_TOKEN` | no | — | Socrata app token. Without it RDW's unauthenticated rate tier applies. |
| `FUEL_FAILURE_FLOOR` | no | 3 | Fuel fetch failures tolerated regardless of export size. |
| `FUEL_FAILURE_RATIO` | no | 0.10 | Additional proportional allowance on large exports. |
| `FUEL_CONCURRENCY` | no | 16 | Fuel batches fetched at once. Past ~16 RDW throttling dominates. |
| `PORT` / `SERVER_PORT` | no | 3000 | Listen port. `PORT` wins; hosts usually inject it. |
| `RUST_LOG` | no | — | Log filter, e.g. `info` or `rdw_api=debug`. |

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for Vercel, including the two traps that make a deploy
fail silently.

## Development

```bash
cargo test --workspace          # unit and integration tests, all RDW calls mocked
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Three crates: `rdw-client` talks to RDW, `rdw-core` holds the join, widening, CSV assembly and rate
limiting, and `rdw-api` is the axum service. Dependency versions are centralised in the workspace
root.

Tests mock every RDW call, which keeps them fast and offline but means they cannot catch a malformed
query — a mock answers anything. Two bugs got through exactly that way. **Run a real request against
RDW before trusting a change to the query layer.**

## Whole-brand exports

Asking for a brand without a `limit` works and is the intended use. Measured end to end:

| Export | Vehicles | On disk | Over the wire (gzip) | Time |
|---|---|---|---|---|
| Lexus | 31,512 | 29 MB | 2.6 MB | 8s |
| Toyota | 824,620 | 736 MB | 63 MB | 237s |

Two things make that possible. Responses are **streamed from disk**, so neither the service nor the
host has to hold the file in memory, and a streamed body is not subject to the 4.5MB cap that hosts
such as Vercel apply to buffered responses. Responses are also **gzipped**, and this CSV compresses
about 13x, which is most of the transfer time and most of the bandwidth bill.

Fuel batches are fetched concurrently, `FUEL_CONCURRENCY` at a time, in issue order so the rows stay
kenteken-sorted for the join. Measured on a full Lexus export: 1 -> 30.4s, 8 -> 12.9s, 16 -> 7.7s,
24 -> 7.7s, 32 -> 14.9s. Sixteen is the knee; past it Socrata throttling costs more than the extra
parallelism gains.

Fuel rows are fetched by naming the plates, not by kenteken range. A brand's plates are scattered
across the whole kenteken space — every Lexus lies between `00GDF5` and `ZV939H`, a range holding
16,962,192 of the dataset's 16,966,705 fuel rows — so a range query walked almost the entire dataset
to find a few thousand cars. Naming plates in batches fetches only what the export needs.

## Known limits
- **Only three brands.** Anything else is rejected with 400, by design.
- **`api_key` in the URL is visible** in logs, proxies and browser history. Prefer `X-Api-Key`.
- **RDW's data is the source of truth.** Blank cells usually mean RDW holds no value; Socrata omits
  null fields entirely.
