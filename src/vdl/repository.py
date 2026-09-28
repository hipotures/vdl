"""Filesystem source storage for vdl."""

from __future__ import annotations

from dataclasses import dataclass
from hashlib import sha256
from pathlib import Path
import re
from urllib.parse import unquote, urlsplit


SOURCE_FILE = ".source"
DISABLED_SOURCE_FILE = ".source.del"
ARCHIVE_FILE = ".archive"
LAST_CHECK_FILE = ".last-check"


@dataclass(frozen=True, slots=True)
class Source:
    """A source discovered below a configured download root."""

    service: str
    account: str
    path: Path
    url: str
    active: bool
    last_check: float | None

    @property
    def source_file(self) -> Path:
        return self.path / (SOURCE_FILE if self.active else DISABLED_SOURCE_FILE)

    @property
    def last_check_path(self) -> Path:
        return self.path / LAST_CHECK_FILE


@dataclass(frozen=True, slots=True)
class AddResult:
    """Result of adding a source, including whether it was already present."""

    source: Source
    created: bool


class SourceRepository:
    """Discover and mutate source directories rooted at ``download_root``."""

    def __init__(self, download_root: Path | str):
        self.download_root = Path(download_root).expanduser()

    @staticmethod
    def sort_key(source: Source) -> tuple[str, str, str]:
        return (source.service.casefold(), source.account.casefold(), source.path.as_posix().casefold())

    def list_sources(self) -> list[Source]:
        """Return all marked source directories in deterministic order."""

        if not self.download_root.is_dir():
            return []
        discovered: list[Source] = []
        for service_dir in self._sorted_directories(self.download_root):
            for source_dir in self._sorted_directories(service_dir):
                source = self._read_source(service_dir, source_dir)
                if source is not None:
                    discovered.append(source)
        return sorted(discovered, key=self.sort_key)

    def _read_source(self, service_dir: Path, source_dir: Path) -> Source | None:
        active_marker = source_dir / SOURCE_FILE
        inactive_marker = source_dir / DISABLED_SOURCE_FILE
        if active_marker.is_file():
            marker = active_marker
            active = True
        elif inactive_marker.is_file():
            marker = inactive_marker
            active = False
        else:
            return None
        try:
            url = marker.read_text(encoding="utf-8").strip()
        except OSError:
            url = ""
        try:
            last_check = (source_dir / LAST_CHECK_FILE).stat().st_mtime
        except OSError:
            last_check = None
        return Source(
            service=service_dir.name,
            account=source_dir.name,
            path=source_dir,
            url=url,
            active=active,
            last_check=last_check,
        )

    @staticmethod
    def _sorted_directories(root: Path) -> list[Path]:
        try:
            children = [child for child in root.iterdir() if child.is_dir()]
        except OSError:
            return []
        return sorted(children, key=lambda path: (path.name.casefold(), path.name))

    def add_source(self, url: str) -> AddResult:
        """Create a marked source directory or return the existing matching one."""

        if not isinstance(url, str) or not url.strip():
            raise ValueError("source URL must be a non-empty string")
        url = url.strip()
        service, account = derive_source_location(url)
        candidate = self.download_root / service / account
        existing = self._source_at(candidate)
        if existing is not None and existing.url == url:
            return AddResult(existing, created=False)
        if existing is not None or (candidate.exists() and not candidate.is_dir()):
            candidate = self._collision_path(service, account, url)
            existing = self._source_at(candidate)
            if existing is not None and existing.url == url:
                return AddResult(existing, created=False)
        candidate.mkdir(parents=True, exist_ok=True)
        marker = candidate / SOURCE_FILE
        if marker.exists():
            # A source may have appeared between discovery and creation. Resolve
            # it deterministically rather than overwriting a user's marker.
            return self.add_source_at_collision(url, service, account)
        marker.write_text(f"{url}\n", encoding="utf-8")
        source = self._read_source(candidate.parent, candidate)
        if source is None:  # pragma: no cover - the marker was just written
            raise OSError(f"could not read newly created source {marker}")
        return AddResult(source, created=True)

    def add_source_at_collision(self, url: str, service: str, account: str) -> AddResult:
        """Retry an add after a marker appeared at the initial candidate path."""

        candidate = self._collision_path(service, account, url)
        existing = self._source_at(candidate)
        if existing is not None and existing.url == url:
            return AddResult(existing, created=False)
        candidate.mkdir(parents=True, exist_ok=True)
        marker = candidate / SOURCE_FILE
        if marker.exists():
            return self.add_source_at_collision(url, service, account)
        marker.write_text(f"{url}\n", encoding="utf-8")
        source = self._read_source(candidate.parent, candidate)
        if source is None:  # pragma: no cover
            raise OSError(f"could not read newly created source {marker}")
        return AddResult(source, created=True)

    def _source_at(self, path: Path) -> Source | None:
        if not path.is_dir():
            return None
        return self._read_source(path.parent, path)

    def _collision_path(self, service: str, account: str, url: str) -> Path:
        digest = sha256(url.encode("utf-8")).hexdigest()
        for length in (8, 12, 16, 32):
            candidate = self.download_root / service / f"{account}-{digest[:length]}"
            existing = self._source_at(candidate)
            if (existing is None and (not candidate.exists() or candidate.is_dir())) or (
                existing is not None and existing.url == url
            ):
                return candidate
        # SHA-256 collisions are outside practical concern; the final suffix
        # keeps this operation finite if a test deliberately constructs one.
        counter = 2
        while True:
            candidate = self.download_root / service / f"{account}-{digest}-{counter}"
            existing = self._source_at(candidate)
            if (existing is None and (not candidate.exists() or candidate.is_dir())) or (
                existing is not None and existing.url == url
            ):
                return candidate
            counter += 1

    def disable(self, source: Source) -> Source:
        """Rename an active marker to ``.source.del`` without touching data."""

        if not source.active:
            return source
        active_marker = source.path / SOURCE_FILE
        inactive_marker = source.path / DISABLED_SOURCE_FILE
        if active_marker.exists():
            active_marker.rename(inactive_marker)
        updated = self._source_at(source.path)
        if updated is None:
            raise OSError(f"source marker disappeared: {source.path}")
        return updated

    def touch_last_check(self, source: Source, when: float | None = None) -> float:
        """Touch a source marker and return the resulting mtime."""

        marker = source.path / LAST_CHECK_FILE
        marker.touch(exist_ok=True)
        if when is not None:
            marker.touch(exist_ok=True)
            import os

            os.utime(marker, (when, when))
        return marker.stat().st_mtime


def _sanitize_component(value: str, fallback: str = "source") -> str:
    value = unquote(value).strip()
    value = re.sub(r"[^A-Za-z0-9._-]+", "-", value)
    value = re.sub(r"-+", "-", value).strip(".-_")
    return value or fallback


def _hostname_service(hostname: str) -> str:
    host = hostname.lower().rstrip(".")
    if host == "youtu.be" or host == "youtube.com" or host.endswith(".youtube.com"):
        return "youtube"
    if host == "tiktok.com" or host.endswith(".tiktok.com"):
        return "tiktok"
    if host == "instagram.com" or host.endswith(".instagram.com"):
        return "instagram"
    host = host.removeprefix("www.")
    return _sanitize_component(host.replace(".", "-"), fallback="site").lower()


def derive_source_location(url: str) -> tuple[str, str]:
    """Derive stable service and account directory names from a source URL."""

    if not isinstance(url, str) or not url.strip():
        raise ValueError("source URL must be a non-empty string")
    parsed = urlsplit(url.strip())
    if not parsed.scheme or not parsed.netloc:
        raise ValueError("source URL must include a scheme and hostname")
    hostname = parsed.hostname
    if not hostname:
        raise ValueError("source URL must include a hostname")
    service = _hostname_service(hostname)
    segments = [segment for segment in parsed.path.split("/") if segment]
    component = segments[-1] if segments else hostname
    component = component.removeprefix("@")
    account = _sanitize_component(component)
    return service, account


def source_age_seconds(source: Source, now: float | None = None) -> float | None:
    """Return elapsed seconds since a source's last check, if it has one."""

    if source.last_check is None:
        return None
    import time

    current = time.time() if now is None else now
    return max(0.0, current - source.last_check)


def display_service(service: str) -> str:
    """Human-readable service label for CLI/UI tables."""

    return service.replace("-", " ").replace("_", " ").title()
