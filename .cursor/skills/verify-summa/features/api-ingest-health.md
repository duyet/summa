# API ingest and health

Live telemetry Worker at **https://summa.duyet.net** (`apps/api`). Users hit `/health` (liveness) and `POST /v1/ingest` (authenticated fan-out to MotherDuck + the CH sink). Dashboard `/` is Clerk HTML, not required for this feature.

## Sub-features

- `GET /health` — public JSON `{ok, service: "summa", version}`
- `POST /v1/ingest` — API key (`Authorization: Bearer summa_…` or `X-Summa-Token`); stamps `account_id` / `api_key_id`; writes sinks
- Unauth ingest → **401** `{"error":"unauthorized"}`, never 500
- `GET /install.sh` — public installer text
- Auth-only: `/ping`, `/status`, `/v1/analytics`, `/v1/analytics/summary` (not the default drive)
- Worker unit tests: `cargo test --locked -p summa-api --lib`

## How to get to it (user POV)

Browser: https://summa.duyet.net (mint a key). CLI: `summa import` POSTs ingest when `telemetry_token` is set. curl:

```bash
curl -sS https://summa.duyet.net/health
curl -sS -X POST https://summa.duyet.net/v1/ingest \
  -H 'content-type: application/json' \
  -d '{"events":[]}'
```

## Driving it with HTTP

Existing tests first:

```bash
cargo test --locked -p summa-api --lib -- --test-threads=1
cargo test --locked -p summa-import --lib script::serve::tests::ingest_requires_token -- --test-threads=1
cargo test --locked -p summa-import --lib script::serve::tests::health_is_public -- --test-threads=1
```

Live hub (user path; empty body, no invented costs):

```bash
source .cursor/skills/verify-summa/helpers/isolated-env.sh

.cursor/skills/verify-summa/helpers/capture.sh api-health -- \
  curl -sS -D - https://summa.duyet.net/health

.cursor/skills/verify-summa/helpers/capture.sh api-ingest-unauth -- \
  curl -sS -D - -X POST https://summa.duyet.net/v1/ingest \
    -H 'content-type: application/json' \
    -d '{"events":[]}'
```

Pass:

- `api-health`: HTTP 200, body `"ok":true` and `"service":"summa"`
- `api-ingest-unauth`: HTTP **401**, body `"error":"unauthorized"` (or equivalent JSON error). Status 500 is a product regression — report it, do not paper over it.

Do **not** POST events with fabricated `cost` / token counts. Do **not** use a guessed `summa_` token. Authenticated ingest is out of scope for this skill unless the operator supplies a real key and accepts a write; even then send empty `events` or real imported rows, never invented dollars.

Do not start `wrangler dev` for default verification (needs `.dev.vars` secrets).

## Gotchas

- Worker `require_api_key` rejects tokens that do not start with `summa_`. A random Bearer value is still 401, not 500 (`auth_error_response` maps `"unauthorized"`).
- Ingest parses JSON only after auth. Unauth + invalid JSON must still be 401.
- `/health` is public CORS; ingest is not.
- Live ingest writes MotherDuck + the CH sink for **real** keys. Isolated CLI credentials stay empty so `summa import` cannot fan-out.
- Never commit Worker secrets or `apps/api/.dev.vars`.
