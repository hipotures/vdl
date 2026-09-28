"""Install and remove vdl's per-user services.

The installer deliberately only owns the two unit files it writes.  The
download tree and the user's configuration are independent of installation
and are never removed by :func:`deinstall_services`.
"""

from __future__ import annotations

import os
import shlex
import shutil
import subprocess
import sys
import tomllib
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

try:
    # The application configuration module owns the canonical default file
    # format.  Keep a local fallback so this module remains easy to inspect
    # and test in isolation.
    from .config import DEFAULT_CONFIG_TEXT as _CONFIG_DEFAULT_TEXT
    from .config import ensure_config as _config_ensure_config
except ImportError:  # pragma: no cover - only used when loaded as a file
    _CONFIG_DEFAULT_TEXT = None
    _config_ensure_config = None


CONFIG_DIR = Path.home() / ".config" / "vdl"
CONFIG_PATH = CONFIG_DIR / "config.toml"
SYSTEMD_USER_DIR = Path.home() / ".config" / "systemd" / "user"
TMUX_CONFIG_PATH = CONFIG_DIR / "tmux.conf"
OWNER_UNIT_NAME = "vdl.service"
WEB_UNIT_NAME = "vdl-web.service"
TMUX_SOCKET_NAME = "vdl"
TMUX_SESSION_NAME = "main"


@dataclass(frozen=True)
class InstallationPaths:
    """Paths and connection details produced by an installation."""

    config_path: Path
    owner_unit_path: Path
    web_unit_path: Path
    tmux_config_path: Path
    web_host: str
    web_port: int

    @property
    def web_url(self) -> str:
        host = self.web_host
        if ":" in host and not host.startswith("["):
            host = f"[{host}]"
        return f"http://{host}:{self.web_port}"


CommandRunner = Callable[..., subprocess.CompletedProcess[Any]]


def default_config_text(download_root: str | os.PathLike[str] | None = None) -> str:
    """Return a usable initial configuration without overwriting user data."""

    if download_root is None and _CONFIG_DEFAULT_TEXT is not None:
        return _CONFIG_DEFAULT_TEXT
    root = Path(download_root).expanduser() if download_root is not None else Path.home() / "Downloads" / "vdl"
    root_text = root.as_posix()
    return f'''download_root = {root_text!r}

check_interval = "24h"
minimum_spacing = "10m"
scheduler_poll = "1m"

log_file = "/tmp/vdl/vdl.log"

web_host = "0.0.0.0"
web_port = 8780

yt_dlp_command = [
    "yt-dlp",
    "--download-archive", ".archive"
]
'''


def ensure_config(path: str | os.PathLike[str] | None = None) -> Path:
    """Create the default config file when it does not exist.

    Existing files are left byte-for-byte untouched.  Returning the resolved
    path makes this helper convenient for both the CLI and service generator.
    """

    config_path = Path(path).expanduser() if path is not None else CONFIG_PATH
    if _config_ensure_config is not None:
        return Path(_config_ensure_config(config_path)).expanduser()
    config_path.parent.mkdir(parents=True, exist_ok=True)
    if not config_path.exists():
        config_path.write_text(default_config_text(), encoding="utf-8")
        try:
            config_path.chmod(0o600)
        except OSError:
            pass
    return config_path


def _value(config: Any, name: str, default: Any) -> Any:
    if config is None:
        return default
    if isinstance(config, Mapping):
        return config.get(name, default)
    return getattr(config, name, default)


def _read_config(path: Path) -> dict[str, Any]:
    try:
        with path.open("rb") as config_file:
            loaded = tomllib.load(config_file)
    except (FileNotFoundError, tomllib.TOMLDecodeError):
        return {}
    return loaded if isinstance(loaded, dict) else {}


def _config_values(config: Any, config_path: Path) -> tuple[str, int]:
    """Get web settings from a config object, mapping, or TOML file."""

    file_config: Mapping[str, Any] = {} if config is not None else _read_config(config_path)
    host = _value(config, "web_host", file_config.get("web_host", "0.0.0.0"))
    port = _value(config, "web_port", file_config.get("web_port", 8780))
    try:
        port_number = int(port)
    except (TypeError, ValueError):
        port_number = 8780
    return str(host), port_number


def _application_command(executable: str | os.PathLike[str] | Sequence[str] | None = None) -> list[str]:
    """Resolve the installed vdl command without depending on a shell PATH.

    The executable override is useful to package managers and tests.  When
    called through an installed console script, ``which`` gives the stable
    script path; otherwise the current interpreter and module are a reliable
    fallback.
    """

    if executable is not None:
        if isinstance(executable, Sequence) and not isinstance(executable, (str, bytes, os.PathLike)):
            return [str(part) for part in executable]
        return [str(Path(executable).expanduser())]

    found = shutil.which("vdl")
    if found:
        return [found]

    interpreter = Path(sys.executable).resolve()
    return [str(interpreter), "-m", "vdl"]


def _textual_command(textual_executable: str | os.PathLike[str] | Sequence[str] | None = None) -> list[str]:
    """Resolve Textual's supported ``serve`` command.

    ``textual serve`` is supplied by Textual's developer tools.  A direct
    override allows distributions that place that entry point at a custom
    path to use the same unit generator.
    """

    if textual_executable is not None:
        if isinstance(textual_executable, Sequence) and not isinstance(
            textual_executable, (str, bytes, os.PathLike)
        ):
            return [str(part) for part in textual_executable]
        return [str(Path(textual_executable).expanduser())]

    found = shutil.which("textual")
    if found:
        return [found]

    # Keep the generated service useful in virtual environments where the
    # executable was not exported to PATH.  Textual's module entry point is
    # the same CLI used by its ``textual`` script.
    return [str(Path(sys.executable).resolve()), "-m", "textual"]


def _systemd_line(name: str, value: Sequence[str]) -> str:
    return f"{name}={shlex.join([str(part) for part in value])}"


def _tmux_executable() -> str:
    return shutil.which("tmux") or "/usr/bin/tmux"


def _write_tmux_config(path: Path, application_command: Sequence[str]) -> None:
    # ``tmux -D`` is intentionally invoked without a command: tmux's
    # foreground mode only accepts a config file. Creating the detached
    # session from that config lets systemd supervise the actual server.
    command = shlex.join([str(part) for part in application_command])
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "# vdl owns this small dedicated tmux server.\n"
        "set-option -g exit-empty on\n"
        "set-option -g exit-unattached off\n"
        f"new-session -d -s {TMUX_SESSION_NAME} {command}\n"
        # tmux -D turns exit-empty off while starting the server on some
        # versions; setting it again keeps the intended lifetime explicit.
        "set-option -g exit-empty on\n",
        encoding="utf-8",
    )
    try:
        path.chmod(0o600)
    except OSError:
        pass


def owner_unit_text(
    *,
    application_command: Sequence[str],
    config_path: Path = CONFIG_PATH,
    tmux_config_path: Path = TMUX_CONFIG_PATH,
) -> str:
    """Build the foreground tmux owner unit."""

    tmux_command = [
        _tmux_executable(),
        "-L",
        TMUX_SOCKET_NAME,
        "-f",
        str(tmux_config_path),
        "-D",
    ]
    stop_command = [_tmux_executable(), "-L", TMUX_SOCKET_NAME, "kill-server"]
    return "\n".join(
        [
            "[Unit]",
            "Description=vdl terminal application",
            "After=default.target",
            "",
            "[Service]",
            "Type=simple",
            f"Environment=VDL_CONFIG={shlex.quote(str(config_path))}",
            _systemd_line("ExecStart", tmux_command),
            _systemd_line("ExecStop", stop_command),
            "KillMode=control-group",
            "Restart=always",
            "RestartSec=10",
            "",
            "[Install]",
            "WantedBy=default.target",
            "",
        ]
    )


def web_unit_text(
    *,
    application_command: Sequence[str],
    textual_command: Sequence[str],
    host: str,
    port: int,
    config_path: Path = CONFIG_PATH,
) -> str:
    """Build the Textual web-serving unit in client-only mode."""

    # textual serve launches this command as a child app.  ``--web`` is a
    # private vdl mode: it uses the same UI but does not own the scheduler or
    # downloader.  Keeping this command construction here makes the private
    # CLI spelling easy to adjust without touching service lifecycle code.
    web_app_command = [*application_command, "--web"]
    serve_command = [
        *textual_command,
        "serve",
        "--host",
        str(host),
        "--port",
        str(port),
        shlex.join([str(part) for part in web_app_command]),
    ]
    return "\n".join(
        [
            "[Unit]",
            "Description=vdl Textual web interface",
            "After=network-online.target",
            "Wants=network-online.target",
            "",
            "[Service]",
            "Type=simple",
            f"Environment=VDL_CONFIG={shlex.quote(str(config_path))}",
            _systemd_line("ExecStart", serve_command),
            "Restart=always",
            "RestartSec=10",
            "",
            "[Install]",
            "WantedBy=default.target",
            "",
        ]
    )


def _run_systemctl(
    args: Sequence[str],
    *,
    runner: CommandRunner | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[Any]:
    command = ["systemctl", "--user", *args]
    run = runner or subprocess.run
    return run(command, check=check)


def _print_install_summary(paths: InstallationPaths) -> None:
    print(f"Config: {paths.config_path}")
    print(f"Owner service: systemctl --user status {OWNER_UNIT_NAME}")
    print(f"Web service: systemctl --user status {WEB_UNIT_NAME}")
    print("Attach: vdl attach")
    print(f"Web URL: {paths.web_url}")


def install_services(
    config: Any = None,
    *,
    config_path: str | os.PathLike[str] | None = None,
    executable: str | os.PathLike[str] | Sequence[str] | None = None,
    textual_executable: str | os.PathLike[str] | Sequence[str] | None = None,
    runner: CommandRunner | None = None,
    print_summary: bool = True,
) -> InstallationPaths:
    """Install vdl's config, tmux config, and systemd user units."""

    path = ensure_config(config_path)
    host, port = _config_values(config, path)
    tmux_config_path = path.parent / "tmux.conf"
    owner_unit_path = SYSTEMD_USER_DIR / OWNER_UNIT_NAME
    web_unit_path = SYSTEMD_USER_DIR / WEB_UNIT_NAME
    application_command = _application_command(executable)
    textual_command = _textual_command(textual_executable)

    _write_tmux_config(tmux_config_path, application_command)
    owner_unit_path.parent.mkdir(parents=True, exist_ok=True)
    owner_unit_path.write_text(
        owner_unit_text(
            application_command=application_command,
            config_path=path,
            tmux_config_path=tmux_config_path,
        ),
        encoding="utf-8",
    )
    web_unit_path.write_text(
        web_unit_text(
            application_command=application_command,
            textual_command=textual_command,
            host=host,
            port=port,
            config_path=path,
        ),
        encoding="utf-8",
    )

    _run_systemctl(["daemon-reload"], runner=runner)
    _run_systemctl(["enable", "--now", OWNER_UNIT_NAME, WEB_UNIT_NAME], runner=runner)

    paths = InstallationPaths(path, owner_unit_path, web_unit_path, tmux_config_path, host, port)
    if print_summary:
        _print_install_summary(paths)
    return paths


def deinstall_services(
    *,
    runner: CommandRunner | None = None,
    print_summary: bool = True,
) -> tuple[Path, Path]:
    """Stop, disable, and remove only vdl's installed unit files."""

    _run_systemctl(
        ["stop", OWNER_UNIT_NAME, WEB_UNIT_NAME],
        runner=runner,
        check=False,
    )
    _run_systemctl(
        ["disable", OWNER_UNIT_NAME, WEB_UNIT_NAME],
        runner=runner,
        check=False,
    )

    owner_unit_path = SYSTEMD_USER_DIR / OWNER_UNIT_NAME
    web_unit_path = SYSTEMD_USER_DIR / WEB_UNIT_NAME
    for unit_path in (owner_unit_path, web_unit_path):
        try:
            unit_path.unlink()
        except FileNotFoundError:
            pass

    _run_systemctl(["daemon-reload"], runner=runner)
    if print_summary:
        print(f"Removed {OWNER_UNIT_NAME} and {WEB_UNIT_NAME}; configuration and downloads were preserved.")
    return owner_unit_path, web_unit_path


# Short aliases keep the CLI integration uncomplicated while the descriptive
# names remain useful to callers that want to make the lifecycle explicit.
install = install_services
deinstall = deinstall_services


__all__ = [
    "CONFIG_DIR",
    "CONFIG_PATH",
    "InstallationPaths",
    "OWNER_UNIT_NAME",
    "SYSTEMD_USER_DIR",
    "TMUX_CONFIG_PATH",
    "TMUX_SESSION_NAME",
    "TMUX_SOCKET_NAME",
    "WEB_UNIT_NAME",
    "default_config_text",
    "deinstall",
    "deinstall_services",
    "ensure_config",
    "install",
    "install_services",
    "owner_unit_text",
    "web_unit_text",
]
