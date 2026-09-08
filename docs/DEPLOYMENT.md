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

Vercel Functions cap a non-streamed response body at roughly 4.5MB. Measured against this service,
**about 5,000 rows produces 4.2MB**, so that is the practical ceiling there; larger exports fail.
Local or self-hosted Docker has no such cap and is the supported path for a full, unrestricted
export.
