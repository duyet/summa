# Pending sources (research notes, not implemented)

Findings for tools named in the source-coverage goal that are **not** yet
imported. Recorded so the research is not repeated, and so nobody builds a
parser against guessed field names.

## Command Code (`command-code` / `cmd`) — NOT IMPLEMENTED

**Real tool, real local transcripts, but the token schema is unverified.**

Confirmed from https://commandcode.ai/docs/sessions:

| What | Value |
|---|---|
| Install | `npm i -g command-code` (Windows alias `cmdc`) |
| Transcripts | `~/.commandcode/projects/<project-slug>/<session-id>.jsonl` |
| Sidecars | `<id>.meta.json`, `<id>.checkpoints.jsonl`, `<id>.prompts.jsonl` |
| Format | append-only JSONL, first line is a header (session id, created, cwd) |
| Structure | entries form a **tree**; each points at its parent |
| Home override | none documented (unlike `PI_CODING_AGENT_DIR` / `FX_HOME`) |

**Why it is not implemented yet:** the per-entry usage and cost field names are
not published anywhere. `https://github.com/CommandCodeAI/command-code` is a
docs-only repository (`readme.md` plus `.github`, no source), so the transcript
writer cannot be read. The docs describe entries only in prose — "the model's
replies (with token usage and cost)" — without a schema.

The one confirmed shape is a *response* body from the `typesafe/jev` decision
model (`{"usage": {"input_tokens": 360, "output_tokens": 57}}`), which is
**not** the transcript format and must not be used to infer it.

Writing a parser from inference would import zero rows, or wrong ones, for
every user — worse than not shipping. To unblock: capture a real transcript
header and one billed entry and add them as a fixture.

**Hazards to handle when it is built** (both confirmed in the docs, both easy
to miss):

- `/fork` copies the **entire** session tree into a new file; `/clone`
  duplicates the active branch, and "the new file's header records the source
  transcript as its `parentSession`". Both put duplicated entries on disk, so
  a whole-directory scan bills the shared prefix twice. This is the same trap
  the pi source handles by deduping on entry `id`.
- Entries include compaction summaries and model/effort changes, and
  compaction usage counts toward session totals — so assistant-reply-only
  parsing under-reports, the same way it would for pi.

## grokbot — ALREADY COVERED

`grokbot` is not a separate CLI. It is a Cursor usage surface, already
classified as `source = "cursor-grok-bot"` in `apps/cli/src/source/cursor.rs`
(`is_grok_bot_event`, matched on a `grok-bot`/`grokbot`/`grok bot` signal or a
grok model name). Account-wide, so it carries `machine_name = "account"` and
dedupes across hosts. Nothing to build.
