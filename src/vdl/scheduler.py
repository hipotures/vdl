"""Filesystem-rescanning single-worker scheduler for vdl."""

from __future__ import annotations

import asyncio
import inspect
import logging
import time
from pathlib import Path
from typing import Any, Callable, Iterable

from .runner import YtDlpRunner, get_logger


def _last_check(source: Any) -> float | None:
    """Read a source's marker timestamp, tolerating lightweight source fakes."""

    value = getattr(source, "last_check", None)
    if value is not None:
        try:
            return float(value)
        except (TypeError, ValueError):
            pass
    path = getattr(source, "path", None)
    if path is None:
        return None
    try:
        return Path(path, ".last-check").stat().st_mtime
    except (FileNotFoundError, NotADirectoryError, OSError):
        return None


def _source_sort_key(source: Any) -> tuple[str, str, str]:
    """Provide deterministic tie-breaking after timestamp ordering."""

    service = str(getattr(source, "service", "")).casefold()
    account = str(getattr(source, "account", "")).casefold()
    path = str(getattr(source, "path", "")).casefold()
    return service, account, path


def source_is_due(source: Any, now: float, check_interval: float) -> bool:
    """Return whether an active source should be considered for a check."""

    marker_time = _last_check(source)
    return marker_time is None or now - marker_time >= check_interval


def select_due_source(
    sources: Iterable[Any],
    *,
    now: float,
    check_interval: float,
    minimum_spacing: float,
) -> Any | None:
    """Select the next source without mutating the filesystem.

    Sources with no marker are new and always win.  Existing markers must be
    overdue and the newest marker across active sources must be at least
    ``minimum_spacing`` old.  Among overdue sources the oldest marker wins.
    """

    all_sources = list(sources)
    marker_times = {id(source): _last_check(source) for source in all_sources}
    active = [source for source in all_sources if bool(getattr(source, "active", True))]
    if not active:
        return None

    new_sources = [source for source in active if marker_times[id(source)] is None]
    if new_sources:
        return min(new_sources, key=_source_sort_key)

    overdue = [
        source
        for source in active
        if now - marker_times[id(source)] >= check_interval
    ]
    if not overdue:
        return None

    latest_check = max(
        marker_time for marker_time in marker_times.values() if marker_time is not None
    )
    if now - latest_check < minimum_spacing:
        return None

    return min(
        overdue,
        key=lambda source: (marker_times[id(source)], _source_sort_key(source)),
    )


# A concise alias reads naturally in tests and callers that use “next” rather
# than “due” terminology.
select_next_source = select_due_source


class Scheduler:
    """Own one recurring scheduler task and one downloader at a time."""

    def __init__(
        self,
        repository: Any,
        config: Any,
        runner: YtDlpRunner | Any | None = None,
        *,
        on_complete: Callable[..., Any] | None = None,
        on_change: Callable[..., Any] | None = None,
        logger: logging.Logger | None = None,
        clock: Callable[[], float] = time.time,
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
        self.on_complete = on_complete if on_complete is not None else on_change
        self.clock = clock

        # This lock covers source selection and the complete subprocess run.
        # Consequently concurrent run_once() calls cannot launch two
        # downloaders, even when one comes from a UI callback and one from the
        # recurring task.
        self._worker_lock = asyncio.Lock()
        self._task: asyncio.Task[None] | None = None
        self._stop_event: asyncio.Event | None = None
        self._current_source: Any | None = None
        self.last_result: tuple[Any, int | None] | None = None

    @property
    def busy(self) -> bool:
        return self._worker_lock.locked()

    @property
    def current_source(self) -> Any | None:
        return self._current_source

    @property
    def task(self) -> asyncio.Task[None] | None:
        return self._task

    def start(self) -> asyncio.Task[None]:
        """Start the recurring scheduler task, returning the task handle."""

        if self._task is not None and not self._task.done():
            return self._task
        self._stop_event = asyncio.Event()
        self._task = asyncio.create_task(self._run_loop(), name="vdl-scheduler")
        return self._task

    async def stop(self) -> None:
        """Stop polling and cancel an in-flight downloader if necessary."""

        task = self._task
        if task is None:
            return
        if self._stop_event is not None:
            self._stop_event.set()
        if task is not asyncio.current_task() and not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        self._task = None
        self._stop_event = None

    async def run_once(self) -> Any | None:
        """Rescan the repository and, when eligible, run one source."""

        if self._worker_lock.locked():
            return None

        async with self._worker_lock:
            self.logger.info("scheduler scan")
            sources = list(self.repository.list_sources())
            now = self.clock()
            source = select_due_source(
                sources,
                now=now,
                check_interval=self.check_interval,
                minimum_spacing=self.minimum_spacing,
            )
            if source is None:
                return None

            self._current_source = source
            self.logger.info(
                "source selected source=%s url=%s",
                getattr(source, "path", ""),
                getattr(source, "url", ""),
            )
            exit_code: int | None = None
            completed = False
            cancelled = False
            try:
                exit_code = await self.runner.run(source)
                completed = True
                self.last_result = (source, exit_code)
                return source
            except asyncio.CancelledError:
                cancelled = True
                self.logger.info(
                    "source cancelled source=%s", getattr(source, "path", "")
                )
                raise
            except Exception:
                self.last_result = (source, None)
                self.logger.exception(
                    "source failed source=%s", getattr(source, "path", "")
                )
                return source
            finally:
                self._current_source = None
                if not cancelled and (completed or exit_code is None):
                    await self._notify_complete(source, exit_code)

    async def _notify_complete(self, source: Any, exit_code: int | None) -> None:
        callback = self.on_complete
        if callback is None:
            return
        try:
            signature = inspect.signature(callback)
        except (TypeError, ValueError):
            result = callback(source, exit_code)
        else:
            # Signature inspection happens before invocation so a TypeError
            # raised by the callback itself is not mistaken for an arity
            # mismatch.  Textual refresh callbacks commonly take no
            # arguments, while integrations may want the source or exit code.
            for arguments in ((source, exit_code), (source,), ()):
                try:
                    signature.bind(*arguments)
                except TypeError:
                    continue
                result = callback(*arguments)
                break
            else:  # pragma: no cover - an invalid callback signature
                raise TypeError("on_complete callback has unsupported arguments")
        if inspect.isawaitable(result):
            await result

    async def _run_loop(self) -> None:
        stop_event = self._stop_event
        if stop_event is None:
            return
        try:
            while not stop_event.is_set():
                try:
                    await self.run_once()
                except asyncio.CancelledError:
                    raise
                except Exception:
                    # A transient repository failure must not silently kill
                    # the owner application.  The next poll retries.
                    self.logger.exception("scheduler scan error")
                try:
                    await asyncio.wait_for(
                        stop_event.wait(), timeout=self.scheduler_poll
                    )
                except asyncio.TimeoutError:
                    pass
        except asyncio.CancelledError:
            self.logger.info("scheduler stopped")
            raise

    async def run_forever(self) -> None:
        """Run polling in the current task (useful for a service entrypoint)."""

        task = self.start()
        await task
