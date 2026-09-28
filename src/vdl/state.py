"""Shared snapshots and stable-ID operations for the terminal and HTTP API."""

from __future__ import annotations

import time

from .repository import SourceRepository, display_service
from .runtime import MUTATION_LOCK, OWNER_LOCK, current_download, file_lock, lock_is_held


def snapshot(repository: SourceRepository) -> dict:
    with file_lock(MUTATION_LOCK):
        busy = current_download()
        sources = repository.list_sources()
        timestamp = time.time()
        rows = []
        for source in sources:
            running = source.id == busy
            pending = source.active and (source.download_requested or source.last_check is None)
            state = ("downloading" if source.active else "finishing") if running else (
                "queued" if pending else "active" if source.active else "inactive")
            rows.append({"id": source.id, "service": display_service(source.service),
                         "account": source.account, "url": source.url,
                         "active": source.active, "state": state,
                         "download_requested": source.download_requested,
                         "last_check": source.last_check})
    return {"sources": rows, "download": busy, "owner_running": lock_is_held(OWNER_LOCK),
            "pending": sum(row["state"] == "queued" for row in rows), "time": timestamp}


def apply_action(repository: SourceRepository, action: str, source_id: str) -> None:
    if action not in {"disable", "download-now"}:
        raise ValueError("unknown source action")
    if not isinstance(source_id, str):
        raise ValueError("source ID must be a string")
    source = next((item for item in repository.list_sources() if item.id == source_id), None)
    if source is None:
        raise FileNotFoundError("source no longer exists; refresh and try again")
    if action == "disable":
        repository.disable(source)
    else:
        repository.request_download(source)
