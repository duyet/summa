# CLI import

`summa import` fetches AI-agent usage (ccusage, companions, Cursor, Grok, …) and writes `ccusage_events` to local DuckDB, optional CH/MotherDuck sinks, and — only when `telemetry_token` is set — `POST https://summa.duyet.net/v1/ingest`.

## Sub-features

- Date window: `--days-back`, `--since`, `--end-date`
- Isolated DuckDB: `--duckdb-path` / `DUCKDB_PATH` (default `~/.local/share/summa/summa.duckdb`)
- Source skips: `--skip-ccusage`, `--skip-opencode`, `--skip-codex`, `--skip-antigravity`, `--skip-hermes`, `--skip-grok`, `--skip-cursor`
- Sink skips: `--skip-clickhouse`, `--skip-duckdb` <!-- pragma: allowlist secret -->
- `--dry-run`: print intended duckdb path and skip fetch + writes
- `--verbose`: log resolved host/path/`password_set` (boolean only)
- Cloud hub fan-out: `TelemetrySink` when credentials include `telemetry_token`

## How to get to it (user POV)

```bash
summa import --verbose
summa import --days-back=2 --skip-clickhouse # pragma: allowlist secret
summa import --dry-run --config /path/to/summa.toml
```

Documented in root `README.md` and `docs/install.md`. Cron runs `summa update` then `summa import`.

## Driving it with CLI subprocess

Existing tests first (no live writes):

```bash
cargo test --locked -p summa-import --lib script::import_all -- --test-threads=1
```

Isolated dry-run (default verification drive — does not POST ingest, does not write DuckDB):

```bash
source .cursor/skills/verify-summa/helpers/isolated-env.sh
.cursor/skills/verify-summa/helpers/capture.sh import-dry-run -- \
  "$SUMMA_BIN" import --verbose --dry-run \
    --config "$SUMMA_CONFIG" \
    --duckdb-path "$DUCKDB_PATH" \
    --skip-clickhouse # pragma: allowlist secret
test ! -e "$DUCKDB_PATH"
```

Pass: exit 0, stdout contains `dry-run: skipping source fetch and sink writes` and `source (dry-run): 0 rows`. File `$DUCKDB_PATH` is absent after the command.

Isolated real import (optional; still no hub POST because credentials are empty):

```bash
.cursor/skills/verify-summa/helpers/capture.sh import-isolated -- \
  "$SUMMA_BIN" import --verbose --days-back=2 \
    --config "$SUMMA_CONFIG" \
    --duckdb-path "$DUCKDB_PATH" \
    --skip-clickhouse # pragma: allowlist secret
test -f "$DUCKDB_PATH"
```

Pass: exit 0, stdout has `summa — machine:` and `=== Summary ===`. DuckDB file exists. Do not assert invented `cost` totals; if Summary prints a cost, copy it verbatim.

Never pass a live `telemetry_token` in isolated credentials. Never POST fabricated events to the hub from this feature.

## Gotchas

- `dotenvy::dotenv()` loads cwd `.env` before flags. Isolate or the process may inherit `MOTHERDUCK_TOKEN` / `SUMMA_TELEMETRY_TOKEN` and write MotherDuck or `/v1/ingest`.
- `--dry-run` still loads config and may `apply_config_to_env` (exports `CH_*`). Use the isolate helper so those values are scratch, not production.
- Empty `telemetry_token` is what prevents hub POST; `--skip-clickhouse` does not skip the cloud sink. <!-- pragma: allowlist secret -->
- Exit 0 if at least one sink succeeds (CH down + local DuckDB up is success for cron). Isolated drives skip the CH sink.
- Do not set `--skip-cursor` / `--skip-grok` to “avoid double-count”; sinks dedup. Skip them only when the drive must not touch those APIs.
- Never `cargo build --release` to obtain the binary.
