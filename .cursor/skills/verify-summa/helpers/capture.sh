#!/usr/bin/env bash
# Capture a command's stdout/stderr/exit into $VERIFY_EVIDENCE/actions/<slug>/.
# Usage: capture.sh <slug> -- <command> [args…]
set -euo pipefail

if [[ $# -lt 3 || "$2" != "--" ]]; then
  echo "usage: $0 <slug> -- <command> [args…]" >&2
  exit 2
fi

slug="$1"
shift 2

if [[ -z "${VERIFY_EVIDENCE:-}" ]]; then
  echo "error: VERIFY_EVIDENCE is unset; source isolated-env.sh first" >&2
  exit 2
fi

dir="${VERIFY_EVIDENCE}/actions/${slug}"
mkdir -p "$dir"
printf '%s\n' "$*" > "$dir/cmd.txt"

set +e
"$@" >"$dir/stdout.txt" 2>"$dir/stderr.txt"
code=$?
set -e

http=""
if grep -qE '^HTTP/' "$dir/stdout.txt" 2>/dev/null; then
  http="$(awk 'toupper($1) ~ /^HTTP\// { print $2; exit }' "$dir/stdout.txt" || true)"
fi

python3 - "$dir/meta.json" "$code" "$http" "$slug" <<'PY'
import json, sys, os
path, code, http, slug = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
meta = {
    "slug": slug,
    "exit_code": code,
    "http_status": int(http) if http.isdigit() else None,
}
with open(path, "w", encoding="utf-8") as f:
    json.dump(meta, f, indent=2)
    f.write("\n")
PY

helper_dir="$(cd "$(dirname "$0")" && pwd)"
"$helper_dir/redact.sh" "$dir/stdout.txt" "$dir/stderr.txt" "$dir/cmd.txt"

exit "$code"
