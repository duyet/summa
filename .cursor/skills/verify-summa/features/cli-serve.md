# CLI serve

`summa serve` no longer binds a local HTTP server. It prints that the cloud hub replaced local serve, then `GET {telemetry.endpoint}/health` and prints `hub <url> <status>` plus the body.

## Sub-features

- Ping `https://summa.duyet.net/health` (or `[telemetry] endpoint` / `SUMMA_TELEMETRY_ENDPOINT`)
- Print hub JSON body (`ok`, `service`, `version`)
- `--bind` is accepted by clap and **ignored** (`let _ = args.bind`)
- `--config` loads endpoint override
- Legacy axum router in `apps/cli/src/script/serve.rs` is unit-tested only (local ingest/health contract); `run()` does not listen

## How to get to it (user POV)

```bash
summa serve
summa serve --bind 0.0.0.0:8787   # bind is ignored; still pings the hub
```

Docs: `docs/telemetry.md` — “`summa serve` only pings this hub”.

## Driving it with CLI subprocess

Existing tests (in-process axum; no live hub):

```bash
cargo test --locked -p summa-import --lib script::serve::tests -- --test-threads=1
```

Those tests prove `/health` is public and unauth `POST /v1/ingest` is 401 on the leftover router. They are not the user path.

User path:

```bash
source .cursor/skills/verify-summa/helpers/isolated-env.sh
.cursor/skills/verify-summa/helpers/capture.sh serve-ping -- \
  "$SUMMA_BIN" serve --config "$SUMMA_CONFIG"
```

Pass: exit 0, stdout contains `hub https://summa.duyet.net/health 200 OK` (status wording may be `200`), body includes `"service":"summa"` (whitespace may vary). stderr may say `summa serve is replaced by the cloud hub`.

If the hub is unreachable, stderr has `hub unreachable:` and the command still exits 0. Treat that as **not verified** for this feature (doctor `/health` should have failed first).

## Gotchas

- Do not expect a port to open. Scanning `8787` / `localhost` is the wrong surface.
- `--bind` in tests (`parse_serve_bind`) only checks clap parsing.
- Serve uses `reqwest` to the public hub; it does not send the telemetry token on `/health`.
- Isolated config must keep `endpoint = "https://summa.duyet.net"` unless intentionally pointing at another hub. Do not point at a user's private URL from memory.
