# vdl

`vdl` is a small Linux application for recurring `yt-dlp` downloads. Its
Textual interface runs persistently in a dedicated tmux server, while an
optional browser view serves the same interface with Textual's supported web
driver. Source directories and their marker files are the only persistent
application state.

## Install

The application requires Python 3.12 or newer, `uv`, `tmux`, `yt-dlp`, and a
systemd user session. From a clone of this repository:

```console
$ uv tool install .
$ vdl install
```

`vdl install` creates `~/.config/vdl/config.toml` if it does not exist,
installs and starts `vdl.service` and `vdl-web.service` under
`~/.config/systemd/user/`, and prints the relevant status commands. It does
not need root access.

Edit the generated configuration to choose the download directory and the
exact `yt-dlp` argument vector:

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

Durations accept `s`, `m`, `h`, or `d`. The command is executed directly,
with the source URL appended and the source directory as its working
directory. Add cookies, format selection, retries, output templates, and other
download policy to `yt_dlp_command` itself. Restart the services after a
configuration change:

```console
$ systemctl --user restart vdl vdl-web
```

## Use

```console
$ vdl                       # run the owner Textual application
$ vdl attach                # attach to the persistent owner
$ vdl list
$ vdl add https://www.youtube.com/@example
$ vdl del 2,4 7
```

When a source is added, the first `?` and everything after it are removed from
the URL before it is stored.

Use the **Detach** button to leave tmux while the scheduler continues running.
`Ctrl-b d` does the same when the surrounding terminal passes tmux shortcuts.
Choosing Quit ends the tmux session, and systemd starts a fresh owner after
about ten seconds. An explicit `systemctl --user stop vdl` remains stopped.

The browser interface listens on `web_host:web_port`. With the default bind
address, open `http://HOSTNAME:8780` from the trusted local network, replacing
`HOSTNAME` with the machine's resolvable name or LAN address. Browser sessions
are client-only: they may list, add, disable, and queue a selected source with
**Download now**, but never start a second scheduler or downloader. The owner
scheduler processes queued downloads sequentially.

`vdl del` accepts whitespace, commas, semicolons, or mixtures of them. It
resolves the temporary numbers against the current sorted list, shows the
active sources it will disable, and asks once before changing their markers.

## Filesystem state

An active source has `SERVICE/ACCOUNT/.source`; an inactive one has
`.source.del`. The marker contains the source URL. `.archive` belongs to
`yt-dlp`, and the mtime of `.last-check` records when the latest attempt
started. A temporary `.download-now` marker queues an immediate check and is
removed when that attempt starts. Directories without either source marker are
ignored.

Disabling a source only renames `.source` to `.source.del`. Media, archives,
attempt markers, and account directories remain in place. The local directory
name is selected when a source is added and is never changed later when the URL
inside its marker is edited by hand.

Only one download runs at a time. Newly added sources go next after the active
job, while normal checks observe both the interval and minimum start spacing.
Complete downloader output and scheduler events are written to
`/tmp/vdl/vdl.log` by default:

```console
$ tail -f /tmp/vdl/vdl.log
```

To remove the user services while preserving configuration and all downloads:

```console
$ vdl deinstall
```
