"""Run the configured yt-dlp command for one source.

The runner deliberately has very little policy.  It knows how to launch the
configured argument vector in a source directory, record the attempt marker,
and put all downloader output in the application log.  Scheduling policy lives
in :mod:`vdl.scheduler`.
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence


DEFAULT_LOG_FILE = Path("/tmp/vdl/vdl.log")


def get_logger(log_file: Path | str | None = DEFAULT_LOG_FILE) -> logging.Logger:
    """Return the file logger used by the application.

    A named logger is used for each path so tests and separately configured
    application instances do not accidentally share a file handler.  The
    logger does not propagate to the root logger: the log file is the intended
    destination for application and downloader diagnostics.
    """

    path = Path(log_file or DEFAULT_LOG_FILE).expanduser()
    path.parent.mkdir(parents=True, exist_ok=True)
    name = f"vdl:{path.resolve()}"
    logger = logging.getLogger(name)
    logger.setLevel(logging.INFO)
    logger.propagate = False

    # ``get_logger`` may be called for every scheduler construction.  Reuse a
    # handler for the same path instead of appending duplicate log lines.
    if not logger.handlers:
        handler = logging.FileHandler(path, encoding="utf-8")
        handler.setFormatter(
            logging.Formatter("%(asctime)s %(levelname)s %(message)s")
        )
        logger.addHandler(handler)
    return logger


@dataclass(frozen=True, slots=True)
class RunResult:
    """The result of one attempted downloader invocation."""

    source: Any
    exit_code: int | None
    started_at: float


class YtDlpRunner:
    """Launch one configured yt-dlp process.

    ``config_or_command`` may be a configuration object exposing
    ``yt_dlp_command`` and optionally ``log_file``, or it may be the command
    sequence itself.  Keeping this small accommodation here lets the runner
    remain independent of the configuration module's concrete class.
    """

    def __init__(
        self,
        config_or_command: Any,
        log_file: Path | str | None = None,
        logger: logging.Logger | None = None,
    ) -> None:
        if hasattr(config_or_command, "yt_dlp_command"):
            command = getattr(config_or_command, "yt_dlp_command")
            if log_file is None:
                log_file = getattr(config_or_command, "log_file", DEFAULT_LOG_FILE)
        else:
            command = config_or_command
        if isinstance(command, (str, bytes)):
            raise TypeError("yt_dlp_command must be a sequence of arguments")
        self.command = tuple(str(argument) for argument in command)
        if not self.command:
            raise ValueError("yt_dlp_command must not be empty")
        self.logger = logger or get_logger(log_file or DEFAULT_LOG_FILE)

    async def run(self, source: Any) -> int:
        """Run yt-dlp for ``source`` and return its process exit code.

        The source object is intentionally duck-typed.  The repository's
        source record supplies ``path`` and ``url``; accepting an object with
        those two attributes keeps this boundary easy to test.

        A missing executable, invalid working directory, or another launch
        failure is logged and re-raised.  The scheduler catches that failure
        and continues with the next poll.  The marker is touched before the
        launch call, so a failed attempt is still a check attempt.
        """

        source_path = Path(source.path)
        source_url = str(source.url)
        marker = source_path / ".last-check"
        argv = (*self.command, source_url)
        started_at = time.time()

        try:
            # Keep this immediately adjacent to process creation.  In
            # particular, do not create the marker when a source is added.
            marker.touch(exist_ok=True)
            self.logger.info("yt-dlp started source=%s command=%r", source_path, argv)
            process = await asyncio.create_subprocess_exec(
                *argv,
                cwd=str(source_path),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
        except asyncio.CancelledError:
            self.logger.info("yt-dlp launch cancelled source=%s", source_path)
            raise
        except Exception:
            self.logger.exception("yt-dlp launch failed source=%s", source_path)
            raise

        try:
            output, _ = await process.communicate()
            if output:
                text = output.decode("utf-8", errors="replace")
                # Preserve every byte as readable text in the log.  Logging
                # line by line keeps the usual yt-dlp output easy to scan,
                # while the final line is retained even without a newline.
                for line in text.splitlines() or [text]:
                    self.logger.info("yt-dlp output source=%s %s", source_path, line)
            exit_code = await process.wait()
            self.logger.info(
                "yt-dlp exit code=%s source=%s", exit_code, source_path
            )
            self.logger.info("source finished source=%s", source_path)
            return exit_code
        except asyncio.CancelledError:
            self.logger.info("yt-dlp cancelled source=%s", source_path)
            # Cancellation normally happens while communicate() is waiting.
            # Terminate the child so stopping the owner cannot leave a
            # downloader behind.  ``returncode`` is available on asyncio's
            # Process and avoids sending a second signal after natural exit.
            if process.returncode is None:
                process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), timeout=5)
                except asyncio.TimeoutError:
                    process.kill()
                    await process.wait()
            raise
        except Exception:
            self.logger.exception("yt-dlp error source=%s", source_path)
            raise

    async def run_source(self, source: Any) -> int:
        """Compatibility spelling for callers that prefer a verb-noun name."""

        return await self.run(source)


async def run_yt_dlp(
    source: Any,
    command: Sequence[str],
    *,
    log_file: Path | str = DEFAULT_LOG_FILE,
    logger: logging.Logger | None = None,
) -> int:
    """Run ``command`` for one source using :class:`YtDlpRunner`."""

    return await YtDlpRunner(command, log_file=log_file, logger=logger).run(source)
