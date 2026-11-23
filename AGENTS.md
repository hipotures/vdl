# AGENTS.md

## Architecture: Client–Server Split

`vdl` is a pure Rust client–server application:

- **Server (`vdl serve [--config PATH]`)**: single headless process that owns the
  filesystem repository (`download_root`), the `yt-dlp` scheduler, the HTTP JSON
  API, the embedded mobile web UI (`static/`), and HomeStack health/telemetry
  endpoints (`/health/live`, `/health/ready`, `/.well-known/homestack/status`).
- **Clients (`vdl` / `vdl tui`, `vdl list`, `vdl add`, `vdl del`, `vdl now`)**:
  talk to the server exclusively over HTTP (`--server URL`, `VDL_SERVER`, or
  `server_url` in `~/.config/vdl/config.toml`; default `http://127.0.0.1:8780`).
  Clients never access the server's download directory or lock files directly.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
node --test tests/test_web.mjs
```

## Container Image and Releases (HomeStack)

- Containerfile: `deploy/container/Containerfile` (runtime contract: `deploy/container/README.md`)
- HomeStack manifest (schema 3): `.homestack/services/vdl/{service,defaults,inputs}.yaml`
- Release rule: after committing and pushing a completed change, decide whether
  the server image changes (application code, `Cargo.toml`/`Cargo.lock`,
  `static/`, `deploy/container/`). Only then run `./scripts/tag-image.sh` to
  publish `ghcr.io/hipotures/vdl:X.Y.Z`. Changes only to manifests, docs, tests,
  or CI need no image release. Never move or delete release tags.
