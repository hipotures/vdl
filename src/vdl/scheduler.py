"""One downloader, with pending work inferred from filesystem markers."""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable, Iterable
from contextlib import ExitStack
import inspect
import logging
import time

from .config import Config
from .repository import Source, SourceRepository
from .runner import YtDlpRunner, get_logger
from .runtime import DOWNLOAD_LOCK, file_lock, set_busy


def _source_sort_key(source: Source) -> tuple[str, str, str]:
    return source.service.casefold(), source.account.casefold(), str(source.path).casefold()


def _pending_key(source: Source) -> tuple[float, tuple[str, str, str]]:
    return getattr(source, "queued_at", None) or 0.0, _source_sort_key(source)


def source_is_due(source: Source, now: float, check_interval: float) -> bool:
    return source.last_check is None or now - source.last_check >= check_interval


def select_due_source(sources: Iterable[Source], *, now: float,
                      check_interval: float, minimum_spacing: float) -> Source | None:
    """Manual requests, then new sources, then the oldest overdue check.

    Pending requests and new sources use their existing marker timestamps.
    Interval and spacing rules apply only to ordinary recurring checks.
    """
    all_sources = list(sources)
    active = [source for source in all_sources if source.active]
    requested = [source for source in active if source.download_requested]
    if requested:
        return min(requested, key=_pending_key)
    new = [source for source in active if source.last_check is None]
    if new:
        return min(new, key=_pending_key)
    overdue = [source for source in active if source_is_due(source, now, check_interval)]
    if not overdue:
        return None
    latest = max(source.last_check for source in all_sources if source.last_check is not None)
    if now - latest < minimum_spacing:
        return None
    return min(overdue, key=lambda source: (source.last_check, _source_sort_key(source)))


StartCallback = Callable[[Source], Awaitable[None] | None]
CompleteCallback = Callable[[Source, int | None], Awaitable[None] | None]


class Scheduler:
    def __init__(self, repository: SourceRepository, config: Config,
                 runner: YtDlpRunner | None = None, *,
                 on_start: StartCallback | None = None,
                 on_complete: CompleteCallback | None = None,
                 logger: logging.Logger | None = None) -> None:
        self.repository = repository
        self.config = config
        self.check_interval = float(config.check_interval)
        self.minimum_spacing = float(config.minimum_spacing)
        # Cross-process additions become visible within a second while idle.
        # This does not change check_interval or minimum_spacing.
        self.scheduler_poll = min(1.0, max(0.01, float(config.scheduler_poll)))
        self.logger = logger or get_logger(getattr(config, "log_file", None))
        self.runner = runner or YtDlpRunner(config, logger=self.logger)
        self.on_start, self.on_complete = on_start, on_complete
        self._worker_lock = asyncio.Lock()
        self._task: asyncio.Task | None = None
        self._wake_event: asyncio.Event | None = None
        self._current_source: Source | None = None
        self.last_result: tuple[Source, int | None] | None = None

    @property
    def busy(self) -> bool:
        return self._current_source is not None

    @property
    def current_source(self) -> Source | None:
        return self._current_source

    def start(self) -> asyncio.Task:
        if self._task is None or self._task.done():
            self._wake_event = asyncio.Event()
            self._task = asyncio.create_task(self._run_loop(), name="vdl-scheduler")
        return self._task

    def wake(self) -> None:
        if self._wake_event is not None:
            self._wake_event.set()

    async def stop(self) -> None:
        if self._task is not None:
            self._task.cancel()
            await asyncio.gather(self._task, return_exceptions=True)
        self._task = None
        self._wake_event = None

    def _claim(self) -> Source | None:
        def select(sources):
            return select_due_source(sources, now=time.time(),
                                     check_interval=self.check_interval,
                                     minimum_spacing=self.minimum_spacing)
        if hasattr(self.repository, "claim_next"):
            return self.repository.claim_next(select)
        # Small test doubles can implement just list_sources().
        source = select(self.repository.list_sources())
        if source is not None:
            set_busy(f"{source.service}/{source.account}")
        return source

    async def run_once(self) -> Source | None:
        if self._worker_lock.locked():
            return None
        async with self._worker_lock:
            with ExitStack() as stack:
                try:
                    handle = stack.enter_context(file_lock(DOWNLOAD_LOCK, blocking=False))
                except BlockingIOError:
                    return None
                set_busy(None)
                return await self._run_claimed(handle.fileno())

    async def _run_claimed(self, download_fd: int) -> Source | None:
        # Shield a filesystem claim so cancellation cannot leave a thread
        # publishing stale busy state after the downloader lock was released.
        claim = asyncio.create_task(asyncio.to_thread(self._claim))
        try:
            source = await asyncio.shield(claim)
        except asyncio.CancelledError:
            source = await claim
            set_busy(None)
            if source is not None and hasattr(self.repository, "request_download"):
                try:
                    await asyncio.to_thread(self.repository.request_download, source)
                except (OSError, ValueError):
                    self.logger.exception("could not restore cancelled claim")
            raise
        if source is None:
            return None
        self._current_source = source
        code = None
        cancelled = False
        try:
            self.logger.info("source selected source=%s url=%s", source.path, source.url)
            await self._notify(self.on_start, source)
            if isinstance(self.runner, YtDlpRunner):
                code = await self.runner.run(source, pass_fds=(download_fd,))
            else:
                code = await self.runner.run(source)
            self.last_result = (source, code)
            return source
        except asyncio.CancelledError:
            cancelled = True
            raise
        except Exception:
            self.last_result = (source, None)
            self.logger.exception("source failed source=%s", source.path)
            return source
        finally:
            self._current_source = None
            set_busy(None)
            if not cancelled:
                await self._notify(self.on_complete, source, code)

    async def _notify(self, callback, *args) -> None:
        if callback is not None:
            try:
                result = callback(*args)
                if inspect.isawaitable(result):
                    await result
            except Exception:
                self.logger.exception("display callback failed")

    async def _run_loop(self) -> None:
        while True:
            assert self._wake_event is not None
            self._wake_event.clear()
            try:
                completed = await self.run_once()
            except asyncio.CancelledError:
                raise
            except Exception:
                self.logger.exception("scheduler scan error")
                completed = None
            if completed is not None:
                # Do not sleep a polling interval between pending downloads.
                await asyncio.sleep(0)
                continue
            try:
                await asyncio.wait_for(self._wake_event.wait(), self.scheduler_poll)
            except TimeoutError:
                pass
