"""Small TOML configuration loader for vdl."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import tomllib
from typing import Any, Mapping


CONFIG_PATH = Path("~/.config/vdl/config.toml").expanduser()
DEFAULT_DOWNLOAD_ROOT = Path("~/Downloads/vdl").expanduser()
DEFAULT_LOG_FILE = Path("/tmp/vdl/vdl.log")
DEFAULT_CHECK_INTERVAL = 24 * 60 * 60
DEFAULT_MINIMUM_SPACING = 10 * 60
DEFAULT_SCHEDULER_POLL = 60.0
DEFAULT_YT_DLP_COMMAND = ("yt-dlp", "--download-archive", ".archive")

DEFAULT_CONFIG_TEXT = """download_root = \"~/Downloads/vdl\"

check_interval = \"24h\"
minimum_spacing = \"10m\"
scheduler_poll = \"1m\"

log_file = \"/tmp/vdl/vdl.log\"

web_host = \"0.0.0.0\"
web_port = 8780

yt_dlp_command = [
    \"yt-dlp\",
    \"--download-archive\", \".archive\"
]
"""


class ConfigError(ValueError):
    """Raised when a vdl configuration value cannot be used."""


_DURATION_RE = re.compile(r"^(?P<amount>\d+(?:\.\d+)?)\s*(?P<unit>[smhd])$", re.IGNORECASE)
_DURATION_FACTORS = {"s": 1.0, "m": 60.0, "h": 3600.0, "d": 86400.0}


def parse_duration(value: str | int | float) -> float:
    """Return a duration in seconds from a compact value such as ``24h``."""

    if isinstance(value, bool):
        raise ConfigError("duration must be a string such as '10m'")
    if isinstance(value, (int, float)):
        if value < 0:
            raise ConfigError("duration cannot be negative")
        return float(value)
    if not isinstance(value, str):
        raise ConfigError("duration must be a string such as '10m'")
    match = _DURATION_RE.fullmatch(value.strip())
    if match is None:
        raise ConfigError(f"invalid duration {value!r}; expected a number followed by s, m, h, or d")
    amount = float(match.group("amount"))
    return amount * _DURATION_FACTORS[match.group("unit").lower()]


@dataclass(frozen=True, slots=True)
class Config:
    """Runtime configuration with expanded filesystem paths and durations."""

    download_root: Path = DEFAULT_DOWNLOAD_ROOT
    check_interval: float = DEFAULT_CHECK_INTERVAL
    minimum_spacing: float = DEFAULT_MINIMUM_SPACING
    scheduler_poll: float = DEFAULT_SCHEDULER_POLL
    log_file: Path = DEFAULT_LOG_FILE
    web_host: str = "0.0.0.0"
    web_port: int = 8780
    yt_dlp_command: tuple[str, ...] = DEFAULT_YT_DLP_COMMAND


def _path_value(value: Any, field: str) -> Path:
    if not isinstance(value, str) or not value.strip():
        raise ConfigError(f"{field} must be a non-empty path string")
    return Path(value).expanduser()


def _duration_value(mapping: Mapping[str, Any], field: str, default: float) -> float:
    value = mapping.get(field)
    if value is None:
        return default
    parsed = parse_duration(value)
    return parsed


def _command_value(value: Any) -> tuple[str, ...]:
    if not isinstance(value, list) or not value or not all(isinstance(item, str) and item for item in value):
        raise ConfigError("yt_dlp_command must be a non-empty array of strings")
    return tuple(value)


def config_from_mapping(mapping: Mapping[str, Any]) -> Config:
    """Build a :class:`Config`, filling omitted keys from the defaults."""

    download_root = _path_value(mapping.get("download_root", str(DEFAULT_DOWNLOAD_ROOT)), "download_root")
    log_file = _path_value(mapping.get("log_file", str(DEFAULT_LOG_FILE)), "log_file")
    web_host = mapping.get("web_host", "0.0.0.0")
    if not isinstance(web_host, str) or not web_host.strip():
        raise ConfigError("web_host must be a non-empty string")
    web_port = mapping.get("web_port", 8780)
    if isinstance(web_port, bool) or not isinstance(web_port, int) or not 1 <= web_port <= 65535:
        raise ConfigError("web_port must be an integer between 1 and 65535")
    command = mapping.get("yt_dlp_command", list(DEFAULT_YT_DLP_COMMAND))
    return Config(
        download_root=download_root,
        check_interval=_duration_value(mapping, "check_interval", DEFAULT_CHECK_INTERVAL),
        minimum_spacing=_duration_value(mapping, "minimum_spacing", DEFAULT_MINIMUM_SPACING),
        scheduler_poll=_duration_value(mapping, "scheduler_poll", DEFAULT_SCHEDULER_POLL),
        log_file=log_file,
        web_host=web_host,
        web_port=web_port,
        yt_dlp_command=_command_value(command),
    )


def load_config(path: Path | str = CONFIG_PATH) -> Config:
    """Load TOML configuration, using defaults when the file does not exist."""

    config_path = Path(path).expanduser()
    if not config_path.exists():
        return Config()
    try:
        with config_path.open("rb") as handle:
            data = tomllib.load(handle)
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"invalid TOML in {config_path}: {exc}") from exc
    return config_from_mapping(data)


def ensure_config(path: Path | str = CONFIG_PATH) -> Path:
    """Create the default config file if absent and return its expanded path."""

    config_path = Path(path).expanduser()
    if not config_path.exists():
        config_path.parent.mkdir(parents=True, exist_ok=True)
        config_path.write_text(DEFAULT_CONFIG_TEXT, encoding="utf-8")
    return config_path
