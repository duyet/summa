# Devin source

Local Devin CLI (Cognition) usage, read from disk. `source = devin`. `--skip-devin` disables registration.

## Install the CLI

```bash
curl -fsSL https://cli.devin.ai/install.sh | bash    # macOS / Linux / WSL
brew install --cask devin-cli                       # macOS alternative
```

Installs `~/.local/bin/devin`. Then `devin setup` (or `devin auth login`) once per machine; on a headless host use `devin setup --force-manual-token-flow` or `devin auth login --force-manual-token-flow`. Check with `devin auth status`.

`summa` needs no Devin credentials — it only reads local files — but sessions only produce usage data once the CLI is authenticated.

## Data

Resolved dir: `DEVIN_HOME` → `XDG_DATA_HOME/devin` → `~/.local/share/devin`, then the `cli/` subdir.

- **ATIF transcripts** — the only local token counts. Read from both `agent_logs/devin-<sid>.json` (the current `--export` default) and `transcripts/<sid>.json` (the older default). A session present in both is imported once, keeping whichever `final_metrics` is larger. A session with no transcript is skipped, never estimated.
- **`sessions.db`** (SQLite, opened read-only) — `sessions` table supplies `working_directory` (→ `project_path`), `model`, and `created_at` / `last_activity_at`. Optional: a transcript still imports without it.

ATIF-v1.7 puts counts in `final_metrics` only; per-step entries carry no usage. Verified against Devin CLI 3000.11.3.

## Rows

One `session` row per transcript, one `daily` row per date+model, same shape as the Grok and Hermes sources.

`total_prompt_tokens` is cache-inclusive, so cached tokens are split out of input rather than added on top (the Codex / Grok rule):

| field | value |
|---|---|
| `input_tokens` | `total_prompt_tokens - total_cached_tokens` |
| `cache_read_tokens` | `total_cached_tokens` |
| `output_tokens` | `total_completion_tokens` |
| `reasoning_tokens` | `0` (not reported per step) |
| `total_tokens` | `total_prompt_tokens + total_completion_tokens` |
| `entries` | `final_metrics.total_steps` |

Model id precedence: `sessions.db.model` → the last step's `extra.generation_model` (canonical, e.g. `swe-2-high`) → `agent.model_name` (display label, e.g. `SWE-2 High`) → `unknown`. The ATIF display name is not an id, so the canonical value is preferred wherever it exists.

Date = the final step timestamp, falling back to `sessions.last_activity_at`, then `sessions.created_at`. `sessions.metadata` carries `total_credit_cost` / `total_acu_cost`, but Devin reports `0` under zero-data-retention, so it is not read as cost.

## Cost

Devin bills in ACU / credits and exposes no local USD figure, so cost is estimated from `util::pricing` public rates — the same treatment Grok Build gives its cost-less logs. When a transcript does report `final_metrics.total_cost_usd`, that value is used instead, via `resolve_reported_cost`, which rejects an implausible blended rate in favour of the estimate.
