# Command Code

Command Code (`command-code`, CLI `cmd`; `cmdc` on Windows) is a coding agent
for open models by Ahmad Awais / CommandCodeAI. summa reads its session
transcripts. No auth needed — the files are local.

Install: `npm i -g command-code`.

## Where the data lives

| What | Path |
|---|---|
| Home | `~/.commandcode`, or `$COMMANDCODE_HOME` |
| Transcripts | `~/.commandcode/projects/<project-slug>/<session-id>.jsonl` |
| Sidecars | `<id>.meta.json`, `<id>.checkpoints.jsonl`, `<id>.prompts.jsonl` |

The project directory is a lossy slug of the working directory, so `cwd` is
read from the session header rather than inverted from the slug.

## Entry shapes

Verified against the shipped CLI (command-code 1.69.0), not inferred from docs.

```json
{"type":"session","version":3,"id":"<uuid>","timestamp":"<ISO>","cwd":"/Users/me/app"}
{"type":"message","id":"<entry-id>","parentId":null,"timestamp":"<ISO>","message":{"role":"assistant","model":"kimi-k2","usage":{"inputTokens":100,"outputTokens":20,"cacheReadTokens":50,"cacheWriteTokens":10,"costUsd":0.42}}}
```

Only `type: "message"` entries with `message.role == "assistant"` bill tokens.

## Three things this source gets right

**`inputTokens` is non-cached.** Cache arrives separately via the CLI's
`inputTokenDetails` (`cacheReadTokens` / `cacheWriteTokens`), so the total is
the four-term sum and cache is counted once. Getting this backwards would
double-count the largest token class, since cache reads dominate coding-agent
traffic.

**Forks and clones are deduped.** `/fork` copies the *entire* session tree into
a new file and `/clone` duplicates the active branch; both write a copy of the
source entries with a header whose `parentSession` points at the original. Both
files sit on disk, so a whole-directory scan bills the shared prefix twice.
Entries dedupe on their `id` — the entry id, not the filename, is the identity.

**`costUsd` is preferred but sanity-checked.** The CLI computes it locally via
`estimateCostUsd({model, usage})`, so it is an estimate rather than an invoice.
It is passed through `resolve_reported_cost`, which falls back to public rates
if it is missing or implausible. Absent it, `util::pricing` prices the turn.

## Known limitation

The CLI persists compaction statistics to the session's `<id>.meta.json`, not
into the transcript, and branch summaries are not written as billed entries.
So compaction and branch-summary turns are **not** in these rows. This is
recorded rather than estimated — no fabricated turns.

## Row shape

`session` rows per (session, model) and `daily` rows per (date, model), so a
session that switched models is exploded into one row per model. `entries`
counts billed turns.

## Skipping

`summa import --skip-command-code`. A machine without Command Code is not an
error: the source skips silently when `projects/` is missing.
