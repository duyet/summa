# Grok Bot source

Grok Bot (x.ai) **chat** token usage, written as `source = "grok-bot"`.

## Schema used, and why

Grok Bot chat usage is served by the **same** usage-events API the Cursor source
reads — `apps/cli/src/source/cursor.rs`:

| Field | Meaning |
|---|---|
| `usageEventsDisplay[]` / `usageEvents[]` | one entry per chat request |
| `timestamp` (ms, string or number) | when the chat happened |
| `model`, `kind` | grok model / `"Grok Bot"` kind — the signal (`is_grok_bot_event`) |
| `tokenUsage.inputTokens` | uncached prompt tokens |
| `tokenUsage.outputTokens` | completion tokens |
| `tokenUsage.cacheWriteTokens` | → `cache_creation_tokens` |
| `tokenUsage.cacheReadTokens` | → `cache_read_tokens` |
| `chargedCents`, `tokenUsage.totalCents` | USD = cents / 100 |

`tokenUsage` is the non-cached four-term convention, so
`total = input + output + cache_creation + cache_read` (cache is counted once,
never twice).

There is **no** separate Grok Bot token ledger to read instead:

- `~/.cursor/ai-tracking/ai-code-tracking.db` tracks AI-written *code lines*
  (`ai_code_hashes`, `scored_commits`), not tokens.
- `~/.grok/logs/unified.jsonl` is Grok Build's own shell log
  (`shell.turn.inference_done`), collected by `source::grok` — not chat.

So this source reuses Cursor's auth + paging (`fetch_cursor_usage_events`) and
maps only the events Cursor classifies as the Grok Bot surface. One API
response, two mappers.

## Auth

Same as Cursor, first match wins:

1. `CURSOR_API_KEY` / credentials `cursor_api_key` → `POST https://api.cursor.com/teams/filtered-usage-events` (Basic auth)
2. `CURSOR_SESSION` / `CURSOR_COOKIE` / credentials `cursor_session` → `POST https://cursor.com/api/dashboard/get-filtered-usage-events` (Cookie + Origin)
3. Cursor.app `state.vscdb` key `cursorAuth/accessToken`

Missing credentials → empty result, not an error; the rest of the import runs.

## One owner per event

`cursor.rs` **skips** every event it classifies as Grok Bot, and this source
keeps only those. Both derive `dedup_key` from the source, so writing the same
chat from both sides would leave two keys, survive ReplacingMergeTree, and count
the chat twice. The invariant is asserted in
`cursor::cursor_and_grok_bot_claim_disjoint_events` and
`grok_bot::cursor_and_grok_bot_never_both_claim_an_event`.

Rows are account-wide: `machine_name = "account"`, never the importer hostname,
so two hosts importing the same account emit identical dedup keys.

## Cost

Reported cents only (`chargedCents`, fallback `tokenUsage.totalCents`). No rate
is invented: an event with neither field imports its tokens at cost 0. Nothing
is estimated from a token count, and no turn is fabricated for an event the feed
did not report.

## Flags

- `--skip-grok-bot` / `[importer] skip_grok_bot` — **default false (enabled)**.
- Independent of `--skip-grok` (Grok Build local logs + `grok-api` billing) and of
  `--skip-cursor`.

Rows go through the normal pipeline, so they land in ClickHouse / DuckDB /
MotherDuck and in the telemetry hub (`TelemetrySink`) exactly like every other
source. Hub rejections and sink errors are reported, not swallowed.

## Host profile (Grok Bot box, `machine_name = "cursor"`)

Enabled, with no `skip_*` keys at all:

```toml
# ~/.config/summa/config.toml
[importer]
machine_name = "cursor"
# opencode, codex, antigravity, hermes, grok (Build), devin, cursor, pi, fx,
# command-code and grok-bot are all enabled by default. Do not add skip_* here.
```

`devin` is a first-class source: it registers unless `skip_devin` /
`--skip-devin` is set, so Devin ATIF transcripts keep importing. Never set
`skip_devin` to silence an uninstalled CLI — a missing CLI is already an empty
source, not a failure.