# Install summa on a machine

Binary `summa`. Never `cargo build --release` on a laptop or home Linux host — CI builds; you only install the artifact.

## 1. Binary

```bash
curl -fsSL https://raw.githubusercontent.com/duyet/summa/master/install.sh | bash
curl -fsSL https://summa.duyet.net/install.sh | bash
```

Installs `~/.local/bin/summa` from the `SUMMA_CHANNEL` you pick — default `beta`, the rolling tag `release.yml` publishes on every master push; `stable` uses release-please tagged GitHub Releases. Env vars must be on **bash** (the right-hand side of the pipe):

```bash
curl -fsSL https://summa.duyet.net/install.sh | SUMMA_SETUP_CRON=1 SUMMA_CRON_EVERY=1h bash
```

Env: `SUMMA_INSTALL_DIR`, `SUMMA_CHANNEL` (`beta` or `stable`), `SUMMA_VERSION` (a tag like `v0.1.1` overrides the channel), `SUMMA_DRY_RUN=1`, `SUMMA_TELEMETRY_TOKEN`. Switch channels and update with `summa update --beta|--stable`; enable auto-update with `summa config --set update.mode=auto`.

CI publishes a GNU `shasum -a 256` sidecar next to every tarball (`summa-<target>.tar.gz.sha256`, first field is the digest; the filename is `dist/…`). `install.sh` and `summa update` download that sidecar, hash the archive, and **abort** if the sidecar is missing, malformed, or the digest does not match — then extract and replace the binary.

## 2. Config

`~/.config/summa/config.toml` has no secrets. Tokens go in `credentials.toml`.

```toml
# ~/.config/summa/config.toml
[clickhouse]
host = "localhost"
port = 8123
user = "default"
database = "analytics"
protocol = "http"

[importer]
# duckdb_path = "md:ccusage"   # MotherDuck
days_back = 7
# skip_cursor / skip_grok stay off — every machine imports; sinks dedup

[update]
channel = "beta"   # or "stable"
mode = "manual"    # "auto" downloads updates for the next launch
```

Set with `summa config --set update.channel=stable` / `summa config --set update.mode=auto`.

```toml
# ~/.config/summa/credentials.toml
clickhouse_password = "…"
motherduck_token = "…"
# cursor_session = "WorkosCursorSessionToken=…"
# cursor_api_key = "…"
# telemetry_token = "…"
```

Keep **Cursor** and **Grok** enabled on every host. Account-wide Cursor rows use `machine_name=account`. DuckDB delete-by-key and ClickHouse ReplacingMergeTree collapse duplicates. Do not set `skip_cursor` to “avoid double-count”.

## 3. Cron

```bash
summa cronjob install                 # 1h, --days-back from config (else 2)
summa cronjob install --every 6h
summa cronjob install --every 1d      # 08:00
summa cronjob install --dry-run
summa cronjob install --replace       # drop legacy run-import.sh crontab
summa cronjob status
summa cronjob remove
```

Backends: macOS launchd, Linux systemd --user, crontab, or a sleep-loop when neither exists.

## 4. Smoke

```bash
summa config --validate
summa check --json
summa import --verbose --days-back=2
```

## 5. Optional telemetry hub

Hosted hub: [https://summa.duyet.net](https://summa.duyet.net). Put `telemetry_token` in `credentials.toml` and optional `[telemetry] endpoint`. `summa import` POSTs `/v1/ingest`. D1 is keys/accounts only; usage is ClickHouse + MotherDuck. See `docs/telemetry.md` and `apps/api/README.md`. Copy `.env.example` → `.env`.

Cron job: `summa update` then `summa import`.

Kubernetes: Hermes runs **import** (client), not a local telemetry server. Sidebar iframe `https://summa.duyet.net`. Hourly CronJob. Secret: `SUMMA_TELEMETRY_ENDPOINT=https://summa.duyet.net` and `SUMMA_TELEMETRY_TOKEN`. Manifest: `deploy/k8s/summa-sidecar.yaml`.
