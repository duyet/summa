# CLI install and update (checksum path)

Users install a CI-built binary via `install.sh` (curl|bash) or upgrade in place with `summa update`. Checksum skip lives in **`summa update`**: SHA-256 of the on-disk binary vs `install-state.json` / incoming artifact (`should_install`). `install.sh` itself does **not** verify a `.tar.gz.sha256` file (CI still publishes one next to the tarball).

## Sub-features

- `install.sh`: detect `x86_64|aarch64` + `linux-gnu|apple-darwin`, download `summa-<target>.tar.gz` from GitHub Releases (`beta` or stable tag)
- `SUMMA_DRY_RUN=1`: print URL, `mkdir` install dir, do not download
- `SUMMA_INSTALL_DIR` / `SUMMA_VERSION` / `SUMMA_CHANNEL` / `SUMMA_DOWNLOAD_BASE`
- `summa update` / `--dry-run` / `--beta` / `--stable`
- `should_install(current_sha, incoming_sha)`: empty incoming → no; same hash (case-insensitive) → skip; different → install
- State file: `~/.local/share/summa/install-state.json` (`sha256`, `run_id`, `artifact_name`, `channel`)
- CI: `bash scripts/ci/validate-deploy.sh` dry-runs install.sh and curl|bash against a local tarball

## How to get to it (user POV)

```bash
curl -fsSL https://summa.duyet.net/install.sh | bash
curl -fsSL https://summa.duyet.net/install.sh | SUMMA_DRY_RUN=1 bash
summa update
summa update --dry-run
summa update --beta|--stable
```

Env vars must be on **bash** (right-hand side of the pipe), not on curl. Docs: `docs/install.md`, root `README.md`.

## Driving it with install.sh and CLI subprocess

Existing tests first:

```bash
bash scripts/ci/validate-deploy.sh
cargo test --locked -p summa-import --lib script::update::tests -- --test-threads=1
```

`validate-deploy.sh` is the user-shaped installer path (dry-run + local HTTP tarball). Update unit tests cover `sha256_hex_known_vector`, `should_install_same_hash_is_noop`, `should_install_empty_incoming_is_false`.

Isolated dry-run (no GitHub download required if `SUMMA_VERSION` is set — still prints the URL):

```bash
source .cursor/skills/verify-summa/helpers/isolated-env.sh
.cursor/skills/verify-summa/helpers/capture.sh install-dry-run -- \
  env SUMMA_DRY_RUN=1 SUMMA_VERSION=v0.1.1 \
    SUMMA_INSTALL_DIR="$SUMMA_INSTALL_DIR" \
    bash "$VERIFY_REPO_ROOT/install.sh"
test ! -x "$SUMMA_INSTALL_DIR/summa"
```

Pass: exit 0, stdout contains `dry-run: would download` and `releases/download/v0.1.1/summa-`. Install dir exists (mkdir); **no** `summa` binary inside.

Isolated update dry-run (network: GitHub API; no binary replace):

```bash
.cursor/skills/verify-summa/helpers/capture.sh update-dry-run -- \
  env SUMMA_INSTALL_DIR="$SUMMA_INSTALL_DIR" \
    "$SUMMA_BIN" update --dry-run
```

Pass: exit 0, stdout contains `dry-run: would install summa-` **or** `update: already current` with a `sha` prefix. Copy the printed sha prefix as observed; do not invent hashes.

Do not run a live `summa update` (non-dry-run) against `~/.local/bin/summa` from this skill. Do not `cargo build --release`.

## Gotchas

- **`install.sh` does not checksum the tarball.** The checksum path to prove is `script::update::should_install` + `summa update` skip/`already current` lines. Observing CI's `.tar.gz.sha256` file is not what the installer reads.
- `summa update --dry-run --beta` (or `--stable`) persists `update.channel` **before** the dry-run return. Drive `--dry-run` without channel flags, or only inside the isolated `HOME`.
- Every CLI command spawns `auto_update_tick`. Isolated config sets `update.mode=manual` so it no-ops.
- `SUMMA_DRY_RUN=1` still creates `SUMMA_INSTALL_DIR`. Assert “no binary”, not “no directory”.
- `sync_repo_release_copy` may write `target/release/summa` if that directory already exists in the repo cwd. Isolated update dry-run does not reach that path; a live update from the repo root could. Do not create `target/release` just to verify.
- `SUMMA_SETUP_CRON=1` registers a real user cron/systemd job. Never set it in verification.
