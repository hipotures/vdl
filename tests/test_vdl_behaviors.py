from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest
from textual.widgets import Button, DataTable

from vdl.config import config_from_mapping, parse_duration
from vdl.domain import disable_sources, parse_source_numbers
from vdl.install import (
    _write_tmux_config,
    deinstall_services,
    install_services,
    owner_unit_text,
    web_unit_text,
)
from vdl.repository import SourceRepository, derive_source_location
from vdl.runner import YtDlpRunner
from vdl.runtime import OWNER_LOCK, file_lock, set_busy
from vdl.scheduler import Scheduler, select_due_source, source_is_due
from vdl.ui import VdlApp


def _config(root: Path, **overrides: object):
    values: dict[str, object] = {
        "download_root": str(root),
        "check_interval": "24h",
        "minimum_spacing": "10m",
        "scheduler_poll": "1m",
        "log_file": str(root / "vdl.log"),
        "web_host": "127.0.0.1",
        "web_port": 8780,
        "yt_dlp_command": ["yt-dlp", "--download-archive", ".archive"],
    }
    values.update(overrides)
    return config_from_mapping(values)


def _fake_source(
    name: str,
    last_check: float | None,
    *,
    active: bool = True,
    download_requested: bool = False,
):
    return SimpleNamespace(
        service="site",
        account=name,
        path=Path("/tmp/vdl-test") / name,
        url=f"https://example.test/{name}",
        active=active,
        last_check=last_check,
        download_requested=download_requested,
    )


def test_url_derivation_and_existing_directory_handling(tmp_path: Path):
    assert derive_source_location("https://www.tiktok.com/@foo?tab=videos#top") == (
        "tiktok",
        "foo",
    )
    assert derive_source_location("https://www.youtube.com/@foo/") == (
        "youtube",
        "foo",
    )
    assert derive_source_location("https://www.youtube.com/@foo/videos") == (
        "youtube",
        "foo",
    )
    assert derive_source_location("https://www.instagram.com/foo/") == (
        "instagram",
        "foo",
    )
    assert derive_source_location("https://video.example.org/@some user/") == (
        "video-example-org",
        "some-user",
    )

    repository = SourceRepository(tmp_path)
    first = repository.add_source("https://www.tiktok.com/@foo")
    repeated = repository.add_source("https://www.tiktok.com/@foo")

    unrelated = tmp_path / "instagram" / "reserved"
    unrelated.mkdir(parents=True)
    (unrelated / "keep.txt").write_text("untouched", encoding="utf-8")
    adopted = repository.add_source("https://www.instagram.com/reserved/")

    assert first.created
    assert not repeated.created
    assert repeated.source.account == "foo"
    assert adopted.source.path == unrelated
    assert (unrelated / "keep.txt").read_text(encoding="utf-8") == "untouched"
    assert (unrelated / ".source").read_text(encoding="utf-8") == (
        "https://www.instagram.com/reserved/\n"
    )


def test_filesystem_discovery_distinguishes_active_inactive_and_ignored(tmp_path: Path):
    active = tmp_path / "tiktok" / "active"
    inactive = tmp_path / "tiktok" / "inactive"
    ignored = tmp_path / "tiktok" / "ignored"
    active.mkdir(parents=True)
    inactive.mkdir(parents=True)
    ignored.mkdir(parents=True)
    (active / ".source").write_text("https://www.tiktok.com/@active\n", encoding="utf-8")
    (inactive / ".source.del").write_text(
        "https://www.tiktok.com/@inactive\n", encoding="utf-8"
    )
    (ignored / "media.mp4").write_bytes(b"data")
    malformed = tmp_path / "youtube" / "malformed"
    malformed.mkdir(parents=True)
    (malformed / ".source").write_bytes(b"https://example.test/\xff")

    sources = SourceRepository(tmp_path).list_sources()

    assert [(source.account, source.active, source.url) for source in sources] == [
        ("active", True, "https://www.tiktok.com/@active"),
        ("inactive", False, "https://www.tiktok.com/@inactive"),
        ("malformed", True, "https://example.test/�"),
    ]


def test_add_creates_source_marker_without_a_check_marker(tmp_path: Path):
    repository = SourceRepository(tmp_path)

    result = repository.add_source(
        "https://www.instagram.com/example/?utm_source=test&tab=posts"
    )

    source = result.source
    assert result.created
    assert source.path == tmp_path / "instagram" / "example"
    assert (source.path / ".source").read_text(encoding="utf-8") == (
        "https://www.instagram.com/example/\n"
    )
    assert not (source.path / ".source.del").exists()
    assert not (source.path / ".last-check").exists()


def test_add_treats_urls_with_different_query_strings_as_the_same_source(
    tmp_path: Path,
):
    repository = SourceRepository(tmp_path)

    first = repository.add_source("https://example.test/account?one=1")
    repeated = repository.add_source("https://example.test/account?two=2")

    assert first.created
    assert not repeated.created
    assert repeated.source.url == "https://example.test/account"


def test_disable_renames_only_the_source_marker(tmp_path: Path):
    repository = SourceRepository(tmp_path)
    source = repository.add_source("https://www.youtube.com/@archive-test").source
    media = source.path / "video.mp4"
    archive = source.path / ".archive"
    last_check = source.path / ".last-check"
    media.write_bytes(b"media")
    archive.write_text("downloaded-id\n", encoding="utf-8")
    last_check.write_text("marker", encoding="utf-8")
    last_check_mtime = last_check.stat().st_mtime_ns

    updated = disable_sources(repository, [source])[0]

    assert not (source.path / ".source").exists()
    assert (source.path / ".source.del").read_text(encoding="utf-8") == (
        "https://www.youtube.com/@archive-test\n"
    )
    assert media.read_bytes() == b"media"
    assert archive.read_text(encoding="utf-8") == "downloaded-id\n"
    assert last_check.read_text(encoding="utf-8") == "marker"
    assert last_check.stat().st_mtime_ns == last_check_mtime
    assert not updated.active


def test_source_number_parser_accepts_mixed_delimiters():
    for value in ("2 7 15", "2,7,15", "2;7;15", "2, 7 15"):
        assert parse_source_numbers(value) == [2, 7, 15]
    assert parse_source_numbers(["2,", "7", "15"]) == [2, 7, 15]


def test_scheduler_due_selection_handles_new_oldest_and_spacing():
    now = 1_000.0
    newest = _fake_source("newest", 900.0)
    oldest = _fake_source("oldest", 500.0)
    middle = _fake_source("middle", 650.0)
    new = _fake_source("new", None)
    inactive = _fake_source("inactive", 100.0, active=False)
    sources = [newest, inactive, middle, new, oldest]
    requested = _fake_source("requested", 999.0, download_requested=True)

    assert source_is_due(new, now, 10_000.0)
    assert source_is_due(oldest, now, 200.0)
    assert not source_is_due(newest, now, 200.0)
    assert select_due_source(
        sources, now=now, check_interval=200.0, minimum_spacing=10_000.0
    ) is new
    assert select_due_source(
        [*sources, requested],
        now=now,
        check_interval=10_000.0,
        minimum_spacing=10_000.0,
    ) is requested

    without_new = [source for source in sources if source is not new]
    assert select_due_source(
        without_new, now=now, check_interval=200.0, minimum_spacing=150.0
    ) is None
    assert select_due_source(
        without_new, now=now, check_interval=200.0, minimum_spacing=100.0
    ) is oldest
    recently_disabled = _fake_source("disabled-now", 950.0, active=False)
    assert select_due_source(
        [oldest, middle, recently_disabled],
        now=now,
        check_interval=200.0,
        minimum_spacing=100.0,
    ) is None


def test_duration_parser_supports_documented_units():
    assert parse_duration("30s") == 30
    assert parse_duration("10m") == 600
    assert parse_duration("24h") == 86_400
    assert parse_duration("2d") == 172_800


@pytest.mark.asyncio
async def test_runner_uses_configured_argv_cwd_and_touches_marker(tmp_path: Path):
    repository = SourceRepository(tmp_path / "downloads")
    source = repository.add_source("https://example.test/account").source
    fake = tmp_path / "fake-yt-dlp"
    fake.write_text(
        "#!/usr/bin/env python3\n"
        "import json, os, pathlib, sys\n"
        "pathlib.Path(sys.argv[2]).touch()\n"
        "pathlib.Path('runner-record.json').write_text(json.dumps({\n"
        "    'cwd': os.getcwd(),\n"
        "    'argv': sys.argv[1:],\n"
        "    'marker_at_start': pathlib.Path('.last-check').exists(),\n"
        "}), encoding='utf-8')\n"
        "print('fake stdout')\n"
        "print('x' * 70000)\n"
        "print('fake stderr', file=sys.stderr)\n",
        encoding="utf-8",
    )
    fake.chmod(fake.stat().st_mode | 0o111)
    log_file = tmp_path / "logs" / "vdl.log"
    runner = YtDlpRunner(
        [str(fake), "--download-archive", ".archive"], log_file=log_file
    )

    exit_code = await runner.run(source)

    record = json.loads((source.path / "runner-record.json").read_text(encoding="utf-8"))
    assert exit_code == 0
    assert record == {
        "cwd": str(source.path),
        "argv": ["--download-archive", ".archive", source.url],
        "marker_at_start": True,
    }
    assert (source.path / ".archive").exists()
    assert (source.path / ".last-check").exists()
    log = log_file.read_text(encoding="utf-8")
    assert "fake stdout" in log
    assert "fake stderr" in log
    assert "x" * 70_000 in log
    assert "yt-dlp exit code=0" in log


@pytest.mark.asyncio
async def test_scheduler_never_runs_two_downloads_at_once(tmp_path: Path):
    source = _fake_source("one", None)
    started = asyncio.Event()
    release = asyncio.Event()
    calls: list[object] = []
    active = 0
    max_active = 0

    class BlockingRunner:
        async def run(self, selected_source):
            nonlocal active, max_active
            calls.append(selected_source)
            active += 1
            max_active = max(max_active, active)
            started.set()
            await release.wait()
            active -= 1
            return 0

    class Repository:
        def list_sources(self):
            return [source]

    config = SimpleNamespace(
        check_interval=0,
        minimum_spacing=0,
        scheduler_poll=1,
        log_file=tmp_path / "scheduler.log",
    )
    completion_busy_states: list[bool] = []
    scheduler = Scheduler(
        Repository(),
        config,
        runner=BlockingRunner(),
        on_complete=lambda _source, _code: completion_busy_states.append(scheduler.busy),
    )
    first = asyncio.create_task(scheduler.run_once())
    await started.wait()
    second = asyncio.create_task(scheduler.run_once())

    assert await second is None
    release.set()
    assert await first is source
    assert len(calls) == 1
    assert max_active == 1
    assert not scheduler.busy
    assert completion_busy_states == [False]


@pytest.mark.asyncio
async def test_download_now_bypasses_schedule_and_is_consumed(tmp_path: Path):
    repository = SourceRepository(tmp_path / "downloads")
    source = repository.add_source("https://example.test/download-now").source
    marker = source.path / ".last-check"
    marker.touch()
    source = repository.list_sources()[0]
    repository.request_download(source)
    calls: list[object] = []

    class RecordingRunner:
        async def run(self, selected_source):
            calls.append(selected_source)
            return 0

    config = SimpleNamespace(
        check_interval=10_000,
        minimum_spacing=10_000,
        scheduler_poll=60,
        log_file=tmp_path / "scheduler.log",
    )
    scheduler = Scheduler(repository, config, runner=RecordingRunner())

    selected = await scheduler.run_once()

    assert selected is not None
    assert selected.path == source.path
    assert len(calls) == 1
    assert not (source.path / ".download-now").exists()


@pytest.mark.asyncio
async def test_runner_cancellation_stops_downloader_process_group(tmp_path: Path):
    source = SourceRepository(tmp_path / "downloads").add_source(
        "https://example.test/cancel"
    ).source
    child_marker = tmp_path / "child-survived"
    fake = tmp_path / "fake-tree"
    fake.write_text(
        "#!/usr/bin/env python3\n"
        "import subprocess, sys, time\n"
        "subprocess.Popen([sys.executable, '-c', "
        f"\"import time; from pathlib import Path; time.sleep(1); Path({str(child_marker)!r}).write_text('bad')\"])\n"
        "time.sleep(30)\n",
        encoding="utf-8",
    )
    fake.chmod(0o755)
    task = asyncio.create_task(YtDlpRunner([str(fake)], log_file=tmp_path / "log").run(source))
    await asyncio.sleep(0.2)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    await asyncio.sleep(1.1)
    assert not child_marker.exists()

    fake.write_text(
        "#!/usr/bin/env python3\n"
        "import subprocess, sys\n"
        "subprocess.Popen([sys.executable, '-c', "
        f"\"import time; from pathlib import Path; time.sleep(1); Path({str(child_marker)!r}).write_text('bad')\"], "
        "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n",
        encoding="utf-8",
    )
    assert await YtDlpRunner([str(fake)], log_file=tmp_path / "log-2").run(source) == 0
    await asyncio.sleep(1.1)
    assert not child_marker.exists()


@pytest.mark.asyncio
async def test_client_textual_app_reads_sources_without_starting_scheduler(tmp_path: Path):
    repository = SourceRepository(tmp_path)
    repository.add_source("https://www.tiktok.com/@ui-test")
    app = VdlApp(_config(tmp_path), owner=False)

    async with app.run_test() as pilot:
        await pilot.pause()
        table = app.query_one("#sources", DataTable)
        assert table.row_count == 1
        assert app.scheduler is None
        assert not app.query_one("#add", Button).disabled
        assert not app.query_one("#disable", Button).disabled
        assert not app.query_one("#download-now", Button).disabled
        set_busy("tiktok/ui-test")
        app._sync_runtime_busy()
        assert app.query_one("#add", Button).disabled
        assert app.query_one("#disable", Button).disabled
        assert app.query_one("#download-now", Button).disabled
        set_busy(None)
        app._sync_runtime_busy()
        assert not app.query_one("#download-now", Button).disabled
        await pilot.click("#download-now")
        await pilot.pause()
        request_marker = repository.list_sources()[0].path / ".download-now"
        assert request_marker.exists()
        assert app.query_one("#download-now", Button).disabled
        assert table.get_row_at(0)[4] == "queued"
        request_marker.unlink()
        set_busy("tiktok/ui-test")
        app._sync_runtime_busy()
        assert app.query_one("#download-now", Button).disabled
        assert table.get_row_at(0)[4] == "downloading"
        set_busy(None)
        app._sync_runtime_busy()
        assert not app.query_one("#download-now", Button).disabled
        assert table.get_row_at(0)[4] == "active"
        await pilot.press("q")


@pytest.mark.asyncio
async def test_owner_textual_app_detaches_tmux_client(tmp_path: Path, monkeypatch):
    calls: list[tuple[list[str], bool]] = []

    def fake_run(command, *, check):
        calls.append((command, check))
        return SimpleNamespace(returncode=0)

    monkeypatch.setattr("vdl.ui.subprocess.run", fake_run)
    app = VdlApp(_config(tmp_path), owner=True)

    async with app.run_test() as pilot:
        await pilot.click("#detach")
        await pilot.pause()
        assert calls == [
            (["tmux", "-L", "vdl", "detach-client", "-s", "main"], True)
        ]
        await pilot.press("q")


def test_owner_lock_is_process_wide():
    code = (
        "from vdl.runtime import OWNER_LOCK, file_lock; "
        "\ntry:\n"
        " with file_lock(OWNER_LOCK, blocking=False): pass\n"
        "except BlockingIOError:\n raise SystemExit(3)\n"
    )
    with file_lock(OWNER_LOCK):
        result = subprocess.run([sys.executable, "-c", code], check=False)
    assert result.returncode == 3


def test_generated_units_keep_vdl_tmux_and_restart_contract(tmp_path: Path):
    tmux_config = tmp_path / "tmux.conf"
    application = ["/opt/vdl/bin/vdl"]
    _write_tmux_config(tmux_config, application)

    tmux_text = tmux_config.read_text(encoding="utf-8")
    owner_text = owner_unit_text(tmux_config)
    web_text = web_unit_text(application)

    assert "exit-empty on" in tmux_text
    assert "new-session -d -s main" in tmux_text
    assert "exec /opt/vdl/bin/vdl" in tmux_text
    assert "-L vdl" in owner_text
    assert "-D" in owner_text
    assert f"-f {tmux_config}" in owner_text
    assert "Restart=always" in owner_text
    assert "RestartSec=10" in owner_text
    assert "--serve-web" in web_text
    assert "Restart=always" in web_text


def test_install_and_deinstall_preserve_configuration(tmp_path: Path):
    calls: list[list[str]] = []

    def fake_run(command, *, check):
        calls.append(command)
        return SimpleNamespace(returncode=0)

    config = tmp_path / "config" / "config.toml"
    units = tmp_path / "systemd"
    tmux = tmp_path / "config" / "tmux.conf"
    installed = install_services(
        config_path=config,
        unit_dir=units,
        tmux_config=tmux,
        executable=["/opt/vdl/bin/vdl"],
        runner=fake_run,
    )
    original_config = config.read_bytes()

    deinstall_services(unit_dir=units, runner=fake_run)

    assert installed.owner_unit.exists() is False
    assert installed.web_unit.exists() is False
    assert config.read_bytes() == original_config
    assert tmux.exists()
    assert calls == [
        ["systemctl", "--user", "daemon-reload"],
        ["systemctl", "--user", "enable", "--now", "vdl.service", "vdl-web.service"],
        ["systemctl", "--user", "disable", "--now", "vdl.service", "vdl-web.service"],
        ["systemctl", "--user", "daemon-reload"],
    ]
