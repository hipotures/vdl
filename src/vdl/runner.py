"""Run the configured yt-dlp command for one source.

The runner deliberately has very little policy.  It knows how to launch the
configured argument vector in a source directory, record the attempt marker,
and put all downloader output in the application log.  Scheduling policy lives
in :mod:`vdl.scheduler`.
"""

from __future__ import annotations

import asyncio
import codecs
from collections.abc import Sequence
import logging
import os
import signal
from pathlib import Path

from .config import Config
from .repository import Source


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


class YtDlpRunner:
    """Launch one configured yt-dlp process.

    Tests may pass an explicit command sequence in place of a full config.
    """

    def __init__(
        self,
        config_or_command: Config | Sequence[str],
        log_file: Path | str | None = None,
        logger: logging.Logger | None = None,
    ) -> None:
        if isinstance(config_or_command, Config):
            command = config_or_command.yt_dlp_command
            if log_file is None:
                log_file = config_or_command.log_file
        else:
            command = config_or_command
        if isinstance(command, (str, bytes)):
            raise TypeError("yt_dlp_command must be a sequence of arguments")
        self.command = tuple(str(argument) for argument in command)
        if not self.command:
            raise ValueError("yt_dlp_command must not be empty")
        self.logger = logger or get_logger(log_file or DEFAULT_LOG_FILE)

    async def run(self, source: Source, *, pass_fds: tuple[int, ...] = ()) -> int:
        """Run yt-dlp for ``source`` and return its process exit code.

        A missing executable, invalid working directory, or another launch
        failure is logged and re-raised.  The scheduler catches that failure
        and continues with the next poll.  The marker is touched before the
        launch call, so a failed attempt is still a check attempt.
        """

        source_path = Path(source.path)
        source_url = str(source.url)
        marker = source_path / ".last-check"
        argv = (*self.command, source_url)
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
                start_new_session=True,
                # Keep the global download lock alive if the owner crashes.
                pass_fds=pass_fds,
            )
        except asyncio.CancelledError:
            self.logger.info("yt-dlp launch cancelled source=%s", source_path)
            raise
        except Exception:
            self.logger.exception("yt-dlp launch failed source=%s", source_path)
            raise

        try:
            assert process.stdout is not None
            decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
            pending = ""
            while output := await process.stdout.read(64 * 1024):
                pending += decoder.decode(output)
                while "\n" in pending:
                    line, pending = pending.split("\n", 1)
                    self.logger.info("yt-dlp output source=%s %s", source_path, line.rstrip("\r"))
                if len(pending) > 1024 * 1024:
                    self.logger.info("yt-dlp output source=%s %s", source_path, pending)
                    pending = ""
            pending += decoder.decode(b"", final=True)
            if pending:
                self.logger.info("yt-dlp output source=%s %s", source_path, pending)
            exit_code = await process.wait()
            await self._stop_process(process)
            self.logger.info(
                "yt-dlp exit code=%s source=%s", exit_code, source_path
            )
            self.logger.info("source finished source=%s", source_path)
            return exit_code
        except asyncio.CancelledError:
            self.logger.info("yt-dlp cancelled source=%s", source_path)
            await self._stop_process(process)
            raise
        except Exception:
            self.logger.exception("yt-dlp error source=%s", source_path)
            await self._stop_process(process)
            raise

    @staticmethod
    async def _stop_process(process: asyncio.subprocess.Process) -> None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            if process.returncode is None:
                await process.wait()
            return
        for _ in range(50):
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                break
            await asyncio.sleep(0.1)
        else:
            os.killpg(process.pid, signal.SIGKILL)
        if process.returncode is None:
            await process.wait()
