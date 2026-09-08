# Deployment

## Local (cargo)

```bash
export VALID_API_KEYS="test-key-123"
export RDW_APP_TOKEN="your-rdw-app-token"   # optional; service starts without it
cargo run -p rdw-api
```

```bash
curl -v "http://localhost:3000/api/v1/fuel?brands=toyota&limit=10&api_key=test-key-123"
```

## Docker (primary production path)

```bash
docker build -t rdw-api:latest .

docker run -p 3000:3000 \
  -e VALID_API_KEYS="test-key-123" \
  -e RDW_APP_TOKEN="your-rdw-app-token" \
  rdw-api:latest
```

Test the running container:

```bash
curl -v "http://localhost:3000/api/v1/fuel?brands=toyota&limit=10&api_key=test-key-123"
curl -v "http://localhost:3000/api/v1/fuel?brands=toyota&api_key=wrong-key"          # expect 401
curl -v -H "Accept: text/html" "http://localhost:3000/api/v1/fuel?brands=ford&api_key=test-key-123"  # expect 400 HTML
```

Local/Docker has no response-size or execution-duration limit, so it is the supported path for a
full, unrestricted export.

## Vercel

The project deploys to Vercel's **container** runtime: `vercel.json` declares a service whose
entrypoint is this repository's `Dockerfile`, and every request is rewritten to it. Vercel builds
that image, stores it in its own registry, and serves it from a function that scales to zero when
idle.

```bash
vercel deploy --prod
```

Pushing to `main` on the connected GitHub repository deploys as well.

Environment variables are set with the CLI (or the dashboard):

```bash
vercel env add VALID_API_KEYS production
vercel env add RDW_APP_TOKEN production      # optional
vercel env add FUEL_FAILURE_FLOOR production # optional, default 3
vercel env add FUEL_FAILURE_RATIO production # optional, default 0.10
```

### Deploying requires a running Docker daemon

The container image is built **on the machine running `vercel deploy`**, not on Vercel's builders.
With Docker stopped, the deploy produces no image and the deployment sits in `UNKNOWN` with a 0ms
build and no build logs — a confusing failure with no obvious cause. Start Docker Desktop first and
confirm with `docker info`.

`.vercelignore` must keep `target/` out of the upload. After a release build that directory reaches
several GB and will stall the deploy at the upload step; the image is built from source inside the
Dockerfile, so no local artifact needs uploading.

### Size limit on Vercel

`vercel.json` runs this service under Vercel's **container** runtime (see the Vercel section above),
not the Vercel Functions runtime, so the historical ~4.5MB non-streamed response-body cap does not
apply here. The response is streamed either way. Vercel's platform-level request timeout and
container restart/cold-start behavior still apply, though, and a full unrestricted export (gzip +
CSV brings a full Toyota export from ~2.6GB to roughly ~130MB, but it is still a large,
multi-minute request) is more reliably run against local or self-hosted Docker, which has neither a
request-size nor a duration limit.

### `curl --compressed` is required for large exports

A response above `UNCOMPRESSED_SIZE_THRESHOLD_MB` (default 50MB staged/compressed size) is refused
with `406 Not Acceptable` for any client that does not send `Accept-Encoding: gzip`, rather than
decompressing a potentially multi-hundred-MB body server-side for a client that could simply have
asked for gzip. Every mainstream browser sends this header automatically, so a plain
address-bar download is unaffected; a bare `curl` needs `--compressed`:

```bash
curl --compressed -v -o fuel-export.csv \
  "http://localhost:3000/api/v1/fuel?brands=toyota&api_key=test-key-123"
```

### Row order is non-deterministic for unlimited exports

An export with no `limit` is fetched via many concurrent, unordered kenteken ranges (see
`docs/ARCHITECTURE.md`) for wall-time, rather than the single sequential kenteken-ascending cursor a
`?limit=` request still uses. Row order in the output CSV/ZIP is therefore not guaranteed
kenteken-ascending for an unlimited export, and can differ between runs of the same request.

### Rate-limit quota requires session affinity across containers

The per-API-key/per-brand rate limiter (`FUEL_FAILURE_FLOOR`/`FUEL_FAILURE_RATIO` aside — this is the
daily/weekly request quota) and the single-export concurrency guard are both **per-process, in-memory
state**. Running more than one container behind a load balancer means each container tracks its own
independent quota and its own independent "an export is in progress" flag: a client whose requests
are spread across containers can exceed its intended quota, and two containers can each believe they
are the only export in flight. Fixed-IP or session affinity at the load balancer is required for the
quota and the single-export guard to behave as documented; this is a known, accepted limitation
(see `docs/plans/rdw-fuel-export-perf-cost.md`), not a bug fixed by this deployment guide.
