"""Filesystem-rescanning single-worker scheduler for vdl."""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable, Iterable
import logging
import time

from .config import Config
from .repository import Source, SourceRepository
from .runner import YtDlpRunner, get_logger
from .runtime import set_busy


def _source_sort_key(source: Source) -> tuple[str, str, str]:
    """Provide deterministic tie-breaking after timestamp ordering."""

    return source.service.casefold(), source.account.casefold(), str(source.path).casefold()


def source_is_due(source: Source, now: float, check_interval: float) -> bool:
    """Return whether an active source should be considered for a check."""

    return source.last_check is None or now - source.last_check >= check_interval


def select_due_source(
    sources: Iterable[Source],
    *,
    now: float,
    check_interval: float,
    minimum_spacing: float,
) -> Source | None:
    """Select the next source without mutating the filesystem.

    Explicit download requests always win and bypass interval and spacing
    checks. Sources with no marker are otherwise new and always win. Existing
    markers must be overdue and the newest marker across active sources must
    be at least ``minimum_spacing`` old. Among overdue sources the oldest
    marker wins.
    """

    all_sources = list(sources)
    active = [source for source in all_sources if source.active]
    if not active:
        return None

    requested = [
        source for source in active if getattr(source, "download_requested", False)
    ]
    if requested:
        return min(requested, key=_source_sort_key)

    new_sources = [source for source in active if source.last_check is None]
    if new_sources:
        return min(new_sources, key=_source_sort_key)

    overdue = [
        source
        for source in active
        if source.last_check is not None and now - source.last_check >= check_interval
    ]
    if not overdue:
        return None

    latest_check = max(
        source.last_check for source in all_sources if source.last_check is not None
    )
    if now - latest_check < minimum_spacing:
        return None

    return min(
        overdue,
        key=lambda source: (source.last_check, _source_sort_key(source)),
    )


StartCallback = Callable[[Source], Awaitable[None] | None]
CompleteCallback = Callable[[Source, int | None], Awaitable[None] | None]


class Scheduler:
    """Own one recurring scheduler task and one downloader at a time."""

    def __init__(
        self,
        repository: SourceRepository,
        config: Config,
        runner: YtDlpRunner | None = None,
        *,
        on_start: StartCallback | None = None,
        on_complete: CompleteCallback | None = None,
        logger: logging.Logger | None = None,
    ) -> None:
        self.repository = repository
        self.config = config
        self.check_interval = float(config.check_interval)
        self.minimum_spacing = float(config.minimum_spacing)
        self.scheduler_poll = max(0.01, float(config.scheduler_poll))
        self.logger = logger or get_logger(
            getattr(config, "log_file", None) or None
        )
        self.runner = runner or YtDlpRunner(config, logger=self.logger)
        self.on_start = on_start
        self.on_complete = on_complete

        # This lock covers source selection and the complete subprocess run.
        # Consequently concurrent run_once() calls cannot launch two
        # downloaders, even when one comes from a UI callback and one from the
        # recurring task.
        self._worker_lock = asyncio.Lock()
        self._task: asyncio.Task[None] | None = None
        self._stop_event: asyncio.Event | None = None
        self._wake_event: asyncio.Event | None = None
        self._current_source: Source | None = None
        self.last_result: tuple[Source, int | None] | None = None

    @property
    def busy(self) -> bool:
        return self._current_source is not None

    @property
    def current_source(self) -> Source | None:
        return self._current_source

    def start(self) -> asyncio.Task[None]:
        """Start the recurring scheduler task, returning the task handle."""

        if self._task is not None and not self._task.done():
            return self._task
        self._stop_event = asyncio.Event()
        self._wake_event = asyncio.Event()
        self._task = asyncio.create_task(self._run_loop(), name="vdl-scheduler")
        return self._task

    def wake(self) -> None:
        """Ask the existing scheduler loop to scan as soon as it can."""

        if self._wake_event is not None:
            self._wake_event.set()

    async def stop(self) -> None:
        """Stop polling and cancel an in-flight downloader if necessary."""

        task = self._task
        if task is None:
            return
        if self._stop_event is not None:
            self._stop_event.set()
        if self._wake_event is not None:
            self._wake_event.set()
        if task is not asyncio.current_task() and not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        self._task = None
        self._stop_event = None
        self._wake_event = None

    async def run_once(self) -> Source | None:
        """Rescan the repository and, when eligible, run one source."""

        if self._worker_lock.locked():
            return None

        async with self._worker_lock:
            self.logger.info("scheduler scan")
            sources = list(self.repository.list_sources())
            now = time.time()
            source = select_due_source(
                sources,
                now=now,
                check_interval=self.check_interval,
                minimum_spacing=self.minimum_spacing,
            )
            if source is None:
                return None

            self._current_source = source
            set_busy(f"{source.service}/{source.account}")
            self.logger.info(
                "source selected source=%s url=%s", source.path, source.url
            )
            exit_code: int | None = None
            cancelled = False
            try:
                clear_request = getattr(self.repository, "clear_download_request", None)
                if clear_request is not None:
                    clear_request(source)
                await self._notify_start(source)
                exit_code = await self.runner.run(source)
                self.last_result = (source, exit_code)
                return source
            except asyncio.CancelledError:
                cancelled = True
                self.logger.info("source cancelled source=%s", source.path)
                raise
            except Exception:
                self.last_result = (source, None)
                self.logger.exception("source failed source=%s", source.path)
                return source
            finally:
                self._current_source = None
                set_busy(None)
                if not cancelled:
                    await self._notify_complete(source, exit_code)

    async def _notify_start(self, source: Source) -> None:
        if self.on_start is None:
            return
        result = self.on_start(source)
        if hasattr(result, "__await__"):
            await result

    async def _notify_complete(self, source: Source, exit_code: int | None) -> None:
        if self.on_complete is None:
            return
        result = self.on_complete(source, exit_code)
        if hasattr(result, "__await__"):
            await result

    async def _run_loop(self) -> None:
        stop_event = self._stop_event
        wake_event = self._wake_event
        if stop_event is None or wake_event is None:
            return
        try:
            while not stop_event.is_set():
                # Consume a wake request before scanning.  A request arriving
                # during the scan remains set and wakes the following wait.
                wake_event.clear()
                try:
                    await self.run_once()
                except asyncio.CancelledError:
                    raise
                except Exception:
                    # A transient repository failure must not silently kill
                    # the owner application.  The next poll retries.
                    self.logger.exception("scheduler scan error")
                await self._wait_for_next_scan(stop_event, wake_event)
        except asyncio.CancelledError:
            self.logger.info("scheduler stopped")
            raise

    async def _wait_for_next_scan(
        self, stop_event: asyncio.Event, wake_event: asyncio.Event
    ) -> None:
        stop_task = asyncio.create_task(stop_event.wait())
        wake_task = asyncio.create_task(wake_event.wait())
        try:
            await asyncio.wait(
                (stop_task, wake_task),
                timeout=self.scheduler_poll,
                return_when=asyncio.FIRST_COMPLETED,
            )
        finally:
            stop_task.cancel()
            wake_task.cancel()
            await asyncio.gather(stop_task, wake_task, return_exceptions=True)
