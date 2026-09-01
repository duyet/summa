# Isolated HOME / XDG / config / DuckDB for summa verification.
# Usage (must be sourced so exports persist):
#   source .cursor/skills/verify-summa/helpers/isolated-env.sh
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  echo "error: source this file: source $0" >&2
  exit 2
fi

_repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
VERIFY_REPO_ROOT="${VERIFY_REPO_ROOT:-$_repo_root}"
VERIFY_RUN_ID="${VERIFY_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
VERIFY_EVIDENCE="${VERIFY_EVIDENCE:-/tmp/verify-summa/${VERIFY_RUN_ID}}"
VERIFY_SCRATCH="${VERIFY_SCRATCH:-${VERIFY_EVIDENCE}/scratch}"
SUMMA_BIN="${SUMMA_BIN:-${VERIFY_REPO_ROOT}/target/debug/summa}"
VERIFY_PIDS="${VERIFY_PIDS:-}"

mkdir -p \
  "$VERIFY_EVIDENCE/doctor" \
  "$VERIFY_EVIDENCE/actions" \
  "$VERIFY_SCRATCH/home/.config/summa" \
  "$VERIFY_SCRATCH/home/.local/share/summa" \
  "$VERIFY_SCRATCH/home/.local/bin" \
  "$VERIFY_SCRATCH/xdg-config" \
  "$VERIFY_SCRATCH/xdg-data" \
  "$VERIFY_SCRATCH/xdg-cache" \
  "$VERIFY_SCRATCH/install"

export HOME="$VERIFY_SCRATCH/home"
export XDG_CONFIG_HOME="$VERIFY_SCRATCH/xdg-config"
export XDG_DATA_HOME="$VERIFY_SCRATCH/xdg-data"
export XDG_CACHE_HOME="$VERIFY_SCRATCH/xdg-cache"
export SUMMA_INSTALL_DIR="$VERIFY_SCRATCH/install"
export DUCKDB_PATH="$VERIFY_SCRATCH/home/.local/share/summa/summa.duckdb"
export SUMMA_CONFIG="$HOME/.config/summa/config.toml"
export SUMMA_CREDENTIALS="$HOME/.config/summa/credentials.toml"

unset SUMMA_TELEMETRY_TOKEN MOTHERDUCK_TOKEN CH_PASSWORD CH_HOST \
  CURSOR_SESSION CURSOR_COOKIE CURSOR_API_KEY BOOTSTRAP_TOKEN \
  CLERK_SECRET_KEY CLERK_PUBLISHABLE_KEY 2>/dev/null || true

cat > "$SUMMA_CONFIG" <<'EOF'
[clickhouse] # pragma: allowlist secret
host = "127.0.0.1"
port = 8123
user = "default"
database = "default"
protocol = "http"

[importer]
days_back = 2
skip_clickhouse = true # pragma: allowlist secret

[update]
channel = "beta"
mode = "manual"

[telemetry]
endpoint = "https://summa.duyet.net"
EOF

: > "$SUMMA_CREDENTIALS"
chmod 600 "$SUMMA_CREDENTIALS" 2>/dev/null || true

export VERIFY_RUN_ID VERIFY_EVIDENCE VERIFY_SCRATCH SUMMA_BIN VERIFY_REPO_ROOT VERIFY_PIDS

verify_summa_cleanup() {
  if [[ -n "${VERIFY_PIDS:-}" ]]; then
    local pid
    for pid in $VERIFY_PIDS; do
      kill "$pid" >/dev/null 2>&1 || true
    done
  fi
  if [[ -n "${VERIFY_SCRATCH:-}" && -d "${VERIFY_SCRATCH:-}" ]]; then
    rm -rf "$VERIFY_SCRATCH"
  fi
  if [[ -n "${VERIFY_EVIDENCE:-}" ]]; then
    mkdir -p "$VERIFY_EVIDENCE"
  fi
}

cat > "$VERIFY_EVIDENCE/run.md" <<EOF
# verify-summa run ${VERIFY_RUN_ID}

- repo: ${VERIFY_REPO_ROOT}
- binary: ${SUMMA_BIN}
- hub: https://summa.duyet.net
- duckdb: ${DUCKDB_PATH}
- config: ${SUMMA_CONFIG}
- started_utc: $(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF
