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
shasum -a 256 "$www/beta/${asset}.tar.gz" > "$www/beta/${asset}.tar.gz.sha256"
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
kill "$http_pid" >/dev/null 2>&1 || true
wait "$http_pid" >/dev/null 2>&1 || true
[[ -x "$curl_bin/summa" ]] || fail "curl | bash did not install an executable"
got="$("$curl_bin/summa")"
[[ "$got" == "summa 0.0.0-ci" ]] || fail "installed stub printed: $got"
ok "curl | bash install.sh"

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

ci="$(cat .github/workflows/ci.yml)"
echo "$ci" | grep -qE 'cargo test .* -p summa-import' || fail "ci.yml cargo test must use -p summa-import"
echo "$ci" | grep -q 'summa-api' || fail "ci.yml missing API wasm job"
echo "$ci" | grep -q 'validate-deploy.sh' || fail "ci.yml must run scripts/ci/validate-deploy.sh"
echo "$ci" | grep -q 'cargo package' || fail "ci.yml must cargo package -p summa-import"
ok "CI/release workflows pin -p summa-import"

[[ -f apps/cli/README.md ]] || fail "apps/cli/README.md required for cargo package"
ok "apps/cli README present for crates.io"
