# vdl

A small Linux application for recurring `yt-dlp` downloads, with a mouse-enabled
Rich terminal interface and a separate mobile-first web interface. Source
folders and marker files remain the persistent state: no database, broker,
external queue, frontend framework, or JavaScript build step is required.

## Install

Requires Python 3.12+, `uv`, `tmux`, `yt-dlp`, and a systemd user session.

```console
uv tool install .
vdl install
```

Installation creates configuration if absent and starts `vdl.service` (the
terminal and single scheduler in a dedicated tmux server) and
`vdl-web.service` (HTTP only). Neither service requires root.

For an existing installation, let the current download finish before restarting:

```console
git fetch origin
git switch work/rich-mobile-web
uv tool install --force .
vdl install
systemctl --user restart vdl vdl-web
```

Existing configuration, downloaded media, archives, and markers are preserved.
The previous Textual-based lockfile is removed in this branch rather than left
inconsistent with the new dependencies. Run `uv lock` with registry access to
create a new development lockfile; `uv tool install .` does not require one.

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
yt_dlp_command = ["yt-dlp", "--download-archive", ".archive"]
```

Durations accept `s`, `m`, `h`, or `d`. The command is executed directly, with
its source URL appended and the source folder as the working directory. Put
cookies, format selection, retries, and output templates in `yt_dlp_command`.
Configuration changes require restarting the services.

`scheduler_poll` remains accepted for compatibility. Idle discovery is capped
at one second so CLI and browser additions are noticed promptly. This does not
change `check_interval` or `minimum_spacing` for recurring checks.

## CLI and terminal

```console
vdl                       # run the owner terminal application
vdl attach                # attach to the persistent owner in tmux
vdl list
vdl add https://www.youtube.com/@example
vdl del 2,4 7
```

The CLI commands and temporary list numbering retain their existing semantics.
`del` accepts whitespace, commas, and semicolons, lists the sources selected for
disabling, and asks for confirmation. It never deletes media or archives.

Rich renders the terminal table; `prompt_toolkit` handles input, mouse events,
focus, and terminal cleanup. Only one renderer owns the terminal. Click rows to
select them and click buttons to add, queue, disable, or detach. Mouse wheel
scrolling and keyboard navigation are supported. Add and Disable remain usable
while another source is downloading.

| Control | Action |
| --- | --- |
| Enter in the URL field | Add the displayed source |
| Tab / Shift-Tab | Move focus |
| Arrow / Page keys in the table | Select a source |
| F2 | Toggle mouse capture to allow native text selection |
| F6 / Detach | Detach the tmux client without stopping downloads |
| Ctrl-Q / Ctrl-C / Quit | Exit the owner application |
| `a`, `n`, `d`, `r`, `q` outside the URL field | Add, download now, disable, refresh, quit |

Use `vdl --no-mouse` or `VDL_MOUSE=0 vdl` when a terminal has incompatible mouse
reporting. The normal `Ctrl-b d` tmux shortcut also detaches. Exiting the owner
ends its current downloader; systemd restarts the owner after about ten seconds.
An explicit `systemctl --user stop vdl` remains stopped.

## Web and phone workflow

Open `http://HOSTNAME:8780` on a trusted local network. The page is ordinary
HTML, CSS, and JavaScript, not a terminal streamed into a browser. The URL form
stays at the top while the source list scrolls. Small screens use cards, native
text input, and large buttons without horizontal page scrolling.

1. Paste a profile/channel URL, or press **Paste from clipboard**.
2. Review the cleaned URL displayed directly in the form.
3. Press **Add source**. The source appears immediately, even during a download.

The first `?` and everything following it are removed, matching the existing
application rule. This is intended for profile/channel URLs, not links such as
`watch?v=...` whose identity is encoded in query parameters. The server repeats
normalization and validates the URL; the JavaScript preview is not trusted.

Browsers generally allow programmatic clipboard reading only in a secure
context (HTTPS or localhost), after a user action and any required permission.
Plain HTTP on a phone's LAN connection therefore falls back to focusing the
normal input: long-press and paste. Denied permission uses the same fallback.
There is no automatic submission and the application never clears the system
clipboard. It cleans the pasted URL, not the clipboard itself.

The visible page fetches state roughly once per second, without overlapping
polls or replacing focused controls. Other tabs and CLI changes appear without
manual refresh. Polling pauses while hidden and resumes when the page returns.
An offline indicator replaces the live indicator when a request fails; the
last known list and the current draft are retained.

The web process never owns a scheduler or launches `yt-dlp`. If the owner is
stopped, the page reports that fact and can still save sources for later.

## Scheduling and filesystem state

An active source has `SERVICE/ACCOUNT/.source`; an inactive one has
`.source.del`. The marker contains its URL. `.archive` belongs to `yt-dlp`.
The mtime of `.last-check` records when an attempt started, not successful
completion. A `.download-now` marker requests an immediate check.

The scheduler chooses manual requests first, then sources never checked, then
the oldest overdue recurring check. Manual requests and new sources use their
existing marker mtimes for ordering, with a stable name tie-break. They bypass
ordinary interval/spacing delays. Repeated requests do not create duplicate
queue entries or reset the pending request's timestamp. After a download ends,
the scheduler immediately checks for the next eligible source.

Exactly one downloader runs for a user runtime at a time. A process-wide file
lock complements the scheduler's in-process lock. The downloader inherits its
lock descriptor so an owner crash cannot immediately start another download
while the old child is still alive. Short source mutations have their own
filesystem lock; it is never held for the duration of a download.

Disabling a waiting source removes its pending request. Disabling a currently
running source lets that attempt finish and prevents future attempts; the UI
shows `finishing`. There is no implicit cancellation or media deletion. Source
folders retain their original names even if marker URLs are edited by hand.

## HTTP API and security

The HTTP process serves only packaged assets and these JSON endpoints:

- `GET /api/state`
- `POST /api/sources` with `{"url": "https://example.test/account"}`
- `POST /api/sources/disable` with `{"id": "service/account"}`
- `POST /api/sources/download-now` with the same ID payload

Mutations require `Content-Type: application/json` and `X-VDL-Request: 1`.
Cross-origin browser mutations are rejected; there is no CORS opt-in. The API
uses stable IDs, not list positions or client-supplied filesystem paths.

This remains a **trusted-LAN application without authentication**. Do not
expose the listening port directly to the Internet. Use a private VPN or an
authenticated HTTPS reverse proxy for access outside a trusted network. HTTPS
also enables the browser clipboard button where permission is granted.

## Logs, tests, and removal

```console
tail -f /tmp/vdl/vdl.log
systemctl --user status vdl vdl-web
uv run --group dev pytest
node --test tests/test_web.mjs
```

Browser tests are optional. Install the `browser` dependency group and Chromium,
then follow [the validation notes](docs/validation.md). They explain the normal
network test and the restricted-environment DOM harness used during this change.

```console
vdl deinstall
```

Removal stops the services and deletes their unit files only. Configuration,
source folders, media, and archives remain intact.
