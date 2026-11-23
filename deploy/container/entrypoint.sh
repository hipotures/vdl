#!/usr/bin/env sh
set -eu

HOME_DIR="${VDL_HOME:-/home/app}"

if [ -L "$HOME_DIR" ]; then
    echo "vdl-entrypoint: refusing to start: $HOME_DIR is a symlink" >&2
    exit 1
fi

if [ "${VDL_ALLOW_UNMOUNTED_HOME:-0}" != "1" ]; then
    if ! grep -q " $HOME_DIR " /proc/mounts 2>/dev/null; then
        echo "vdl-entrypoint: refusing to start: $HOME_DIR is not a mounted volume (set VDL_ALLOW_UNMOUNTED_HOME=1 for throwaway tests)" >&2
        exit 1
    fi
fi

mkdir -p "$HOME_DIR/config" "$HOME_DIR/downloads" "$HOME_DIR/logs"

CONFIG_FILE="$HOME_DIR/config/config.toml"
if [ ! -f "$CONFIG_FILE" ]; then
    cat > "$CONFIG_FILE" <<EOF
download_root = "$HOME_DIR/downloads"

check_interval = "24h"
minimum_spacing = "10m"
scheduler_poll = "1m"

log_file = "$HOME_DIR/logs/vdl.log"

web_host = "0.0.0.0"
web_port = 8780

yt_dlp_command = [
    "yt-dlp",
    "--download-archive", ".archive"
]
EOF
fi

exec /usr/local/bin/vdl serve --config "$CONFIG_FILE"
