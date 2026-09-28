"""Install and remove vdl's systemd user services."""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
import shlex
import shutil
import subprocess
import sys

from .config import CONFIG_PATH, ensure_config, load_config


SYSTEMD_USER_DIR = Path("~/.config/systemd/user").expanduser()
TMUX_CONFIG_PATH = Path("~/.config/vdl/tmux.conf").expanduser()
OWNER_UNIT_NAME = "vdl.service"
WEB_UNIT_NAME = "vdl-web.service"


@dataclass(frozen=True, slots=True)
class InstallationPaths:
    config: Path
    owner_unit: Path
    web_unit: Path
    tmux_config: Path


Runner = Callable[..., subprocess.CompletedProcess[object]]


def _vdl_command(executable: Sequence[str] | None = None) -> list[str]:
    if executable is not None:
        return list(executable)
    invoked = Path(sys.argv[0])
    if invoked.name == "vdl" and invoked.exists():
        return [str(invoked.resolve())]
    installed = shutil.which("vdl")
    if installed:
        return [installed]
    return [str(Path(sys.executable).resolve()), "-m", "vdl"]


def _exec_line(arguments: Sequence[str]) -> str:
    return shlex.join(arguments)


def owner_unit_text(tmux_config: Path) -> str:
    tmux = shutil.which("tmux") or "/usr/bin/tmux"
    return f"""[Unit]
Description=vdl owner application
After=default.target

[Service]
Type=simple
ExecStart={_exec_line([tmux, '-L', 'vdl', '-f', str(tmux_config), '-D'])}
ExecStop={_exec_line([tmux, '-L', 'vdl', 'kill-server'])}
KillMode=control-group
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
"""


def web_unit_text(application_command: Sequence[str]) -> str:
    return f"""[Unit]
Description=vdl Textual web interface
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={_exec_line([*application_command, '--serve-web'])}
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
"""


def _write_tmux_config(path: Path, application_command: Sequence[str]) -> None:
    shell_command = shlex.join(["exec", *application_command])
    path.write_text(
        "set-option -g exit-empty on\n"
        "set-option -g exit-unattached off\n"
        f"new-session -d -s main {shlex.quote(shell_command)}\n"
        "set-option -g exit-empty on\n",
        encoding="utf-8",
    )


def _systemctl(
    arguments: Sequence[str], runner: Runner | None, *, check: bool = True
) -> subprocess.CompletedProcess[object]:
    return (runner or subprocess.run)(["systemctl", "--user", *arguments], check=check)


def install_services(
    *,
    config_path: Path = CONFIG_PATH,
    unit_dir: Path = SYSTEMD_USER_DIR,
    tmux_config: Path = TMUX_CONFIG_PATH,
    executable: Sequence[str] | None = None,
    runner: Runner | None = None,
) -> InstallationPaths:
    """Create configuration and units, then enable both user services."""

    config_path = ensure_config(config_path)
    config = load_config(config_path)
    application_command = _vdl_command(executable)
    unit_dir.mkdir(parents=True, exist_ok=True)
    tmux_config.parent.mkdir(parents=True, exist_ok=True)
    _write_tmux_config(tmux_config, application_command)

    owner = unit_dir / OWNER_UNIT_NAME
    web = unit_dir / WEB_UNIT_NAME
    owner.write_text(owner_unit_text(tmux_config), encoding="utf-8")
    web.write_text(web_unit_text(application_command), encoding="utf-8")

    _systemctl(["daemon-reload"], runner)
    _systemctl(["enable", "--now", OWNER_UNIT_NAME, WEB_UNIT_NAME], runner)

    display_host = "localhost" if config.web_host in {"0.0.0.0", "::"} else config.web_host
    print(f"Config: {config_path}")
    print(f"Owner status: systemctl --user status {OWNER_UNIT_NAME}")
    print(f"Web status: systemctl --user status {WEB_UNIT_NAME}")
    print("Attach: vdl attach")
    print(f"Web: http://{display_host}:{config.web_port}")
    return InstallationPaths(config_path, owner, web, tmux_config)


def deinstall_services(
    *, unit_dir: Path = SYSTEMD_USER_DIR, runner: Runner | None = None
) -> None:
    """Stop the services and remove only vdl's installed unit files."""

    _systemctl(["disable", "--now", OWNER_UNIT_NAME, WEB_UNIT_NAME], runner, check=False)
    for name in (OWNER_UNIT_NAME, WEB_UNIT_NAME):
        (unit_dir / name).unlink(missing_ok=True)
    _systemctl(["daemon-reload"], runner)
    print("Removed vdl user services; configuration and downloads were preserved.")
