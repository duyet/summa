# Cursor source

Account-wide Cursor usage (all machines on the Cursor account), not this host’s local editor files.

## Auth

First match wins:

1. `CURSOR_API_KEY` / credentials `cursor_api_key` → `POST https://api.cursor.com/teams/filtered-usage-events` (Basic auth)
2. `CURSOR_SESSION` / `CURSOR_COOKIE` / credentials `cursor_session` → `POST https://cursor.com/api/dashboard/get-filtered-usage-events` (Cookie + Origin)
3. Cursor.app `state.vscdb` key `cursorAuth/accessToken` (macOS: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`)

Missing credentials skip the source; the rest of `summa import` continues. `--skip-cursor` disables registration.

## Rows

- `machine_name` is always `account` so two hosts importing the same account do not double-count.
- `source` distinguishes surfaces: `cursor`, `cursor-cloud-agent` (`cloudAgentId` or `isHeadless`), `cursor-api` (`serviceAccountId`). Unclassifiable events still import as `cursor`.
- Grok Bot chat events (grok-bot signal or grok model) are **not** written here. They are the `grok-bot` source (`docs/knowledge/grok-bot.md`), which reads the same events and is gated by `--skip-grok-bot`. Exactly one of the two sources claims each event, so a chat is never counted twice under two source labels.
- Tokens from `tokenUsage` (`cacheWriteTokens` → `cache_creation_tokens`). Cost from `chargedCents / 100`, fallback `tokenUsage.totalCents / 100`.
