# vdl

A small Linux client–server application written in Rust for recurring `yt-dlp`
downloads, with a mouse-enabled terminal client (`ratatui` + `crossterm`) and an
embedded mobile-first web interface (`axum`). Source folders and marker files
remain the persistent state: no database, broker, external queue, frontend
framework, or JavaScript build step is required.

## Architecture

- **`vdl serve [--config PATH]`**: single headless server owning `download_root`,
  the `yt-dlp` scheduler, the HTTP JSON API, the embedded web UI, and HomeStack
  health/telemetry endpoints (`/health/live`, `/health/ready`,
  `/.well-known/homestack/status`).
- **Clients (`vdl` / `vdl tui`, `vdl list`, `vdl add`, `vdl del`, `vdl now`)**:
  talk to the server exclusively over HTTP (`--server URL`, `VDL_SERVER`, or
  `server_url` in `~/.config/vdl/config.toml`; default `http://127.0.0.1:8780`).
  No `tmux`, no dual services, and no cross-process lock files are required.

## Install

Requires Rust (`cargo`), `yt-dlp`, and a systemd user session (or deploy via
HomeStack using `.homestack/services/vdl/`).

```console
cargo install --path .
vdl install
```

`vdl install` creates `~/.config/vdl/config.toml` if absent and enables the
single `vdl.service` (`vdl serve`).

## Configuration

Edit `~/.config/vdl/config.toml`:

```toml
download_root = "~/Downloads/vdl"
check_interval = "24h"
minimum_spacing = "10m"
scheduler_poll = "1m"
log_file = "/tmp/vdl/vdl.log"
web_host = "0.0.0.0"
web_port = 8780
# Optional for remote TUI/CLI clients:
# server_url = "https://vdl.homestack.app"
yt_dlp_command = ["yt-dlp", "--download-archive", ".archive"]
```

## CLI and TUI

```console
vdl                                     # open the TUI client (default)
vdl --server https://vdl.homestack.app  # connect TUI to a remote server
vdl list
vdl add https://www.youtube.com/@example
vdl now 1
vdl del 2,4 7
vdl serve                               # run the headless server directly
```

## HTTP API, Health, and Telemetry

- `GET /api/state`
- `GET /api/logs?lines=50`
- `POST /api/sources` (`{"url": "..."}`)
- `POST /api/sources/disable` (`{"id": "service/account"}`)
- `POST /api/sources/download-now` (`{"id": "service/account"}`)
- `GET /health/live` (`application/health+json`)
- `GET /health/ready` (`application/health+json`)
- `GET /.well-known/homestack/status` (HomeStack telemetry schema 3)

Mutations require `Content-Type: application/json` and `X-VDL-Request: 1`.

## Tests and Removal

```console
cargo test
node --test tests/test_web.mjs
vdl deinstall
```
