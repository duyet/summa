# CLI check

`summa check` reads the resolved DuckDB (local file or `md:…`) and prints a date range, record/token totals, cost, and per-model / per-source breakdowns. `--json` emits the same fields as JSON.

## Sub-features

- Human summary (`summa check`) including `cost: $…` from `sum(cost)` in `ccusage_events`
- `--json` object: `duckdb`, `date_range`, `records`, `tokens`, `cost_usd`, `models`, `sources`
- `--config` selects which importer DuckDB path to open
- MotherDuck path (`md:…`) uses the DuckDB sink query connection (needs token; out of scope for isolated drives)

## How to get to it (user POV)

```bash
summa check
summa check --json
summa config --validate   # related: config presence, not DuckDB contents
```

Documented as a smoke step in `docs/install.md`.

## Driving it with CLI subprocess

Existing tests first: there is no dedicated `script::check` integration test; cover parser/sink tests via `cargo test --locked -p summa-import -- --test-threads=1` when claiming a check regression in shared types. Then drive the binary.

Isolated: import once into `$DUCKDB_PATH` (see [cli-import.md](cli-import.md)) so `ccusage_events` exists, then:

```bash
source .cursor/skills/verify-summa/helpers/isolated-env.sh
.cursor/skills/verify-summa/helpers/capture.sh check-json -- \
  "$SUMMA_BIN" check --json --config "$SUMMA_CONFIG"
```

`check` does not take `--duckdb-path`; it uses config / `DUCKDB_PATH` / default. The isolate helper exports `DUCKDB_PATH`.

Pass: exit 0, JSON parses, `duckdb` equals `$DUCKDB_PATH`, `records` is a number. Copy `cost_usd` from the JSON; do not invent it. An empty table after a skipped-source import may show `0` — that is observed state.

If DuckDB is missing the table, the command fails (query error). That is a real failure, not a doctor skip: create the DB with an isolated import first, or report missing table.

## Gotchas

- Opening a missing local path **creates** an empty DuckDB file, then the `FROM ccusage_events` query fails. Doctor must not run `check` against a throwaway default path (it would leave a file under the real data dir if not isolated).
- `--json` `cost_usd` is the database sum. Treat it as evidence from the file, never as a number the agent computed.
- `summa config --validate` is not a substitute for `check`; it only prints `config ok` and the default duckdb path.
- Do not pass `md:` paths in isolated drives (would need MotherDuck).
