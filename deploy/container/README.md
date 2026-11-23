# VDL Container Runtime Contract

## Base image
- Build stage: `docker.io/library/rust:1-slim-bookworm` (compiles the standalone `vdl` Rust binary with embedded web assets).
- Runtime stage: `docker.io/library/debian:bookworm-slim` with `ffmpeg`, `python3`, `ca-certificates`, `curl`, and the standalone `yt-dlp` binary.

## Ports & Endpoints
- `8780/tcp` (plain HTTP; Pangolin terminates public HTTPS and manages certificates):
  - Web UI: `/`, `/app.css`, `/app.js`, `/sort.js`, `/url.js`
  - JSON API: `GET /api/state`, `GET /api/logs`, `POST /api/sources`, `POST /api/sources/disable`, `POST /api/sources/download-now`
  - HomeStack health: `GET /health/live`, `GET /health/ready` (`application/health+json`, real loopback heartbeat)
  - HomeStack telemetry (schema 3): `GET /.well-known/homestack/status` (`application/json`, `VmRSS` from `/proc/self/status`)

## Persistence
Config and logs live under `/home/app` (HomeStack persistent volume `mp0`):
- `/home/app/config/config.toml` — created once on first boot if absent
- `/home/app/logs/vdl.log` — downloader and server log
- `/home/app/downloads/` — default `download_root` if `VDL_DOWNLOAD_ROOT` is not set; for external/shared media mounts, pass `VDL_DOWNLOAD_ROOT` via the HomeStack `download_root` input.

The entrypoint refuses to start if `/home/app` is a symlink or not a mounted volume unless `VDL_ALLOW_UNMOUNTED_HOME=1` is set for throwaway local tests.

## Environment Variables
- `VDL_HOME` (default `/home/app`)
- `VDL_HOST` (default `0.0.0.0`)
- `VDL_PORT` (default `8780`)
- `VDL_DOWNLOAD_ROOT` (optional override for `download_root`)
- `HOMESTACK_HEARTBEAT_SECONDS` (default `60`)
- `VDL_ALLOW_UNMOUNTED_HOME` (set `1` only for local throwaway container tests)
