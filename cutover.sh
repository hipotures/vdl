#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== vdl Rust Cutover ==="
echo "Repository: ${REPO_ROOT}"
echo

echo "Step 1: Build and install the release Rust binary (vdl) via cargo."
echo "Command: cargo install --path \"${REPO_ROOT}\" --force"
read -r -p "Press Enter to run (or Ctrl-C to abort)..."
cargo install --path "${REPO_ROOT}" --force

echo
echo "Step 2: Verify the installed binary."
which vdl
vdl --help | head -n 3
read -r -p "Press Enter to continue..."

echo
echo "Step 3: Update systemd user units and restart vdl.service and vdl-web.service."
echo "Note: Existing config (~/.config/vdl/config.toml), downloads, archives, and markers are preserved."
echo "Command: vdl install && systemctl --user restart vdl vdl-web"
read -r -p "Press Enter to run (or Ctrl-C to abort)..."
vdl install
systemctl --user restart vdl vdl-web

echo
echo "Step 4: Check service status."
systemctl --user --no-pager status vdl vdl-web || true

echo
echo "Cutover complete. To roll back to the previous Python version from git history:"
echo "  git checkout <previous-commit> && uv tool install --force . && vdl install && systemctl --user restart vdl vdl-web"
