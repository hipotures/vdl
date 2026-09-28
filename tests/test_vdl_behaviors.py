"""Original filesystem, CLI, scheduler and runner regression coverage."""
from __future__ import annotations

import asyncio
import json
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest

from vdl.config import config_from_mapping, parse_duration
from vdl.domain import disable_sources, parse_source_numbers
from vdl.install import (_write_tmux_config, deinstall_services, install_services,
                         owner_unit_text, web_unit_text)
from vdl.repository import SourceRepository, derive_source_location
from vdl.runner import YtDlpRunner
from vdl.runtime import OWNER_LOCK, file_lock
from vdl.scheduler import Scheduler, select_due_source, source_is_due


def _config(root: Path, **overrides):
    values = {"download_root": str(root), "check_interval": "24h", "minimum_spacing": "10m",
              "scheduler_poll": "1m", "log_file": str(root / "vdl.log"),
              "web_host": "127.0.0.1", "web_port": 8780,
              "yt_dlp_command": ["yt-dlp", "--download-archive", ".archive"]}
    values.update(overrides)
    return config_from_mapping(values)


def _fake_source(name, last_check, *, active=True, download_requested=False):
    return SimpleNamespace(service="site", account=name, path=Path("/tmp/vdl-test") / name,
                           url=f"https://example.test/{name}", active=active,
                           last_check=last_check, download_requested=download_requested)


def test_url_derivation_and_existing_directory_handling(tmp_path):
    for url, expected in [
        ("https://www.tiktok.com/@foo?tab=videos#top", ("tiktok", "foo")),
        ("https://www.youtube.com/@foo/", ("youtube", "foo")),
        ("https://www.youtube.com/@foo/videos", ("youtube", "foo")),
        ("https://www.instagram.com/foo/", ("instagram", "foo")),
        ("https://video.example.org/@some user/", ("video-example-org", "some-user")),
    ]:
        assert derive_source_location(url) == expected
    repository = SourceRepository(tmp_path)
    first = repository.add_source("https://www.tiktok.com/@foo")
    repeated = repository.add_source("https://www.tiktok.com/@foo")
    unrelated = tmp_path / "instagram" / "reserved"
    unrelated.mkdir(parents=True)
    (unrelated / "keep.txt").write_text("untouched", encoding="utf-8")
    adopted = repository.add_source("https://www.instagram.com/reserved/")
    assert first.created and not repeated.created
    assert repeated.source.account == "foo"
    assert adopted.source.path == unrelated
    assert (unrelated / "keep.txt").read_text() == "untouched"
    assert (unrelated / ".source").read_text() == "https://www.instagram.com/reserved/\n"


def test_filesystem_discovery_distinguishes_active_inactive_and_ignored(tmp_path):
    active, inactive, ignored = [tmp_path / "tiktok" / name for name in ["active", "inactive", "ignored"]]
    for path in (active, inactive, ignored):
        path.mkdir(parents=True)
    (active / ".source").write_text("https://www.tiktok.com/@active\n")
    (inactive / ".source.del").write_text("https://www.tiktok.com/@inactive\n")
    (ignored / "media.mp4").write_bytes(b"data")
    malformed = tmp_path / "youtube" / "malformed"
    malformed.mkdir(parents=True)
    (malformed / ".source").write_bytes(b"https://example.test/\xff")
    sources = SourceRepository(tmp_path).list_sources()
    assert [(s.account, s.active, s.url) for s in sources] == [
        ("active", True, "https://www.tiktok.com/@active"),
        ("inactive", False, "https://www.tiktok.com/@inactive"),
        ("malformed", True, "https://example.test/�")]


def test_add_creates_source_marker_without_a_check_marker(tmp_path):
    result = SourceRepository(tmp_path).add_source("https://www.instagram.com/example/?utm_source=test&tab=posts")
    source = result.source
    assert result.created
    assert source.path == tmp_path / "instagram" / "example"
    assert (source.path / ".source").read_text() == "https://www.instagram.com/example/\n"
    assert not (source.path / ".source.del").exists()
    assert not (source.path / ".last-check").exists()


def test_add_treats_urls_with_different_query_strings_as_the_same_source(tmp_path):
    repository = SourceRepository(tmp_path)
    first = repository.add_source("https://example.test/account?one=1")
    repeated = repository.add_source("https://example.test/account?two=2")
    assert first.created and not repeated.created
    assert repeated.source.url == "https://example.test/account"


def test_disable_renames_only_the_source_marker(tmp_path):
    repository = SourceRepository(tmp_path)
    source = repository.add_source("https://www.youtube.com/@archive-test").source
    media, archive, last_check = [source.path / name for name in ["video.mp4", ".archive", ".last-check"]]
    media.write_bytes(b"media")
    archive.write_text("downloaded-id\n")
    last_check.write_text("marker")
    mtime = last_check.stat().st_mtime_ns
    updated = disable_sources(repository, [source])[0]
    assert not (source.path / ".source").exists()
    assert (source.path / ".source.del").read_text() == "https://www.youtube.com/@archive-test\n"
    assert media.read_bytes() == b"media"
    assert archive.read_text() == "downloaded-id\n"
    assert last_check.read_text() == "marker" and last_check.stat().st_mtime_ns == mtime
    assert not updated.active


def test_source_number_parser_accepts_mixed_delimiters():
    for value in ("2 7 15", "2,7,15", "2;7;15", "2, 7 15", ["2,", "7", "15"]):
        assert parse_source_numbers(value) == [2, 7, 15]


def test_scheduler_due_selection_handles_new_oldest_and_spacing():
    now = 1000.0
    newest, oldest, middle = _fake_source("newest", 900.), _fake_source("oldest", 500.), _fake_source("middle", 650.)
    new, inactive = _fake_source("new", None), _fake_source("inactive", 100., active=False)
    sources = [newest, inactive, middle, new, oldest]
    requested = _fake_source("requested", 999., download_requested=True)
    assert source_is_due(new, now, 10000.)
    assert source_is_due(oldest, now, 200.)
    assert not source_is_due(newest, now, 200.)
    assert select_due_source(sources, now=now, check_interval=200., minimum_spacing=10000.) is new
    assert select_due_source([*sources, requested], now=now, check_interval=10000., minimum_spacing=10000.) is requested
    without_new = [s for s in sources if s is not new]
    assert select_due_source(without_new, now=now, check_interval=200., minimum_spacing=150.) is None
    assert select_due_source(without_new, now=now, check_interval=200., minimum_spacing=100.) is oldest
    disabled = _fake_source("disabled-now", 950., active=False)
    assert select_due_source([oldest, middle, disabled], now=now, check_interval=200., minimum_spacing=100.) is None


def test_duration_parser_supports_documented_units():
    for value, expected in [("30s", 30), ("10m", 600), ("24h", 86400), ("2d", 172800)]:
        assert parse_duration(value) == expected


async def test_runner_uses_configured_argv_cwd_and_touches_marker(tmp_path):
    source = SourceRepository(tmp_path / "downloads").add_source("https://example.test/account").source
    fake = tmp_path / "fake-yt-dlp"
    fake.write_text(
        "#!/usr/bin/env python3\nimport json, os, pathlib, sys\n"
        "pathlib.Path(sys.argv[2]).touch()\n"
        "pathlib.Path('runner-record.json').write_text(json.dumps({"
        "'cwd': os.getcwd(), 'argv': sys.argv[1:], 'marker_at_start': pathlib.Path('.last-check').exists()}))\n"
        "print('fake stdout')\nprint('x' * 70000)\nprint('fake stderr', file=sys.stderr)\n")
    fake.chmod(0o755)
    log_file = tmp_path / "logs" / "vdl.log"
    runner = YtDlpRunner([str(fake), "--download-archive", ".archive"], log_file=log_file)
    assert await runner.run(source) == 0
    record = json.loads((source.path / "runner-record.json").read_text())
    assert record == {"cwd": str(source.path), "argv": ["--download-archive", ".archive", source.url], "marker_at_start": True}
    assert (source.path / ".archive").exists() and (source.path / ".last-check").exists()
    log = log_file.read_text()
    for expected in ["fake stdout", "fake stderr", "x" * 70000, "yt-dlp exit code=0"]:
        assert expected in log


async def test_scheduler_never_runs_two_downloads_at_once(tmp_path):
    source = _fake_source("one", None)
    started, release = asyncio.Event(), asyncio.Event()
    calls, active, max_active = [], 0, 0

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

    config = SimpleNamespace(check_interval=0, minimum_spacing=0, scheduler_poll=1, log_file=tmp_path / "scheduler.log")
    completion = []
    scheduler = Scheduler(Repository(), config, runner=BlockingRunner(), on_complete=lambda *_: completion.append(scheduler.busy))
    first = asyncio.create_task(scheduler.run_once())
    await asyncio.wait_for(started.wait(), 3)
    assert await scheduler.run_once() is None
    release.set()
    assert await first is source
    assert len(calls) == max_active == 1
    assert not scheduler.busy and completion == [False]


async def test_download_now_bypasses_schedule_and_is_consumed(tmp_path):
    repository = SourceRepository(tmp_path / "downloads")
    source = repository.add_source("https://example.test/download-now").source
    (source.path / ".last-check").touch()
    repository.request_download(repository.list_sources()[0])
    calls = []

    class RecordingRunner:
        async def run(self, source):
            calls.append(source)
            return 0

    scheduler = Scheduler(repository, _config(tmp_path, download_root=str(repository.download_root)), runner=RecordingRunner())
    selected = await scheduler.run_once()
    assert selected.path == source.path and len(calls) == 1
    assert not (source.path / ".download-now").exists()


async def test_runner_cancellation_stops_downloader_process_group(tmp_path):
    source = SourceRepository(tmp_path / "downloads").add_source("https://example.test/cancel").source
    child_marker, fake = tmp_path / "child-survived", tmp_path / "fake-tree"
    child = f"import time; from pathlib import Path; time.sleep(1); Path({str(child_marker)!r}).write_text('bad')"
    fake.write_text("#!/usr/bin/env python3\nimport subprocess, sys, time\n"
                    f"subprocess.Popen([sys.executable, '-c', {child!r}])\ntime.sleep(30)\n")
    fake.chmod(0o755)
    task = asyncio.create_task(YtDlpRunner([str(fake)], log_file=tmp_path / "log").run(source))
    await asyncio.sleep(.2)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    await asyncio.sleep(1.1)
    assert not child_marker.exists()
    fake.write_text("#!/usr/bin/env python3\nimport subprocess, sys\n"
                    f"subprocess.Popen([sys.executable, '-c', {child!r}], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n")
    assert await YtDlpRunner([str(fake)], log_file=tmp_path / "log-2").run(source) == 0
    await asyncio.sleep(1.1)
    assert not child_marker.exists()


def test_owner_lock_is_process_wide():
    code = "from vdl.runtime import OWNER_LOCK, file_lock\ntry:\n with file_lock(OWNER_LOCK, blocking=False): pass\nexcept BlockingIOError:\n raise SystemExit(3)\n"
    with file_lock(OWNER_LOCK):
        result = subprocess.run([sys.executable, "-c", code], check=False)
    assert result.returncode == 3


def test_generated_units_keep_vdl_tmux_and_restart_contract(tmp_path):
    tmux = tmp_path / "tmux.conf"
    _write_tmux_config(tmux, ["/opt/vdl/bin/vdl"])
    owner, web = owner_unit_text(tmux), web_unit_text(["/opt/vdl/bin/vdl"])
    assert "exit-empty on" in tmux.read_text() and "new-session -d -s main" in tmux.read_text()
    assert "exec /opt/vdl/bin/vdl" in tmux.read_text()
    for value in ["-L vdl", "-D", f"-f {tmux}", "Restart=always", "RestartSec=10"]:
        assert value in owner
    assert "--serve-web" in web and "Restart=always" in web


def test_install_and_deinstall_preserve_configuration(tmp_path):
    calls = []
    def fake_run(command, *, check):
        calls.append(command)
        return SimpleNamespace(returncode=0)
    config, units, tmux = tmp_path / "config" / "config.toml", tmp_path / "systemd", tmp_path / "config" / "tmux.conf"
    installed = install_services(config_path=config, unit_dir=units, tmux_config=tmux,
                                 executable=["/opt/vdl/bin/vdl"], runner=fake_run)
    original = config.read_bytes()
    deinstall_services(unit_dir=units, runner=fake_run)
    assert not installed.owner_unit.exists() and not installed.web_unit.exists()
    assert config.read_bytes() == original and tmux.exists()
    assert calls == [
        ["systemctl", "--user", "daemon-reload"],
        ["systemctl", "--user", "enable", "--now", "vdl.service", "vdl-web.service"],
        ["systemctl", "--user", "disable", "--now", "vdl.service", "vdl-web.service"],
        ["systemctl", "--user", "daemon-reload"]]
