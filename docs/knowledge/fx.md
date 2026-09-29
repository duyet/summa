# fx (Vercel Labs)

fx is Vercel Labs' coding agent, a small Zig binary (vercel-labs/fx, Apache-2.0).
summa reads fx's own usage ledger — the same file `fx usage` reads, so the
numbers match what fx shows the user.

Install: `curl -fsSL https://fx.sh/setup.sh | bash`. Sign in with `fx login`
(AI Gateway), `fx login codex`, or `fx login grok`.

## Where the data lives

| What | Path |
|---|---|
| Profile root | `~/.fx`, or `$FX_HOME` |
| **Usage ledger** | `<root>/usage.jsonl` |
| Per-session snapshot | `<root>/sessions/<id>/usage-v2.json` |
| Conversations | `<root>/sessions/` |

summa reads `usage.jsonl`, not the per-session `usage-v2.json`. The ledger is
the profile-wide record that survives compaction and carries `created_at_ms`,
`model` and `total_cost` per request, which the per-session snapshot does not
retain in a directly importable form.

## Record shapes

JSON Lines, tagged by `kind`:

```json
{"schema_version":1,"kind":"coverage","started_at_ms":1700000000000}
{"schema_version":1,"kind":"generation","fact":{"id":"gen_01ARZ...","created_at_ms":1768435200000,"model":"anthropic/claude-sonnet-4-5","input_tokens":100,"output_tokens":20,"cache_read_tokens":30,"cache_write_tokens":0,"reasoning_tokens":null,"billable_web_search_calls":0,"total_cost":0.25}}
{"schema_version":1,"kind":"pending","id":"gen_...","observed_at_ms":1768435201000}
{"schema_version":1,"kind":"incident","occurred_at_ms":1768435202000,"completeness":"incomplete"}
```

## Three things this source gets right

**Only `generation` records are billable.** `pending` marks a request still
awaiting usage data from the provider, and `incident` records that coverage was
lost. Neither has settled token counts, so importing them would fabricate
spend. `coverage` just marks when tracking started.

**Token totals are the four-term sum.** fx reports input, output, cache-read
and cache-write separately and deliberately omits a `total_tokens` field, so
`total = input + output + cache_write + cache_read`. Cache counts once.

**A torn final line is dropped, not guessed.** fx appends whole lines and
compacts in place, so a partial tail is an interrupted write. Only complete
lines are parsed. Records with an unknown `schema_version` are ignored rather
than reinterpreted.

## Row shape

`daily` rows per (date, model), aggregating each day's settled requests.
`entries` counts requests. `model` is `provider/model` as fx logs it; the
`util::pricing` lookup matches on the model part, so gateway-prefixed names do
not fall through to the free tier.

Cost prefers the recorded `total_cost` and falls back to public rates.

## Skipping

`summa import --skip-fx`. A machine without fx is not an error: the source
skips silently when the ledger is missing.
