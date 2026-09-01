# Feature map

Verification source for **summa**. Drive features from this list; a proof that only exercises one convenient entry point is incomplete when the map lists others.

| File | User-facing surface | Default harness |
| --- | --- | --- |
| [cli-import.md](cli-import.md) | `summa import` → local DuckDB (optional cloud sinks) | CLI subprocess |
| [cli-check.md](cli-check.md) | `summa check` / `summa check --json` DuckDB summary | CLI subprocess |
| [cli-serve.md](cli-serve.md) | `summa serve` pings `https://summa.duyet.net/health` | CLI subprocess |
| [api-ingest-health.md](api-ingest-health.md) | Hub `GET /health`, `POST /v1/ingest` (401 unauth) | HTTP (curl) |
| [cli-install-update.md](cli-install-update.md) | `install.sh` + `summa update` sha256 skip | install.sh + CLI + cargo tests |

Keep each file's H2s (`Sub-features`, `How to get to it (user POV)`, `Driving it with …`, `Gotchas`) in sync with the code. After product changes, run `/maintain-verification-skill`.
