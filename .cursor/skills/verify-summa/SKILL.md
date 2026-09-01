---
name: verify-summa
description: >-
  Verify summa (CLI binary `summa` / crate `summa-import`, plus telemetry API
  Worker at https://summa.duyet.net): install, update checksum skip, import,
  check, serve, GET /health, and POST /v1/ingest auth. Use mid-task when proving
  CLI or hub behavior, after importer/auth/install changes, or before claiming a
  fix. Isolated PTY/HTTP only; never invent costs or POST fake billed sessions.
---

# Verify summa

Project-local harness for **summa**. Later agents run this cold. Do not interview the user.

**Primary surface:** CLI `summa` (`apps/cli`, crate `summa-import`). Drive with isolated subprocess / PTY and transcripts.

**Secondary surface:** Telemetry hub `https://summa.duyet.net` (`apps/api`, Cloudflare Worker). Drive with HTTP. Unauthenticated `POST /v1/ingest` must be **401**, never 500. Do not POST fake sessions with invented dollars.

There is no local product HTTP server. `summa serve` pings the cloud hub and exits. Do not start `wrangler dev` unless a mapped feature explicitly needs a local Worker (it needs secrets; default verification does not).

Never commit `.env`, `credentials.toml`, MotherDuck tokens, CH passwords, Clerk keys, or `telemetry_token`. Never use git worktrees as a product workflow.

Feature map: [features/README.md](features/README.md).

## Launch

No long-lived server. Launch means **build the debug CLI once**, then start each drive in its own isolated env.

From the repo root (never `cargo build --release` here; CI only):

```bash
cargo build --locked --bin summa
SUMMA_BIN="${PWD}/target/debug/summa"
test -x "$SUMMA_BIN"
```

Ready when `"$SUMMA_BIN" --version` prints `summa <semver>` (crate version is `apps/cli/Cargo.toml`, currently `0.1.2`) and `"$SUMMA_BIN" --help` lists `import`, `check`, `serve`, `update`.

Repo commands (prefer these over inventing new ones):

| Surface | Command |
| --- | --- |
| CLI typecheck | `cargo check --locked -p summa-import` |
| CLI tests | `cargo test --locked -p summa-import -- --test-threads=1` |
| CLI build | `cargo build --locked --bin summa` |
| API wasm check | `cargo check --locked -p summa-api --target wasm32-unknown-unknown` |
| API unit tests | `cargo test --locked -p summa-api --lib -- --test-threads=1` |
| install.sh / k8s / wrangler / release-please | `bash scripts/ci/validate-deploy.sh` |
| Live hub | `https://summa.duyet.net` (no local port) |

**Isolate every CLI drive.** Source the helper (creates a scratch `HOME`, empty credentials, temp DuckDB, and unsets secret env vars):

```bash
# shellcheck source=/dev/null
source .cursor/skills/verify-summa/helpers/isolated-env.sh
# sets: VERIFY_RUN_ID, VERIFY_EVIDENCE, VERIFY_SCRATCH, SUMMA_BIN,
#       SUMMA_CONFIG, SUMMA_CREDENTIALS, DUCKDB_PATH, HOME, XDG_*, …
```

Do not double-drive a shared live ingest. Isolated credentials are empty, so `summa import` must not POST `/v1/ingest`. If a token is in the process env or `./.env` (dotenv is loaded from cwd), stop and isolate — do not proceed.

**Teardown of launch:** nothing to keep alive. Cleanup kills only PIDs this run started (see Cleanup).

## Doctor

Read-only. Answers: is this instance worth driving? Run first whenever anything looks off. Do not write DuckDB, do not POST ingest, do not run `summa update` without `--dry-run`.

1. **CLI binary**

   ```bash
   "$SUMMA_BIN" --version
   "$SUMMA_BIN" --help
   ```

   Pass: exit 0, stdout starts with `summa `, help includes `import` / `check` / `serve` / `update`. Use the built `target/debug/summa`, not a random `summa` on `PATH`.

2. **API `/health`** (live hub, no auth)

   ```bash
   curl -sS -D - -o /tmp/summa-health.body \
     https://summa.duyet.net/health
   ```

   Pass: HTTP 200, JSON `"ok": true` and `"service": "summa"`. If this fails, do not drive live ingest/analytics; CLI-only features may continue.

3. **Isolated config (optional but cheap)**

   ```bash
   "$SUMMA_BIN" config --validate --config "$SUMMA_CONFIG"
   ```

   Pass: prints `config ok`. Must not print a real password (redact is `***` on `summa config` without `--validate`).

Fail doctor → stop that surface. Do not "fix" by pointing at the user's `~/.config/summa` or a live token.

## Drive

Prefer existing tests, then CLI subprocess + curl. Capture with [helpers/capture.sh](helpers/capture.sh).

**Tests first (no live writes):**

```bash
cargo test --locked -p summa-import -- --test-threads=1
cargo test --locked -p summa-api --lib -- --test-threads=1
bash scripts/ci/validate-deploy.sh
```

Narrow filters when proving one feature (see the feature file). `--test-threads=1` is required: config/credentials tests mutate process env.

**CLI subprocess** (after `source helpers/isolated-env.sh`):

```bash
.cursor/skills/verify-summa/helpers/capture.sh <action-slug> -- \
  "$SUMMA_BIN" <subcommand> [flags]
```

Use `--config "$SUMMA_CONFIG"` and `--duckdb-path "$DUCKDB_PATH"` on import/check. Always `--skip-clickhouse` on isolated import. `--dry-run` on import proves skips; observe that `$DUCKDB_PATH` is not created. <!-- pragma: allowlist secret -->

PTY/tmux only if a prompt appears (this CLI is non-interactive). `control-cli` / tmux is optional.

**HTTP** (hub):

```bash
.cursor/skills/verify-summa/helpers/capture.sh <action-slug> -- \
  curl -sS -D - -o "$VERIFY_EVIDENCE/actions/<action-slug>/body" \
    -X POST https://summa.duyet.net/v1/ingest \
    -H 'content-type: application/json' \
    -d '{"events":[]}'
```

Unauth ingest: empty `{"events":[]}` is enough. **Do not** send fabricated `cost` / token fields. Auth ingest against the live hub is out of scope unless the operator already has a real `telemetry_token` and explicitly wants a write — this skill never invents one.

Stable handles: clap subcommands (`import`, `check`, `serve`, `update`), flags (`--dry-run`, `--json`, `--skip-clickhouse`, `--duckdb-path`, `--config`), HTTP paths `/health` and `/v1/ingest`, header `Authorization: Bearer` or `X-Summa-Token`. <!-- pragma: allowlist secret -->

## Evidence

Root: **`$VERIFY_EVIDENCE`** which is `/tmp/verify-summa/<run-id>/` (also exported by the isolate helper). Cleanup must not delete this directory.

Layout:

```text
/tmp/verify-summa/<run-id>/
├── run.md
├── doctor/
│   ├── cli-version.txt
│   ├── cli-help.txt
│   └── api-health.txt
├── actions/<slug>/
│   ├── cmd.txt
│   ├── stdout.txt
│   ├── stderr.txt
│   ├── meta.json          # exit_code, http_status if curl
│   └── body               # HTTP body when using curl -o
└── cleanup.md
```

Proof standards:

- Exercise the real user path (the `summa` binary, `install.sh`, or live hub), not internal setters.
- Capture the **action and the resulting state** (exit code, file existence, HTTP status + body). A passing cargo test alone is incomplete when the feature map lists a CLI/HTTP path.
- Record `cost_usd` / dollar amounts **only as printed** by the binary or HTTP body. Never invent or recompute spend for the transcript.
- Side effects: after import `--dry-run`, `$DUCKDB_PATH` must not exist (or must be unchanged). After install `SUMMA_DRY_RUN=1`, no `summa` binary is placed except the dry-run mkdir of `SUMMA_INSTALL_DIR`.
- Redact secrets: run [helpers/redact.sh](helpers/redact.sh) on captured files before copying anything into the repo. Tokens look like `summa_…`; also redact `Bearer`, `CH_PASSWORD`, `MOTHERDUCK_TOKEN`.
- Mocks only where tests already isolate (CLI `serve` axum unit tests, update sha256 unit tests). Live hub `/health` and unauth `/v1/ingest` are not mocked.

Generator first-proof: [evidence/first-proof/](evidence/first-proof/) (redacted copy of one successful mapped drive, added once the generator has executed Launch → Doctor → Drive → Cleanup). Later runs write only under `/tmp/verify-summa/<run-id>/` unless asked to refresh first-proof.

## Cleanup

Kill only processes this run started (helper `VERIFY_PIDS`, or a python `http.server` you spawned for a local `install.sh` fixture). Never `pkill summa` / never kill by process name.

```bash
# if you sourced isolated-env.sh:
verify_summa_cleanup
```

Removes `$VERIFY_SCRATCH` (temp HOME, config, duckdb, install dir). **Does not** remove `$VERIFY_EVIDENCE`. After cleanup, `test -d "$VERIFY_EVIDENCE"` must still succeed.

Do not unregister the user's real `summa cronjob`. Isolated cron `--dry-run` never registers.

## Helpers

All under `.cursor/skills/verify-summa/helpers/`. Executable; invocation is above and here.

| Script | Invocation |
| --- | --- |
| `isolated-env.sh` | `source .cursor/skills/verify-summa/helpers/isolated-env.sh` |
| `capture.sh` | `.cursor/skills/verify-summa/helpers/capture.sh <slug> -- <cmd>…` |
| `redact.sh` | `.cursor/skills/verify-summa/helpers/redact.sh <file> [<file>…]` |

`isolated-env.sh` sets `update.mode=manual` in the scratch config so `auto_update_tick` (spawned on every CLI invocation) does not download a binary. It unsets `SUMMA_TELEMETRY_TOKEN`, `MOTHERDUCK_TOKEN`, `CH_PASSWORD`, `CURSOR_SESSION`, `CURSOR_API_KEY`.
