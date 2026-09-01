# Docs Index

- `install.sh` — `curl | bash` installer (`SUMMA_CHANNEL=beta|stable`; stable tarball, else rolling `beta`). Verifies the CI `.sha256` sidecar before extract.
- `scripts/ci/validate-deploy.sh` — CI checks for install.sh, k8s, wrangler.jsonc, release-please.
- `docs/install.md` — machine install, config, auto cron, telemetry hub.
- `docs/telemetry.md` — hosted hub at summa.duyet.net. Rust Worker: `apps/api`.
- `.env.example` — CLI + API Worker credentials (copy to `.env`).
- `deploy/k8s/summa-sidecar.yaml` — Hermes **import client** + sidebar iframe (summa.duyet.net) + hourly CronJob (not a local telemetry server).
- `deploy/k8s/hermes-values-summa.yaml` — k3s Hermes Helm overlay: import sidecar → cloud hub.
- `docs/k3s.md` — k3s / Hermes client (not an in-cluster telemetry server).
- `docs/knowledge/core-memory.md` — durable maintenance notes for automation runs.
- `docs/knowledge/antigravity.md` — integration details, architecture, and running guide for Antigravity source.
- `docs/knowledge/cursor.md` — Cursor account-wide usage source (dashboard/Admin APIs, surface labels).
- `docs/schema.sql` — single-table ClickHouse schema.
- `docs/migrate_add_source.sql` — migration adding `source`.
- `docs/queries.sql` — common query snippets.
