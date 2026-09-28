"""Headless owner process for the single vdl scheduler."""

from __future__ import annotations

import asyncio
import signal

from .config import Config
from .repository import SourceRepository
from .runtime import DOWNLOAD_LOCK, OWNER_LOCK, file_lock, lock_is_held, set_busy
from .scheduler import Scheduler


async def _serve(config: Config) -> None:
    repository = SourceRepository(config.download_root)
    scheduler = Scheduler(repository, config)
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()

    installed_signals: list[signal.Signals] = []
    for signum in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(signum, stop.set)
            installed_signals.append(signum)
        except (NotImplementedError, RuntimeError):
            pass

    scheduler.start()
    try:
        await stop.wait()
    finally:
        await scheduler.stop()
        for signum in installed_signals:
            loop.remove_signal_handler(signum)


def run_owner(config: Config) -> None:
    """Run the scheduler independently from terminal and browser clients."""

    try:
        with file_lock(OWNER_LOCK, blocking=False):
            if not lock_is_held(DOWNLOAD_LOCK):
                set_busy(None)
            asyncio.run(_serve(config))
    except BlockingIOError as exc:
        raise RuntimeError("another vdl owner is already running") from exc
