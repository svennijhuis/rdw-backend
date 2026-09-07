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

## Vercel (optional; setup and local test only, per this plan's scope)

1. Push the repository to GitHub.
2. Link the GitHub repository to a Vercel project.
3. Set `VALID_API_KEYS` and `RDW_APP_TOKEN` in the Vercel dashboard's environment variables.
4. Deploy with `vercel deploy --prod`.
5. Test only a small export, e.g. `?limit=1000`, to stay within Vercel's constraints.

**Known limitation:** Vercel Functions cap non-streamed response bodies at roughly 4.5MB and enforce
an execution-duration limit. A full, unrestricted `/api/v1/fuel` export exceeds both and will fail
on Vercel. Use local Docker for production-scale exports; Vercel is suitable only for small, capped
requests.
