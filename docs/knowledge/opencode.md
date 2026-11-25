# OpenCode source

`source=opencode`, read natively by `apps/cli/src/source/opencode.rs`. Skip with `--skip-opencode` / `skip_opencode`.

## Why native

OpenCode used to come in through the ccusage companion (`@ccusage/opencode`). After OpenCode moved its storage to `session_message`, the companion stopped returning rows (nothing newer than early October 2026 on an opencode 1.19.6 host). Dedup keys differ between implementations, so one source owns `opencode`; it is not in `COMPANION_AGENTS`, and a test guards that.

## Where it reads

Data dir: `OPENCODE_DATA_DIR` (also `importer.opencode_path`), else `$XDG_DATA_HOME/opencode`, else `~/.local/share/opencode`. Files: `opencode.db` and, when present, `opencode-tui-v2.db`. Each is opened through `util::sqlite::SqliteSnapshot` (a private copy including the WAL), so a running OpenCode is never touched. Tables that are missing are skipped, so any storage version works.

| table | rows used | model | project |
|---|---|---|---|
| `session_message` (current) | `type = 'assistant'` | `data.model.id` | `session_v2.directory` |
| `message` (legacy) | `data.role = 'assistant'` | `data.modelID` | `data.path.cwd` |

OpenCode migrates old history into `session_message`, so both tables hold the same message ids. Messages are deduped on id (current table wins), so history is counted once.

## Rows

One `session` row per session + date + model, one `daily` row per date + model. Date is the message's `time.created` (UTC), falling back to the column.

| field | value |
|---|---|
| `input_tokens` | `tokens.input` (already non-cached) |
| `cache_read_tokens` | `tokens.cache.read` |
| `cache_creation_tokens` | `tokens.cache.write` |
| `output_tokens` | `tokens.output` |
| `reasoning_tokens` | `tokens.reasoning` (reported, not added to total) |
| `total_tokens` | input + output + cache_read + cache_write |

## Cost

`data.cost` when positive and sane, otherwise `util::pricing` public rates via `resolve_reported_cost`. Free-tier models stay at 0.

## Not covered

The pre-SQLite JSON file storage (`storage/message/*.json`) is not read; OpenCode migrates it into the database on upgrade.
