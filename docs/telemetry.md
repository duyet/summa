# Telemetry (summa.duyet.net)

The hub is a **Cloudflare Worker**, not a local `summa serve`. Every `summa` binary POSTs events with an API key. The worker stamps `account_id` / `api_key_id` and double-writes **MotherDuck** and **ClickHouse**. burn.duyet.net, MCP, and the dashboard pull `/v1/analytics`.

Existing rows keep empty `account_id`. Analytics for the **first/owner** tenant includes `account_id IN (theirs, '')` so current data still shows; other tenants only see their own `account_id`. `GET /v1/analytics` uses ClickHouse when reachable and falls back to MotherDuck (MCP SQL at `https://api.motherduck.com/mcp`). Worker `CH_HOST` is the tunnel hostname `clickhouse-homelab.duyet.net` (HTTPS) plus Cloudflare Access service-token headers (`CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`). Sync local keys with `bash scripts/sync-worker-secrets.sh`.

Worker deploy, D1 (keys/accounts + daily-row analytics mirror), and secrets: `apps/api/README.md`. Usage data: ClickHouse + MotherDuck; analytics reads MotherDuck → D1 → ClickHouse; mirror (re)fill via `summa d1-sync`. Creds: `.env.example` at repo root.

`summa serve` only pings this hub (deprecated local HTTP). k3s/Hermes: `docs/k3s.md` — run **import**, not a local server. Sidebar iframe: https://summa.duyet.net.

## Client config

```toml
# ~/.config/summa/config.toml
[telemetry]
endpoint = "https://summa.duyet.net"
```

```toml
# ~/.config/summa/credentials.toml
telemetry_token = "summa_…"
```

Env: `SUMMA_TELEMETRY_ENDPOINT`, `SUMMA_TELEMETRY_TOKEN`.

`summa import` still writes local DuckDB. When a token is set it also POSTs `/v1/ingest`. The hourly/6h job runs `summa update` then `summa import` (auto-upgrade from GitHub CI).

## Hub API

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/` | Clerk or bootstrap | Create/revoke API keys |
| GET | `/health` | no | Liveness |
| GET | `/ping` | API key | Sink latency (does not hit sinks without a key) |
| POST | `/v1/ingest` | API key | Fan-out write |
| GET | `/v1/analytics` | API key | Daily points (`group=source\|model`) |
| GET | `/v1/analytics/summary` | API key | Totals + calendar `cost_per_day` |
| POST | `/v1/keys` | Clerk session or bootstrap | Mint `summa_…` key (shown once) |
| GET | `/v1/keys` | Clerk / bootstrap | List prefixes |
| DELETE | `/v1/keys/:id` | Clerk / bootstrap | Revoke |

## Keys

Sign in at https://summa.duyet.net (Clerk). Generate a key, put it on each machine. Multi-tenant: each Clerk user is an `account_id`; events never leak across accounts.

## Install a new machine

```bash
curl -fsSL https://summa.duyet.net/install.sh | \
  SUMMA_SETUP_CRON=1 SUMMA_CRON_EVERY=1h \
  SUMMA_TELEMETRY_ENDPOINT=https://summa.duyet.net \
  SUMMA_TELEMETRY_TOKEN=summa_… \
  bash
summa cronjob install --every 1h --replace
```

Never `cargo build --release` on laptops or home Linux.
