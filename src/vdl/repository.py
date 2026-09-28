"""Filesystem sources and pending requests; no separate queue database."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
import re
from urllib.parse import unquote, urlsplit

from .runtime import MUTATION_LOCK, current_download, file_lock, set_busy

SOURCE_FILE = ".source"
DISABLED_SOURCE_FILE = ".source.del"
LAST_CHECK_FILE = ".last-check"
DOWNLOAD_NOW_FILE = ".download-now"
MAX_URL_LENGTH = 4096


def normalize_url(value: str) -> str:
    """Apply the same query-stripping rule to CLI, terminal and web input."""
    if not isinstance(value, str) or not value.strip():
        raise ValueError("source URL must be a non-empty string")
    if len(value) > MAX_URL_LENGTH:
        raise ValueError(f"source URL must not exceed {MAX_URL_LENGTH} characters")
    url = value.strip().partition("?")[0]
    if any(character.isspace() or ord(character) < 32 or ord(character) == 127 for character in url):
        raise ValueError("source URL must not contain spaces or control characters")
    parsed = urlsplit(url)
    if parsed.scheme.lower() not in {"http", "https"} or not parsed.hostname:
        raise ValueError("source URL must start with http:// or https:// and include a hostname")
    if parsed.username is not None or parsed.password is not None or "\\" in url:
        raise ValueError("source URL must not contain credentials or backslashes")
    # Accessing port also validates a malformed numeric port.
    _ = parsed.port
    return url


@dataclass(frozen=True, slots=True)
class Source:
    service: str
    account: str
    path: Path
    url: str
    active: bool
    last_check: float | None
    download_requested: bool = False
    queued_at: float | None = None

    @property
    def id(self) -> str:
        return f"{self.service}/{self.account}"


@dataclass(frozen=True, slots=True)
class AddResult:
    source: Source
    created: bool


class SourceRepository:
    """Keep mutation locks short: never hold one while downloading."""

    def __init__(self, download_root: Path | str):
        self.download_root = Path(download_root).expanduser()

    @staticmethod
    def sort_key(source: Source) -> tuple[str, str, str]:
        return source.service.casefold(), source.account.casefold(), str(source.path).casefold()

    def list_sources(self) -> list[Source]:
        if not self.download_root.is_dir():
            return []
        discovered = []
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
            marker, active = active_marker, True
        elif inactive_marker.is_file():
            marker, active = inactive_marker, False
        else:
            return None
        try:
            url = marker.read_text(encoding="utf-8", errors="replace").strip()
        except OSError:
            url = ""
        last_check = self._mtime(source_dir / LAST_CHECK_FILE)
        requested_at = self._mtime(source_dir / DOWNLOAD_NOW_FILE)
        queued_at = requested_at
        if queued_at is None and last_check is None:
            queued_at = self._mtime(marker)
        return Source(service_dir.name, source_dir.name, source_dir, url, active,
                      last_check, requested_at is not None, queued_at)

    @staticmethod
    def _mtime(path: Path) -> float | None:
        try:
            return path.stat().st_mtime
        except OSError:
            return None

    @staticmethod
    def _sorted_directories(root: Path) -> list[Path]:
        try:
            children = [child for child in root.iterdir() if child.is_dir() and not child.is_symlink()]
        except OSError:
            return []
        return sorted(children, key=lambda path: (path.name.casefold(), path.name))

    def _source_at(self, path: Path) -> Source | None:
        # API clients pass stable IDs, never arbitrary filesystem paths. Also
        # reject a directory replaced by a symlink after the last snapshot.
        root = self.download_root.resolve()
        if not path.resolve().is_relative_to(root) or path.is_symlink() or path.parent.is_symlink():
            raise ValueError("source directory must remain inside download_root")
        return self._read_source(path.parent, path) if path.is_dir() else None

    def add_source(self, url: str) -> AddResult:
        url = normalize_url(url)
        service, account = derive_source_location(url)
        candidate = self.download_root / service / account
        with file_lock(MUTATION_LOCK):
            existing = self._source_at(candidate)
            if existing is not None:
                if existing.url == url:
                    return AddResult(existing, False)
                raise FileExistsError(f"{candidate} already contains a different source URL")
            candidate.mkdir(parents=True, exist_ok=True)
            with (candidate / SOURCE_FILE).open("x", encoding="utf-8") as handle:
                handle.write(f"{url}\n")
            source = self._source_at(candidate)
        if source is None:
            raise OSError("could not read newly created source")
        return AddResult(source, True)

    def disable(self, source: Source) -> Source:
        """Disable future attempts; allow an already claimed attempt to finish."""
        with file_lock(MUTATION_LOCK):
            current = self._source_at(source.path)
            if current is None:
                raise FileNotFoundError("source marker disappeared")
            if current.active:
                (current.path / SOURCE_FILE).rename(current.path / DISABLED_SOURCE_FILE)
            (current.path / DOWNLOAD_NOW_FILE).unlink(missing_ok=True)
            updated = self._source_at(current.path)
        if updated is None:
            raise OSError("source marker disappeared")
        return updated

    def request_download(self, source: Source) -> Source:
        """Coalesce repeated clicks, including requests for the running source."""
        with file_lock(MUTATION_LOCK):
            current = self._source_at(source.path)
            if current is None:
                raise FileNotFoundError("source marker disappeared")
            if not current.active:
                raise ValueError("source is inactive")
            if current_download() != current.id:
                try:
                    (current.path / DOWNLOAD_NOW_FILE).touch(exist_ok=False)
                except FileExistsError:
                    pass  # Do not change the timestamp of an existing request.
            updated = self._source_at(current.path)
        if updated is None:
            raise OSError("source marker disappeared")
        return updated

    def clear_download_request(self, source: Source) -> None:
        with file_lock(MUTATION_LOCK):
            (source.path / DOWNLOAD_NOW_FILE).unlink(missing_ok=True)

    def claim_next(self, select: Callable[[list[Source]], Source | None]) -> Source | None:
        """Select, publish and consume a request atomically against mutations.

        The scheduler holds DOWNLOAD_LOCK, not MUTATION_LOCK, for the actual
        subprocess. Disabling before this claim prevents the attempt; disabling
        afterwards affects only future attempts.
        """
        with file_lock(MUTATION_LOCK):
            source = select(self.list_sources())
            if source is not None:
                set_busy(source.id)
                (source.path / DOWNLOAD_NOW_FILE).unlink(missing_ok=True)
            return source


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
    """Keep the existing service/account naming rules and directory layout."""
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
    if len(segments) > 1 and segments[-1].casefold() in {"posts", "reels", "shorts", "videos"}:
        segments.pop()
    component = segments[-1] if segments else hostname
    account = _sanitize_component(component.removeprefix("@"))
    return service, account


def display_service(service: str) -> str:
    known = {"tiktok": "TikTok", "youtube": "YouTube"}
    return known.get(service.casefold(), service.replace("-", " ").replace("_", " ").title())
