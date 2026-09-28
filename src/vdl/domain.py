"""Shared application operations used by CLI, Textual, and the scheduler."""

from __future__ import annotations

from collections.abc import Iterable, Sequence
import re

from .repository import AddResult, Source, SourceRepository


def list_sources(repository: SourceRepository) -> list[Source]:
    """Read the current source list from the filesystem."""

    return repository.list_sources()


def add_source(repository: SourceRepository, url: str) -> AddResult:
    """Add a source using the repository's deterministic naming rules."""

    return repository.add_source(url)


def request_download(repository: SourceRepository, source: Source) -> Source:
    """Queue one immediate download for an active source."""

    return repository.request_download(source)


def parse_source_numbers(values: str | Iterable[str]) -> list[int]:
    """Parse source list positions separated by whitespace, commas, or semicolons."""

    if isinstance(values, str):
        text = values
    else:
        text = " ".join(values)
    pieces = [piece for piece in re.split(r"[\s,;]+", text.strip()) if piece]
    if not pieces:
        raise ValueError("at least one source number is required")
    try:
        numbers = [int(piece) for piece in pieces]
    except ValueError as exc:
        raise ValueError("source numbers must be positive integers") from exc
    if any(number < 1 for number in numbers):
        raise ValueError("source numbers must be positive integers")
    return list(dict.fromkeys(numbers))


def resolve_source_numbers(sources: Sequence[Source], values: str | Iterable[str]) -> list[Source]:
    """Resolve temporary one-based list positions against a current source list."""

    numbers = parse_source_numbers(values)
    invalid = [number for number in numbers if number > len(sources)]
    if invalid:
        joined = ", ".join(str(number) for number in invalid)
        raise ValueError(f"source number out of range: {joined}")
    return [sources[number - 1] for number in numbers]


def disable_sources(repository: SourceRepository, sources: Iterable[Source]) -> list[Source]:
    """Disable selected sources by renaming only their active marker files."""

    return [repository.disable(source) for source in sources if source.active]


def format_age(last_check: float | None, now: float | None = None) -> str:
    """Format a last-check timestamp for a compact list."""

    if last_check is None:
        return "never"
    import time

    current = time.time() if now is None else now
    seconds = max(0, int(current - last_check))
    if seconds < 60:
        return f"{seconds}s ago"
    minutes, seconds = divmod(seconds, 60)
    if minutes < 60:
        return f"{minutes}m {seconds:02d}s ago"
    hours, minutes = divmod(minutes, 60)
    if hours < 24:
        return f"{hours}h {minutes:02d}m ago"
    days, hours = divmod(hours, 24)
    return f"{days}d {hours}h ago"
