#!/usr/bin/env bash
# CI / local checks for install, k8s, wrangler, and release wiring.
# No network except optional actionlint download (not used here).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
ok() { printf 'ok: %s\n' "$*"; }

[[ -f install.sh && -f run-import.sh ]] || fail "missing install.sh or run-import.sh"
bash -n install.sh
bash -n run-import.sh
bash -n apps/api/build.sh
ok "bash -n install.sh run-import.sh apps/api/build.sh"

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck -x install.sh run-import.sh apps/api/build.sh
  ok "shellcheck install.sh run-import.sh apps/api/build.sh"
else
  echo "skip: shellcheck not installed"
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/summa-ci-deploy.XXXXXX")"
http_pid=""
cleanup() {
  if [ -n "${http_pid:-}" ]; then
    kill "$http_pid" >/dev/null 2>&1 || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

HOME="$tmp/home"
mkdir -p "$HOME"
export HOME
install_dir="$tmp/bin"
out="$(
  SUMMA_DRY_RUN=1 \
    SUMMA_VERSION=v0.1.1 \
    SUMMA_INSTALL_DIR="$install_dir" \
    bash install.sh
)"
printf '%s\n' "$out"
echo "$out" | grep -q 'dry-run: would download' || fail "install.sh dry-run missing marker"
echo "$out" | grep -q 'releases/download/v0.1.1/summa-' || fail "install.sh dry-run missing release URL"
echo "$out" | grep -q '.tar.gz.sha256' || fail "install.sh dry-run missing checksum URL"
echo "$out" | grep -Eq 'target +: +(x86_64|aarch64)-(unknown-linux-gnu|apple-darwin)' \
  || fail "install.sh dry-run missing platform target"
[[ -d "$install_dir" ]] || fail "install.sh dry-run did not create install dir"
ok "install.sh SUMMA_DRY_RUN=1"

# curl | bash against a local tarball (same layout as GitHub Releases).
www="$tmp/www"
target="$(printf '%s\n' "$out" | sed -n 's/.*target *: *//p' | awk 'NR==1 { print }')"
[ -n "$target" ] || fail "could not parse target from dry-run"
asset="summa-${target}"
mkdir -p "$www/beta/${asset}"
printf '#!/bin/sh\necho summa 0.0.0-ci\n' > "$www/beta/${asset}/summa"
chmod +x "$www/beta/${asset}/summa"
tar -C "$www/beta" -czf "$www/beta/${asset}.tar.gz" "$asset"
# CI writes GNU `shasum -a 256` sidecars next to the archive.
(
  cd "$www/beta"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${asset}.tar.gz" > "${asset}.tar.gz.sha256"
  else
    sha256sum "${asset}.tar.gz" > "${asset}.tar.gz.sha256"
  fi
)
mkdir -p "$www/mismatch/beta" "$www/nochecksum/beta" "$www/extra/beta"
cp "$www/beta/${asset}.tar.gz" "$www/mismatch/beta/"
cp "$www/beta/${asset}.tar.gz" "$www/nochecksum/beta/"
cp "$www/beta/${asset}.tar.gz" "$www/extra/beta/"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "${asset}.tar.gz" \
  > "$www/mismatch/beta/${asset}.tar.gz.sha256"
{
  cat "$www/beta/${asset}.tar.gz.sha256"
  printf '%s  %s\n' "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" "other.tar.gz"
} > "$www/extra/beta/${asset}.tar.gz.sha256"
cp install.sh "$www/install.sh"
portfile="$tmp/http.port"
python3 - "$www" "$portfile" <<'PY' &
import http.server, os, socketserver, sys, time
www, portfile = sys.argv[1], sys.argv[2]
os.chdir(www)
httpd = socketserver.TCPServer(("127.0.0.1", 0), http.server.SimpleHTTPRequestHandler)
with open(portfile, "w", encoding="utf-8") as f:
    f.write(str(httpd.server_address[1]))
httpd.serve_forever()
PY
http_pid=$!
for _ in $(seq 1 50); do
  [ -s "$portfile" ] && break
  sleep 0.05
done
[ -s "$portfile" ] || fail "http.server did not bind"
port="$(cat "$portfile")"
curl_bin="$tmp/curl-bin"
mkdir -p "$curl_bin" "$tmp/curl-home"
info_curl="$(
  curl -fsSL "http://127.0.0.1:${port}/install.sh" \
    | env \
      HOME="$tmp/curl-home" \
      SUMMA_DOWNLOAD_BASE="http://127.0.0.1:${port}" \
      SUMMA_VERSION=beta \
      SUMMA_INSTALL_DIR="$curl_bin" \
      bash
)"
printf '%s\n' "$info_curl"
grep -q 'channel beta written' <<<"$info_curl" || fail "install.sh did not record update channel"
grep -q 'checksum ok' <<<"$info_curl" || fail "install.sh did not verify sha256"
[[ -x "$curl_bin/summa" ]] || fail "curl | bash did not install an executable"
got="$("$curl_bin/summa")"
[[ "$got" == "summa 0.0.0-ci" ]] || fail "installed stub printed: $got"
ok "curl | bash install.sh"

run_install() {
  local prefix="$1" dest="$2"
  env \
    HOME="$tmp/home-${prefix}" \
    SUMMA_DOWNLOAD_BASE="http://127.0.0.1:${port}/${prefix}" \
    SUMMA_VERSION=beta \
    SUMMA_INSTALL_DIR="$dest" \
    bash install.sh
}

mismatch_bin="$tmp/mismatch-bin"
mkdir -p "$mismatch_bin" "$tmp/home-mismatch"
mismatch_out=""
mismatch_rc=0
if mismatch_out="$(run_install mismatch "$mismatch_bin" 2>&1)"; then
  mismatch_rc=0
else
  mismatch_rc=$?
fi
printf '%s\n' "$mismatch_out"
[ "$mismatch_rc" -ne 0 ] || fail "install.sh accepted a mismatched checksum"
grep -q 'checksum mismatch' <<<"$mismatch_out" || fail "mismatch error missing 'checksum mismatch'"
[[ ! -x "$mismatch_bin/summa" ]] || fail "mismatch install left a binary"
ok "install.sh checksum mismatch is fatal"

nochecksum_bin="$tmp/nochecksum-bin"
mkdir -p "$nochecksum_bin" "$tmp/home-nochecksum"
nocheck_out=""
nocheck_rc=0
if nocheck_out="$(run_install nochecksum "$nochecksum_bin" 2>&1)"; then
  nocheck_rc=0
else
  nocheck_rc=$?
fi
printf '%s\n' "$nocheck_out"
[ "$nocheck_rc" -ne 0 ] || fail "install.sh accepted an archive with no checksum"
grep -q 'checksum not found' <<<"$nocheck_out" || fail "missing-sidecar error missing 'checksum not found'"
[[ ! -x "$nochecksum_bin/summa" ]] || fail "missing-checksum install left a binary"
ok "install.sh missing checksum is fatal"

extra_bin="$tmp/extra-bin"
mkdir -p "$extra_bin" "$tmp/home-extra"
extra_out=""
extra_rc=0
if extra_out="$(run_install extra "$extra_bin" 2>&1)"; then
  extra_rc=0
else
  extra_rc=$?
fi
printf '%s\n' "$extra_out"
[ "$extra_rc" -ne 0 ] || fail "install.sh accepted a sidecar with extra records"
grep -q 'exactly one checksum record' <<<"$extra_out" || fail "extra-record error missing 'exactly one checksum record'"
[[ ! -x "$extra_bin/summa" ]] || fail "extra-record install left a binary"
ok "install.sh extra checksum records are fatal"

kill "$http_pid" >/dev/null 2>&1 || true
wait "$http_pid" >/dev/null 2>&1 || true

python3 - <<'PY'
from pathlib import Path
import json, re, sys

sidecar = Path("deploy/k8s/summa-sidecar.yaml").read_text()
docs = [d.strip() for d in re.split(r"^---\s*$", sidecar, flags=re.M) if d.strip()]
if len(docs) < 2:
    sys.exit("summa-sidecar.yaml expected Secret + CronJob")
if not re.search(r"kind:\s*Secret", sidecar):
    sys.exit("sidecar missing Secret")
if not re.search(r"kind:\s*CronJob", sidecar):
    sys.exit("sidecar missing CronJob")
if "SUMMA_TELEMETRY_ENDPOINT" not in sidecar:
    sys.exit("sidecar missing telemetry endpoint")
if '["import"' not in sidecar and "- import" not in sidecar:
    sys.exit("CronJob must run summa import, not serve")
live = "\n".join(l for l in sidecar.splitlines() if not l.strip().startswith("#"))
if re.search(r"\bserve\b", live):
    sys.exit("live sidecar spec must not invoke summa serve")
values = Path("deploy/k8s/hermes-values-summa.yaml").read_text()
if "extraContainers:" not in values:
    sys.exit("hermes values missing extraContainers")
if not re.search(r"name:\s*summa-import", values):
    sys.exit("hermes sidecar name")
if "summa import" not in values:
    sys.exit("hermes sidecar must run import")
print("ok: k8s manifests")

wrangler = Path("apps/api/wrangler.jsonc").read_text()
stripped = re.sub(r"//.*?$", "", wrangler, flags=re.M)
stripped = re.sub(r",(\s*[}\]])", r"\1", stripped)
cfg = json.loads(stripped)
assert cfg.get("name") == "summa-telemetry", cfg.get("name")
assert cfg.get("main") == "build/worker/shim.mjs", cfg.get("main")
build_cmd = cfg.get("build", {}).get("command", "")
assert "build.sh" in build_cmd, build_cmd
build_sh = Path("apps/api/build.sh").read_text()
if "worker-build --version 0.8.5" not in build_sh:
    sys.exit("apps/api/build.sh must pin worker-build 0.8.5")
if "reference-types" not in build_sh:
    sys.exit("apps/api/build.sh must set wasm reference-types for worker-build 0.8")
if "--no-panic-recovery" not in build_sh:
    sys.exit("apps/api/build.sh must pass --no-panic-recovery (wasm-bindgen abort-handler/externref)")
routes = cfg.get("routes") or []
assert any(r.get("pattern") == "summa.duyet.net" for r in routes), routes
d1 = cfg.get("d1_databases") or []
assert d1 and d1[0].get("binding") == "DB", d1
mig = Path(d1[0].get("migrations_dir") or "migrations")
mig_dir = Path("apps/api") / mig
files = sorted(p.name for p in mig_dir.glob("*.sql"))
assert "0001_init.sql" in files, files
assert "0003_drop_events.sql" in files, files
drop = (mig_dir / "0003_drop_events.sql").read_text()
assert "DROP TABLE IF EXISTS events" in drop, drop
print("ok: wrangler.jsonc + D1 migrations")

rp = json.loads(Path("release-please-config.json").read_text())
packages = rp.get("packages") or {}
if "apps/cli" not in packages:
    sys.exit("release-please-config.json must package apps/cli (workspace member)")
cli = packages["apps/cli"]
assert cli.get("release-type") == "rust", cli
assert cli.get("package-name") == "summa-import", cli
plugins = rp.get("plugins") or []
if not any((p.get("type") if isinstance(p, dict) else p) == "cargo-workspace" for p in plugins):
    sys.exit("release-please-config.json needs cargo-workspace plugin")
manifest = json.loads(Path(".release-please-manifest.json").read_text())
if "apps/cli" not in manifest:
    sys.exit(".release-please-manifest.json must key apps/cli")
if "." in manifest:
    sys.exit(".release-please-manifest.json must not use '.' after the workspace move")
print("ok: release-please workspace package")
PY

rel="$(cat .github/workflows/release.yml)"
echo "$rel" | grep -q 'cargo publish' || fail "release.yml missing cargo publish"
echo "$rel" | grep -qE 'cargo publish .* -p summa-import|-p summa-import .* cargo publish|publish --locked -p summa-import' \
  || fail "release.yml cargo publish must use -p summa-import"
echo "$rel" | grep -q 'cargo package' || fail "release.yml missing cargo package"
echo "$rel" | grep -qE 'package .* -p summa-import|-p summa-import' \
  || fail "release.yml cargo package must use -p summa-import"
echo "$rel" | grep -q -- '--bin summa' || fail "release.yml must build --bin summa"
echo "$rel" | grep -q 'tag_name: beta' || fail "release.yml must publish rolling beta channel binaries"
echo "$rel" | grep -q '.tar.gz.sha256' || fail "release.yml must publish .sha256 sidecars"

ci="$(cat .github/workflows/ci.yml)"
echo "$ci" | grep -qE 'cargo test .* -p summa-import' || fail "ci.yml cargo test must use -p summa-import"
echo "$ci" | grep -q 'summa-api' || fail "ci.yml missing API wasm job"
echo "$ci" | grep -q 'validate-deploy.sh' || fail "ci.yml must run scripts/ci/validate-deploy.sh"
echo "$ci" | grep -q 'cargo package' || fail "ci.yml must cargo package -p summa-import"
ok "CI/release workflows pin -p summa-import"

[[ -f apps/cli/README.md ]] || fail "apps/cli/README.md required for cargo package"
ok "apps/cli README present for crates.io"

python3 - <<'PY'
import re
import subprocess
import sys
from pathlib import Path

def ignored(path: str) -> bool:
    return subprocess.run(
        ["git", "check-ignore", "--no-index", "-q", path]
    ).returncode == 0

must_ignore = [
    ".env",
    ".env.bak",
    ".env.local",
    ".env.production",
    ".env.backup",
    ".dev.vars.bak",
    "apps/api/.dev.vars.bak",
    "credentials.toml",
    "credentials.toml.bak",
    "summa.credentials.toml",
    "apps/other/examples/credentials.toml",
]
must_not_ignore = [
    ".env.example",
    "apps/api/.dev.vars.example",
    "apps/cli/examples/credentials.toml",
]
failed = False
for path in must_ignore:
    if not ignored(path):
        print(f"FAIL: {path} must be gitignored", file=sys.stderr)
        failed = True
for path in must_not_ignore:
    if ignored(path):
        print(f"FAIL: {path} must remain trackable", file=sys.stderr)
        failed = True

tracked = subprocess.check_output(["git", "ls-files"], text=True).splitlines()

def forbidden_tracked(path: str) -> bool:
    name = path.rsplit("/", 1)[-1]
    known_template = path == "apps/cli/examples/credentials.toml"
    if name == ".env":
        return True
    if name.startswith(".env.") and name != ".env.example":
        return True
    if name == ".dev.vars":
        return True
    if name.startswith(".dev.vars.") and name != ".dev.vars.example":
        return True
    if name.endswith(".bak"):
        return True
    if name in {"credentials.toml", "summa.credentials.toml"} and not known_template:
        return True
    return False

for path in tracked:
    if forbidden_tracked(path):
        print(f"FAIL: tracked secret-shaped path {path}", file=sys.stderr)
        failed = True

example = Path("apps/cli/examples/credentials.toml").read_text()
if re.search(r"keep out of git", example, re.I):
    print("FAIL: example credentials.toml must not say to keep the template out of git", file=sys.stderr)
    failed = True
placeholder = re.compile(
    r"(?i)^(replace-me|change-me|changeme|change_me|.*[=]replace-me)$"
)
for i, line in enumerate(example.splitlines(), 1):
    s = line.strip()
    if not s or s.startswith("#") or "=" not in s:
        continue
    key, _, raw = s.partition("=")
    val = raw.strip().strip('"').strip("'")
    if not placeholder.match(val):
        print(
            f"FAIL: apps/cli/examples/credentials.toml:{i} key {key.strip()} is not a placeholder",
            file=sys.stderr,
        )
        failed = True

if failed:
    sys.exit(1)
print("ok: gitignore covers env/credential backups; example stays a placeholder template")
PY
