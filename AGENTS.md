# AGENTS.md

Public product: **summa** (crate `summa-import`, binary `summa`).
Data pipeline importing AI coding-agent usage into local DuckDB and optional ClickHouse/MotherDuck.

## Status

Rust is the primary implementation (0.1.x public line). Single `ccusage_events` table.

Docs index: `docs/INDEX.md` (core memory: `docs/knowledge/core-memory.md`).

## Commands

```bash
bun run check                           # CLI cargo check + API wasm check
bun run build:cli                       # cargo build --locked --bin summa (not --release)
bun run build:api                       # Worker typecheck
bun run test                            # CLI + API cargo test (CLI env tests hold EnvLock)
bun run test:deploy                     # install.sh / k8s / wrangler / release-please
bun run deploy:api                      # wrangler deploy (apps/api)
cargo test --locked -p summa-import     # CLI tests
cargo test --locked -p summa-api --lib  # Worker unit tests
cargo check --locked -p summa-import    # typecheck CLI
cargo package --locked -p summa-import  # crates.io pack (CI also runs this)
git switch -c automation/<topic> origin/master  # create branch first when worktree is on detached HEAD
git worktree list --porcelain  # find owning worktree when .git/worktrees/.../*.lock errors appear
git log --since='<last-run-iso>' --pretty=format:'%H %cI %s' --name-only  # recent-change audit window
rg -n "<symbol>" apps/cli/src apps/cli/tests -g '!**/*.test.ts' -g '!**/*.spec.ts'  # dead-code evidence (non-test refs)
cargo run -- import --verbose           # full import (local DuckDB by default)
cargo run -- backfill-duckdb            # backfill DuckDB from ClickHouse
# Do not cargo build --release here (low memory). CI builds; install:
#   curl -fsSL https://summa.duyet.net/install.sh | bash
#   summa update   # newest CI Release-workflow artifact for this OS/arch
git log --since='7 days ago' --no-merges --name-only --pretty=format:'--- %h %ad %s' --date=short
```

Config: `~/.config/summa/config.toml` + `credentials.toml` (secrets separate). `.env` still overlays `CH_*` / `MOTHERDUCK_TOKEN` / `DUCKDB_PATH`.
Default DuckDB: `~/.local/share/summa/summa.duckdb` (auto-created).
Install: `curl -fsSL https://summa.duyet.net/install.sh | bash` (or GitHub raw `install.sh`). Env vars go on bash: `curl … | SUMMA_SETUP_CRON=1 bash`. Never `cargo build --release` on laptops or home Linux hosts (CI only).
Scheduler: `summa cronjob install` (launchd / systemd user timer / crontab / loop). Installer: `SUMMA_SETUP_CRON=1` runs that after curl|bash.
Telemetry hub: Cloudflare Worker at **https://summa.duyet.net** (`apps/api`). Clients POST with `telemetry_token`; worker stamps `account_id`/`api_key_id` and double-writes MotherDuck + ClickHouse. Clerk login generates keys. Analytics: `GET /v1/analytics` + `/summary` for burn.duyet.net. Cron job runs `summa update` then `summa import`. k3s: `deploy/k8s/summa-sidecar.yaml` is an **import client** (CronJob), not a local serve. Creds: `.env.example`. Docs: `docs/install.md`, `docs/telemetry.md`.

Keep **Cursor** and **local Grok Build** on every machine. Account-wide Cursor rows use `machine_name=account`; sinks (`ccusage_events` ReplacingMergeTree / DuckDB dedup) must collapse duplicates — do not disable those sources to “avoid double-count”.

## Architecture

Plugin: sources → pipeline runner → sinks. Single table `ccusage_events`.

- CLI crate: `apps/cli` (`summa-import`, binary `summa`)
- API Worker: `apps/api` (`summa-api`, `workers-rs` wasm)
- Sources: `apps/cli/src/source/{ccusage,companion,antigravity,hermes,grok,grok_api,cursor}.rs`
- Sinks: `apps/cli/src/sink/{clickhouse,duckdb,csv}.rs`
- Types: `apps/cli/src/model.rs` — `EventRow`

## Key conventions

- Model breakdowns exploded into rows (one per model per record)
- Codex `inputTokens` includes cached — total = input + output (no cache double-count)
- Claude `cacheReadTokens` is separate — total = input + output + cacheCreate + cacheRead
- Cost distributed across models when per-model costs missing (`distributeCost()`)
- Companion packages may print log lines before JSON — parser skips to first `{`/`[`
- Grok Build: `~/.grok` / `GROK_HOME` — `logs/unified.jsonl` (`shell.turn.inference_done`) + session `summary.json` for model/cwd; tokens: input=`prompt-cached`, cache_read=`cached`, output=`completion`, total=`prompt+completion` (reasoning not double-counted); `--skip-grok`. Optional account-wide CLI-proxy billing (`grok-api`) is imported only when the JSON has countable spend/tokens — credits-percent payloads are skipped (no fabricated turns).
- Cursor (account-wide, `machine_name=account`): dashboard `POST https://cursor.com/api/dashboard/get-filtered-usage-events` (session/cookie or Cursor.app `state.vscdb` JWT) or Admin `POST https://api.cursor.com/teams/filtered-usage-events`; surfaces `cursor` / `cursor-cloud-agent` / `cursor-api` / `cursor-grok-bot`; `--skip-cursor`. Missing auth skips the source.
- Monthly not fetched — derivable via `toYYYYMM(date)` SQL

## Code style

No comments unless WHY is non-obvious. Surgical changes only. No AI slop.

## Core memory

See `docs/knowledge/core-memory.md` for the compact maintenance runbook.
