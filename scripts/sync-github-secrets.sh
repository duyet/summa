#!/usr/bin/env bash
# Push the CI/release secrets from a local env file to GitHub Actions secrets.
#
#   bash scripts/sync-github-secrets.sh            # sync from .env.local (else .env)
#   bash scripts/sync-github-secrets.sh --dry-run  # show what would be set
#
# Source file: $SUMMA_ENV_FILE, else .env.local, else .env (repo root).
# Only the allowlisted names below are read; everything else in the file (ClickHouse,
# MotherDuck, ...) stays local. Values are piped to `gh` over stdin and never printed.
# The file is parsed, not sourced, so nothing in it is executed.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SECRETS=(CLOUDFLARE_API_TOKEN CLOUDFLARE_ACCOUNT_ID CARGO_REGISTRY_TOKEN)

dry_run=0
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=1 ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

if [[ -n "${SUMMA_ENV_FILE:-}" ]]; then
  env_file="$SUMMA_ENV_FILE"
elif [[ -f "$ROOT/.env.local" ]]; then
  env_file="$ROOT/.env.local"
else
  env_file="$ROOT/.env"
fi
[[ -f "$env_file" ]] || { echo "no env file found (tried .env.local, .env); set SUMMA_ENV_FILE" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "gh CLI is required" >&2; exit 1; }

# Last assignment wins. Strips optional "export ", one pair of quotes and a trailing CR.
env_get() {
  local name="$1" line value
  line="$(grep -E "^[[:space:]]*(export[[:space:]]+)?${name}=" "$env_file" | tail -n1 || true)"
  [[ -n "$line" ]] || return 0
  value="${line#*=}"
  value="${value%$'\r'}"
  if [[ "$value" =~ ^\"(.*)\"$ || "$value" =~ ^\'(.*)\'$ ]]; then
    value="${BASH_REMATCH[1]}"
  fi
  printf '%s' "$value"
}

echo "source: ${env_file#"$ROOT"/}"
missing=0
for name in "${SECRETS[@]}"; do
  value="$(env_get "$name")"
  if [[ -z "$value" ]]; then
    echo "missing $name (not set or empty in the env file)"
    missing=1
    continue
  fi
  if [[ "$dry_run" -eq 1 ]]; then
    echo "would set $name"
  else
    printf '%s' "$value" | gh secret set "$name" >/dev/null
    echo "set $name"
  fi
done

# Fail loud: a silent partial sync is how the deploy job ended up skipping.
exit "$missing"
