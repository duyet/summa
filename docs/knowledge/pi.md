# pi (pi coding agent)

Pi is a TypeScript coding-agent CLI (`@earendil-works/pi-coding-agent`, pi.dev).
summa reads its session transcripts. No auth is needed: the files are local.

Install: `npm install -g --ignore-scripts @earendil-works/pi-coding-agent`
(or `curl -fsSL https://pi.dev/install.sh | sh`). Node 20+ or Bun.

## Where the data lives

| What | Path |
|---|---|
| Agent dir | `~/.pi/agent`, or `$PI_CODING_AGENT_DIR` |
| Sessions | `<agent-dir>/sessions/--<path>--/*.jsonl` |
| Credentials (unused by summa) | `<agent-dir>/auth.json` |

The `--<path>--` directory is the project cwd with the leading `/` removed and
`/`, `\`, `:` replaced by `-`. It is lossy, so the importer reads the real `cwd`
from the session header rather than trying to invert the encoding.

## Entry shapes

JSON Lines, one entry per line, discriminated by `type`.

```json
{"type":"session","version":3,"id":"uuid","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/path/to/project"}
{"type":"message","id":"m1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":100,"output":20,"cacheRead":50,"cacheWrite":10,"totalTokens":180,"cost":{"input":0.3,"output":0.3,"cacheRead":0.015,"cacheWrite":0.0375,"total":0.6525}}}}
{"type":"usage","id":"u1","timestamp":"2024-12-03T14:08:00.000Z","kind":"cache_warm","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":0,"output":0,"cacheRead":50000,"cacheWrite":0,"totalTokens":50000,"cost":{...}}}
```

`total` and `cost.*` are optional. `reasoning` is not reported separately.

## Three things this source gets right

**Usage is not only on assistant messages.** Four entry types bill real
tokens, and all four count toward pi's own session totals:

| Entry | Where usage lives |
|---|---|
| `message` (role `assistant`) | `message.usage` |
| `usage` (e.g. `kind: "cache_warm"`) | `usage` |
| `compaction` | `usage` (the summarization request) |
| `branch_summary` | `usage` (the branch summary request) |

Importing only assistant messages silently drops cache warming and every
summarization turn.

**Forked sessions are not double counted.** Sessions form a tree.
`/fork` and `/clone` write a *new* file whose header carries
`parentSession` pointing at the original, and the new file contains a copy of
the parent's entries. Both files sit on disk, so scanning both bills the shared
prefix twice. summa dedupes entries by their `id` across the whole scan — the
entry id, not the file name, is the identity.

**`totalTokens` is the provider's own total and already includes cache.** It is
trusted when present; the four-term sum
(`input + output + cacheWrite + cacheRead`) is the fallback. Cache is counted
once, never twice.

## Row shape

`session` rows per (session, model) and `daily` rows per (date, model), so a
session that switched models is exploded into one row per model. `entries`
counts billable turns.

Cost prefers `usage.cost.total` and falls back to `util::pricing` public rates
when absent. Zero-token turns produce no row.

## Not read via ccusage

pi is read **natively** and is deliberately absent from the `ccusage`
subprocess list in `import_all.rs`. Two parsers claiming the same
`source = "pi"` write the same dedup key, so one silently overwrites the
other and the surviving row depends on which finished last — meaning a
parser that miscounts forks could inflate the dashboard nondeterministically.
`companion_source_ids_exclude_natively_covered_agents` guards the list, and
`no_source_name_is_registered_twice` guards the wider set.

The native reader is also strictly better here: it dedupes forks and counts
the `usage`, `compaction` and `branch_summary` entries. `ccusage` also ignores
`PI_CODING_AGENT_DIR`, so it would read the real `~/.pi` even when the native
source is pointed at a fixture.

## Skipping

`summa import --skip-pi`. A machine without pi is not an error: the source
skips silently when the sessions directory is missing.
