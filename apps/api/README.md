# @summa/api — Rust Cloudflare Worker

Telemetry hub at [https://summa.duyet.net](https://summa.duyet.net). Written in Rust (`workers-rs`).

D1 holds **API keys and accounts**, plus a **daily-row analytics mirror** (`ccusage_events`, migration `0004_d1_analytics.sql`): ingest upserts `record_type = 'daily'` rows there, and `/v1/analytics` reads in the chain MotherDuck → D1 → ClickHouse, so burn.duyet.net survives a MotherDuck or homelab-tunnel outage. Usage rows go to ClickHouse and MotherDuck; session/block rows stay warehouse-only. Bulk mirror (re)fill: `summa d1-sync [--full]` (POSTs daily rows through the normal authenticated `/v1/ingest`; ongoing coverage is automatic on every `summa import`).

```bash
rustup target add wasm32-unknown-unknown
cargo check -p summa-api --target wasm32-unknown-unknown
# worker crate 0.8.x + worker-build 0.8.5 (pinned in wrangler.jsonc).
bun run deploy:api
```

`wrangler.jsonc` runs `worker-build --release` on deploy. Secrets via `bash scripts/sync-worker-secrets.sh` (CH through `clickhouse-homelab.duyet.net` + Access token; MotherDuck MCP).
