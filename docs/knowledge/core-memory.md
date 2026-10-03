# Core Memory

Small durable notes for ongoing maintenance automation.

## Scan scope commands

```bash
bun install --frozen-lockfile
git switch -c automation/<topic> origin/master
git worktree list --porcelain
git log --since='<last-run-iso>' --pretty=format:'%H %cI %s' --name-only
git log --since='7 days ago' --no-merges --pretty=format:'%h %cI %s'
rg -n "<symbol>" apps/cli/src apps/cli/tests -g '!**/*.test.ts' -g '!**/*.spec.ts'
```

## Known guardrails

- **Never `cargo build --release` on these machines** (macOS laptop is often low-memory; Linux home servers too). CI (`release.yml`) is the only builder. Install: `curl -fsSL https://summa.duyet.net/install.sh | bash` (GitHub raw also works). Env vars belong on bash (`curl … | SUMMA_SETUP_CRON=1 bash`), not on curl. Channels: `SUMMA_CHANNEL=beta` (rolling tag on every master push, default) or `stable` (release-please tags). `summa update --beta|--stable` switches channel; `summa config --set update.mode=auto` enables background auto-update.
- **Telemetry hub is https://summa.duyet.net** (Rust Worker `apps/api`), not local `summa serve`. Clients: `[telemetry] endpoint` + `credentials.toml` `telemetry_token` / `SUMMA_TELEMETRY_TOKEN`. `summa import` POSTs `/v1/ingest`. Worker stamps `account_id`/`api_key_id` and double-writes MotherDuck **and** ClickHouse (`dedup_key` replace, never scoped day-delete). D1 = keys/accounts only. Do not use `std::time::Instant` in the Worker (WASM panics → CF 1101). MotherDuck from the edge is MCP `https://api.motherduck.com/mcp` (`query` / `query_rw`). ClickHouse from the edge is `https://clickhouse-homelab.duyet.net` via the homelab-k8s tunnel + Access service token (`CF_ACCESS_CLIENT_*`). Sync: `bash scripts/sync-worker-secrets.sh`. `/ping` (API key) sink latency; `/v1/analytics` + `/v1/analytics/summary` for burn.duyet.net (ClickHouse, then MotherDuck fallback; `cost_per_day` = cost / inclusive calendar days). `summa serve` only pings the cloud hub. Cron: `summa update` then `summa import`. k8s: Hermes pod runs **import** (client); sidebar iframe https://summa.duyet.net; hourly CronJob; secrets `SUMMA_TELEMETRY_ENDPOINT` + `SUMMA_TELEMETRY_TOKEN`. Manifest: `deploy/k8s/summa-sidecar.yaml`.

- `run-import.sh` is Bun-only; do not add npm/yarn fallback.
- `src/scripts/setup-cronjob.ts` must write crontab via stdin (`crontab -`), not shell-quoted `echo`.
- Rust `summa cronjob`: generate+register launchd / systemd --user / crontab. Crontab updates go through `crontab -` stdin (never `/tmp` + `crontab file`). Status reports legacy `run-import.sh` lines; `--replace` removes them.
- Keep sink dedup delete filters SQL-escaped in both ClickHouse and DuckDB sinks.
- CLI ClickHouse import writes via staging table `ccusage_events__swap` + `EXCHANGE TABLES` (never scoped `ALTER DELETE` then insert). Live `ccusage_events` is unchanged until the exchange. Telemetry ingest is insert-only (`ReplacingMergeTree(updated_at)`). <!-- pragma: allowlist secret -->
- Companion (`codex`/`opencode`) totals must avoid cache double-count: `total_tokens = inputTokens + outputTokens`.
- Claude totals must keep cache components separate: `total_tokens = input + output + cacheCreation + cacheRead`.
- **Rust serde must alias ccusage camelCase** (`inputTokens`, `totalTokens`, `cacheCreationTokens`, `cacheReadTokens`, `modelsUsed`, `modelBreakdowns`). Missing aliases silently zero tokens while `totalCost` still parses → burn.duyet.net “0 tokens / $cost” daily bars (hit ~2026-07-10 after Rust import path). Regression tests in `parser::types` + `parser::rows`.
- Grok Build (`source=grok`, `GROK_HOME`/`~/.grok`): prompt is cache-inclusive; `input = prompt - cached`, `cache_read = cached`, `output = completion`, `total = prompt + completion` (do not add reasoning again). Session model/cwd from `sessions/**/summary.json`. Logs have **no cost field** — estimate from xAI public rates (`estimate_grok_cost`): grok-4.5 $2/$0.30/$6 per 1M (input/cached/output), long-context ≥200k prompt doubles to $4/$0.60/$12; priced **per turn** then summed.
- Grok CLI-proxy (`source=grok-api`, `machine_name=account`): `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` with `~/.grok/auth.json`. Import only countable spend/token payloads. `creditUsagePercent`-only responses must not become fake turn rows. `--skip-grok` skips both local grok and grok-api.
- Cursor account-wide (`machine_name=account`, never importer hostname): CodexBar dashboard/Admin usage-events APIs. Classify `cursor` / `cursor-cloud-agent` (`cloudAgentId` or `isHeadless`) / `cursor-api` (`serviceAccountId`); Grok Bot chat events go to the `grok-bot` source instead (cursor skips them — one owner per event). Cost = `chargedCents/100` (fallback `tokenUsage.totalCents/100`). Tokens from `tokenUsage` (`cacheWriteTokens` → cache_creation). `--skip-cursor`. Missing session/API key skips that source.
- **Grok Bot** (`source=grok-bot`): Grok Bot/x.ai **chat** tokens, a different surface from Grok Build (`source=grok`). No local Grok Bot ledger exists (`~/.cursor/ai-tracking/ai-code-tracking.db` tracks code lines, not tokens), so the rows come from the same account usage-events feed Cursor reads (`tokenUsage`, non-cached four-term total; cost = reported cents only, else 0). Own `--skip-grok-bot`, **default false**, independent of `--skip-grok` and `--skip-cursor`. `docs/knowledge/grok-bot.md`.
- **Devin stays first-class**: `devin` registers unless `skip_devin`/`--skip-devin` is set. Never disable an agent source on a machine to silence a missing CLI — an absent CLI is already an empty source, and `--skip-devin` silently stops ATIF transcripts from importing.
- **Shared pricing** (`util/pricing.rs` → `estimate_model_cost` / `resolve_reported_cost`): Antigravity always estimates (was hardcoded $0). Hermes uses DB cost when sane, else public rates. **Sanity check applies only above $1** — a per-token ratio is meaningless on a small sample (a real $0.25 request on 150 tokens is $1667/1M and was being discarded, zeroing a day of spend); above $1, reject if blended >$200/M or >50× the estimate (Hermes `estimated_cost_usd` produced $100k+ for small volumes). **No per-record rounding** — callers sum many record costs into a row, so rounding each first turned $0.0006 turns into $0.00.
- **Pricing tiers** (USD/MTok, refreshed against the provider pages; every rate has a test): Claude opus 4/4.1 → $15/$1.50/$75 (still sold on Bedrock/GCP), opus 4.5+ → $5/$0.50/$25, sonnet 4.x → $3/$0.30/$15, sonnet 5+ → $2/$0.20/$10, haiku 3.5 → $0.80/$0.08/$4, haiku 4.5 → $1/$0.10/$5. Gemini 3-flash → $0.50/$0.05/$3, 3.5-flash → $1.50/$0.15/$9, 3.6+ flash → $0.75/$0.075/$3.75, 2.5/3.x flash-lite → $0.25-0.30, 2.5-pro → $1.25/$10, 3.1-pro → $2/$12. OpenAI gpt-5.5 → $5/$0.50/$30, gpt-5/5.4 → $2.50/$0.25/$15, gpt-5.4-mini → $0.75/$4.50, gpt-5.4-nano → $0.20/$1.25, gpt-5.3-codex → $1.75/$14, gpt-4.1 → $2/$8, gpt-4o → $2.50/$10, o3 → $2/$8, o4-mini → $1.10/$4.40, o1 → $15/$60. free/* and gpt-oss → $0.
- **Cache billing differs by provider.** `ModelRates.input_includes_cache` is true for OpenAI: its prompt tokens include the cached prefix (and this repo's Codex normalisation keeps them that way), so cache is billed as a discount *within* input. Anthropic/Google report cache separately, so it is added on top. Getting this backwards double-counts the largest token class in coding-agent traffic.
- **pi** (`source=pi`): `~/.pi/agent/sessions/--<path>--/*.jsonl` (`PI_CODING_AGENT_DIR`). Usage rides on FOUR entry types — assistant `message`, `usage` (e.g. `cache_warm`), `compaction`, `branch_summary` — all of which bill. `/fork` and `/clone` copy the parent's entries into a new file, so dedupe on entry `id` or the shared prefix is billed twice. `cwd` comes from the header, not the lossy dir name. Runbook: `docs/knowledge/pi.md`.
- **fx** (`source=fx`, Vercel Labs): `~/.fx/usage.jsonl` (`FX_HOME`) — the same ledger `fx usage` reads. Only `kind=generation` facts are billable; `pending` has no settled counts and `incident` records lost coverage. Drop a torn final line (fx appends whole lines). `docs/knowledge/fx.md`.
- **Command Code** (`source=command-code`): `~/.commandcode/projects/<slug>/<id>.jsonl` (`COMMANDCODE_HOME`). `message.usage.{inputTokens,outputTokens,cacheReadTokens,cacheWriteTokens,costUsd?}`; **`inputTokens` is non-cached**. `/fork` copies the whole tree, `/clone` the active branch — dedupe on entry `id`. `costUsd` is the CLI's own estimate. Schema came from the npm bundle, not the docs (the GitHub repo is docs-only). `docs/knowledge/command-code.md`.
- **Devin CLI** (`source=devin`): install `curl -fsSL https://cli.devin.ai/install.sh | bash` + `devin setup` (headless `--force-manual-token-flow`) on every host; `~/.local/share/devin/cli`. ATIF-v1.7 counts live **only** in `final_metrics` — scan `agent_logs/devin-<sid>.json` **and** legacy `transcripts/<sid>.json`, dedupe by session id keeping the larger metrics. `sessions.db` read-only is optional metadata. Prompt is cache-inclusive (`input = prompt - cached`, `total = prompt + completion`). No transcript → no row. Runbook: `docs/knowledge/devin.md`.
- **Antigravity emit rule**: only decoded SQLite `gen_metadata` token+timestamp blobs become `source=antigravity`. Encrypted leftover `conversations/*.pb` and `implicit/*.pb` are ignored (no per-prompt / per-byte fabrications). Import purges prior antigravity rows in DuckDB/MotherDuck before rewrite so stale estimates cannot linger. `gemini` is a different companion source and must not be labeled Antigravity.
- TypeScript 6: avoid `baseUrl` in `tsconfig.json`; keep path aliases with explicit `./src/...` prefixes.
- In fresh clones/worktrees without `node_modules`, run `bun install --frozen-lockfile` before `bunx tsc --noEmit` to avoid false missing-module/type errors.
- In restricted environments where Bun cannot write temp files, run checks with `BUN_TMPDIR="$PWD/.tmp/bun-tmp"` and `BUN_INSTALL_CACHE_DIR="$PWD/.tmp/bun-install-cache"`.
- In Codex worktrees that start on detached `HEAD`, create a branch from `origin/master` before making automation commits/PRs.
- If git operations fail in a linked worktree with `.git/worktrees/.../*.lock` permission errors, run branch/fetch/push from the owning checkout identified by `git worktree list --porcelain`.

## Routine operations

- Full import: `cargo run --bin summa -- import --verbose` (from `apps/cli`)
- DuckDB backfill from ClickHouse: `cargo run --bin summa -- backfill-duckdb`

## CI / release

- Workspace is virtual: CI and publish **must** use `-p summa-import` (CLI) and `-p summa-api` (Worker). Never `cargo publish` / `cargo package` at the workspace root.
- `scripts/ci/validate-deploy.sh` gates `install.sh` dry-run, k8s manifests, wrangler.jsonc + D1 migrations, and release-please package path (`apps/cli` + `cargo-workspace` plugin).
- release-please: package `apps/cli` (crate `summa-import`), not `.`. Do **not** auto-merge `release-please--*` PRs.
- Wrangler dry-run is a CI job (`wrangler deploy --dry-run`); live deploy is `bun run deploy:api` with Cloudflare creds.

## CI and archived Python docs

- `docs/archive/python/pyproject.toml` should keep `requires-python` aligned with dependency floors to avoid Dependabot security-update resolution failures.
- If that archived lockfile churn is not needed, consider disabling that Dependabot ecosystem in repo settings/config.
