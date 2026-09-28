"""Small cross-process coordination files under vdl's runtime directory."""

from __future__ import annotations

from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
from typing import Iterator, TextIO


def _runtime_dir() -> Path:
    xdg_runtime = os.environ.get("XDG_RUNTIME_DIR")
    candidates = [Path(xdg_runtime) / "vdl"] if xdg_runtime else []
    candidates.append(Path(f"/tmp/vdl-{os.getuid()}"))
    for candidate in candidates:
        try:
            candidate.mkdir(mode=0o700, parents=True, exist_ok=True)
            if candidate.stat().st_uid != os.getuid():
                continue
            candidate.chmod(0o700)
            return candidate
        except OSError:
            continue
    raise RuntimeError("could not create vdl runtime directory")


RUNTIME_DIR = _runtime_dir()
BUSY_FILE = RUNTIME_DIR / "busy"
OWNER_LOCK = RUNTIME_DIR / "owner.lock"
MUTATION_LOCK = RUNTIME_DIR / "mutation.lock"


@contextmanager
def file_lock(path: Path, *, blocking: bool = True) -> Iterator[TextIO]:
    handle = path.open("a+", encoding="utf-8")
    operation = fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB)
    try:
        fcntl.flock(handle, operation)
        yield handle
    finally:
        handle.close()


def set_busy(label: str | None) -> None:
    if label is None:
        BUSY_FILE.unlink(missing_ok=True)
    else:
        temporary = RUNTIME_DIR / f".busy-{os.getpid()}"
        temporary.write_text(f"{label}\n", encoding="utf-8")
        temporary.replace(BUSY_FILE)


def read_busy() -> str | None:
    try:
        return BUSY_FILE.read_text(encoding="utf-8").strip() or None
    except (OSError, UnicodeError):
        return None
