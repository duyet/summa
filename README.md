# summa

**summa** (*Latin*: sum, total, summary) — lightweight CLI that imports Claude
Code (**ccusage**), Codex, OpenCode, Cursor, Grok, Devin, pi, fx, and other agent
usage into local DuckDB, optionally syncing to ClickHouse or MotherDuck.

| | |
|---|---|
| **Binary** | `summa` |
| **Crate** | [`summa-import`](https://crates.io/crates/summa-import) |
| **Version** | 0.1.x |
| **License** | MIT |

> GitHub repo is `duyet/summa`. Crate is `summa-import`
> because bare `summa` is taken on crates.io (full-text search server).

### Name

| Name | Notes |
|------|--------|
| **summa** ✓ | Latin *summa* — sum / total / summary of usage. Product + binary. |
| summa-import | crates.io package (binary still `summa`) |
| usus | “Usage” — taken by another AI-usage CLI |
| sumptus | Expense/outlay — free, alternate we tried |

## Install

### curl | bash

```bash
curl -fsSL https://raw.githubusercontent.com/duyet/summa/master/install.sh | bash
curl -fsSL https://summa.duyet.net/install.sh | bash
```

Installs `~/.local/bin/summa` from the latest GitHub Release that has a tarball, or the rolling `beta` release CI publishes on every master push. Put env vars on **bash**, not curl:

```bash
curl -fsSL https://summa.duyet.net/install.sh | SUMMA_SETUP_CRON=1 bash
```

Env: `SUMMA_INSTALL_DIR`, `SUMMA_CHANNEL` (`beta` default, or `stable`), `SUMMA_VERSION` (`v0.1.1`), `SUMMA_DRY_RUN=1`. Then `summa update --beta|--stable` to switch channels, and `summa config --set update.mode=auto` for auto-updates. Full machine setup: [docs/install.md](docs/install.md).

### Cargo

```bash
cargo install summa-import --locked
```

### From source

Prefer CI binaries on laptops and home servers (`summa update` or the curl installer). `cargo build --release` is for CI only.

Cargo workspace: `apps/cli` (Rust binary `summa`) and `apps/api` (Rust Cloudflare Worker).

```bash
bun install
cp .env.example .env
bun run build:cli    # cargo build --locked --bin summa
bun run build:api    # cargo check -p summa-api --target wasm32-unknown-unknown
bun run deploy:api   # wrangler deploy (worker-build)
```

## Quick start (local-first)

```bash
summa import --verbose
# → ~/.local/share/summa/summa.duckdb
```

Optional cloud:

```bash
summa import --ch-host db.example.com --ch-port 8443
export MOTHERDUCK_TOKEN=…
summa import --duckdb-path=md:summa
```

## Config

1. `$SUMMA_CONFIG` (or legacy `$CCUSAGE_IMPORT_CONFIG`)
2. `./summa.toml` / `./summa-import.toml`
3. `~/.config/summa/config.toml` (XDG)
4. `~/.summa/config.toml`
5. `~/.summa-import.toml` (legacy)
6. `/etc/summa/config.toml`

Main config has no secrets. Credentials: `~/.config/summa/credentials.toml` or
`$SUMMA_CREDENTIALS` / `CH_PASSWORD` / `MOTHERDUCK_TOKEN`.

```toml
# ~/.config/summa/config.toml
[clickhouse]
host = "localhost"
port = 8123
user = "default"
database = "analytics"
protocol = "http"

[importer]
days_back = 7
```

```toml
# ~/.config/summa/credentials.toml
clickhouse_password = "…"
motherduck_token = "…"
# cursor_session = "WorkosCursorSessionToken=…"   # or CURSOR_SESSION / Cursor.app login
# cursor_api_key = "…"                            # team Admin API key (CURSOR_API_KEY)
```

## Usage

```bash
summa import --verbose
summa import --days-back=7
summa import --skip-clickhouse
summa import --skip-cursor
summa import --skip-grok
summa import --skip-pi
summa import --skip-fx
cargo run -- backfill-duckdb
```

## Cron job

`summa cronjob install` generates and registers a user scheduler:

- macOS: launchd LaunchAgent `net.duyet.summa.import`
- Linux: systemd --user timer `summa-import.timer` (falls back to crontab)
- crontab if neither is available (`crontab -` via stdin)

```bash
summa cronjob install                 # every 1h, --days-back from config (else 2)
summa cronjob install --every 6h      # ubuntu-style
summa cronjob install --every 1d      # daily 08:00
summa cronjob install --dry-run       # print unit/crontab, do not register
summa cronjob install --replace       # also drop legacy run-import.sh crontab
summa cronjob status
summa cronjob remove
```

At install time: `SUMMA_SETUP_CRON=1 SUMMA_CRON_EVERY=1h` with `install.sh`. Full guide: [`docs/install.md`](docs/install.md).

## Telemetry API

Hub: [https://summa.duyet.net](https://summa.duyet.net) (Cloudflare Worker, `apps/api`). Not a local HTTP server.

```toml
# ~/.config/summa/config.toml
[telemetry]
endpoint = "https://summa.duyet.net"
```

```toml
# ~/.config/summa/credentials.toml
telemetry_token = "summa_…"
```

`summa import` POSTs `/v1/ingest` when a token is set. D1 stores API keys/accounts only; usage data is ClickHouse + MotherDuck. `GET /v1/analytics` and `/v1/analytics/summary` for [burn.duyet.net](https://burn.duyet.net).

`summa serve` only pings the cloud hub (deprecated local bind). k8s/Hermes runs **import** as a client (`deploy/k8s/summa-sidecar.yaml`); sidebar iframe `https://summa.duyet.net`. See [`docs/telemetry.md`](docs/telemetry.md).

Logs: `~/.local/log/summa/cron.log`. Optional env file: `~/.config/summa/env` (systemd). `SUMMA_SETUP_CRON=1` registers the job at install time.

Keep Cursor and Grok enabled on every host. Account-wide Cursor uses `machine_name=account`; sinks dedup.

## Development / release

```bash
cargo test --locked -p summa-import
cargo test --locked -p summa-api --lib
bash scripts/ci/validate-deploy.sh
# cargo build --release is CI only — never on laptops
cargo package --locked -p summa-import
cargo publish --locked -p summa-import   # needs CARGO_REGISTRY_TOKEN / cargo login
```

- release-please + GitHub Release builds multi-arch `summa` binaries
- crates.io publish gated on `CARGO_REGISTRY_TOKEN`
- See `CHANGELOG.md`
