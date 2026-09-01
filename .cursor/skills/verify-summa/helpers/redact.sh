#!/usr/bin/env bash
# Redact secrets in captured transcripts (in place).
# Usage: redact.sh <file> [<file>…]
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <file> [<file>…]" >&2
  exit 2
fi

python3 - "$@" <<'PY'
import pathlib, re, sys

patterns = [
    (re.compile(r"(?i)(authorization:\s*bearer\s+)\S+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(x-summa-token:\s*)\S+"), r"\1[REDACTED]"),
    (re.compile(r"summa_[A-Za-z0-9_\-]+"), "summa_[REDACTED]"),
    (re.compile(r"(?i)(telemetry_token\s*=\s*)[^\s#]+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(motherduck_token\s*=\s*)[^\s#]+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(clickhouse_password\s*=\s*)[^\s#]+"), r"\1[REDACTED]"),  # pragma: allowlist secret
    (re.compile(r"(?i)(CH_PASSWORD=)\S+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(MOTHERDUCK_TOKEN=)\S+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(SUMMA_TELEMETRY_TOKEN=)\S+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(CURSOR_SESSION=)\S+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(CURSOR_API_KEY=)\S+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(BOOTSTRAP_TOKEN=)\S+"), r"\1[REDACTED]"),
    (re.compile(r"ghp_[A-Za-z0-9]+"), "ghp_[REDACTED]"),
    (re.compile(r"github_pat_[A-Za-z0-9_]+"), "github_pat_[REDACTED]"),
]

for arg in sys.argv[1:]:
    p = pathlib.Path(arg)
    if not p.is_file():
        continue
    text = p.read_text(encoding="utf-8", errors="replace")
    for rx, repl in patterns:
        text = rx.sub(repl, text)
    p.write_text(text, encoding="utf-8")
PY
