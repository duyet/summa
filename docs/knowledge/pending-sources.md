# Pending sources (research notes, not implemented)

Findings for tools named in the source-coverage goal that are **not** imported.
Recorded so the research is not repeated, and so nobody builds a parser against
guessed field names.

## grokbot — now its own source (`grok-bot`)

`grokbot` is not a separate CLI, and there is no Grok Bot token ledger on disk:
the chat counts live in Cursor's account usage-events feed (`tokenUsage` per
chat). It used to be covered only as the `cursor-grok-bot` sub-label of
`source::cursor`, which tied it to `--skip-cursor` and hid it from any query
looking for a `grok*` source.

It is now `source = "grok-bot"` (`apps/cli/src/source/grok_bot.rs`) with its own
`--skip-grok-bot` flag, independent of `--skip-grok` and `--skip-cursor`. The
Cursor source skips the events this source claims, so each chat is still written
exactly once. Schema and auth: `docs/knowledge/grok-bot.md`.

## How Command Code was unblocked

Worth recording as a method, because the docs alone were not enough.

`https://github.com/CommandCodeAI/command-code` is **docs-only** (`readme.md`
plus `.github`, no source), so the transcript writer cannot be read there, and
https://commandcode.ai/docs/sessions describes entries only in prose — "the
model's replies (with token usage and cost)" — with no schema.

The published npm tarball contains the real thing:

```bash
npm view command-code version dist.tarball
curl -fsSL "$(npm view command-code dist.tarball)" -o cc.tgz && tar xzf cc.tgz
```

`package/dist/cli.mjs` is a 2.7 MB minified bundle, but greppable. The
authoritative facts came from three sites:

| Question | How it was answered |
|---|---|
| Which entries bill? | `function isAssistantMessageEntry(e){return"message"===e.type&&"assistant"===e.message.role}` |
| What does the persisted usage look like? | `function toSessionUsage(e){return{inputTokens:…,outputTokens:…,cacheReadTokens:…,cacheWriteTokens:…,…costUsd…}}` |
| Where does it sit, and is cost billed? | `appendMessage({message:s, …{usage:toSessionUsage({usage:c.usage,costUsd:e.estimateCostUsd?.(…)})…}})` |
| Is `inputTokens` cache-inclusive? | Cache is read from `e.inputTokenDetails?.cacheReadTokens`, i.e. separate from `inputTokens` — so it is the non-cached four-term convention |
| Does forking duplicate entries? | The fork/clone writer is `[…entries].map(e=>JSON.stringify(e))` into a new file with `parentSession` set |

Search the bundle with `python3` over the file rather than `grep -o` — the
minified lines are megabytes long, and a short window around each match is what
makes the output readable.

**Generalisable:** when an agent CLI's docs describe local logs but publish no
schema, the npm/pypi/Homebrew tarball is the primary source, not the docs site.
Check `contents/` on the GitHub repo first to see whether it is docs-only, then
fall back to the package. This unblocked Command Code, which had been written
up as "not implementable" for exactly that reason.
