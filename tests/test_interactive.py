"""Concurrency, HTTP, Rich input and mouse regression tests; no live downloads."""
from __future__ import annotations

import asyncio
from dataclasses import replace
import os
import subprocess
import sys
import time

from aiohttp.test_utils import TestClient, TestServer
from prompt_toolkit.data_structures import Point, Size
from prompt_toolkit.input import DummyInput, create_pipe_input
from prompt_toolkit.mouse_events import MouseEvent, MouseEventType, MouseButton
from prompt_toolkit.output import DummyOutput
import pytest

from vdl import cli
from vdl.config import Config
from vdl.repository import SourceRepository, normalize_url
from vdl.runtime import DOWNLOAD_LOCK, OWNER_LOCK, current_download, file_lock, set_busy
from vdl.scheduler import Scheduler, select_due_source
from vdl.state import apply_action, snapshot
from vdl.ui import VdlApp
from vdl.web import create_app


def config(root):
    return Config(download_root=root / "downloads", log_file=root / "vdl.log",
                  web_host="127.0.0.1", scheduler_poll=60)


@pytest.mark.parametrize("value", ["", "ftp://example.test/a", "javascript:alert(1)",
                                  "file:///etc/passwd", "https://", "https://user:pass@example.test/a",
                                  "https://example.test/bad path", "https://example.test:bad/a",
                                  "https://example.test/\x00a", "https://example.test/\\a", "a" * 4097])
def test_reject_invalid_urls(value):
    with pytest.raises(ValueError):
        normalize_url(value)


def test_pending_markers_keep_order_and_survive_repository_restart(tmp_path):
    cfg = config(tmp_path)
    repo = SourceRepository(cfg.download_root)
    z = repo.add_source("https://example.test/z-first").source
    a = repo.add_source("https://example.test/a-second").source
    os.utime(z.path / ".source", (100, 100))
    os.utime(a.path / ".source", (200, 200))
    restarted = SourceRepository(cfg.download_root)
    selected = select_due_source(restarted.list_sources(), now=300, check_interval=999, minimum_spacing=999)
    assert selected.id == z.id
    for source in (z, a):
        (source.path / ".last-check").touch()
        repo.request_download(source)
    marker = z.path / ".download-now"
    os.utime(marker, (100, 100))
    before = marker.stat().st_mtime_ns
    repo.request_download(z)
    assert marker.stat().st_mtime_ns == before
    selected = select_due_source(restarted.list_sources(), now=300, check_interval=999, minimum_spacing=999)
    assert selected.id == z.id


async def test_add_disable_and_http_remain_live_during_download_and_next_starts_immediately(tmp_path):
    cfg = config(tmp_path)
    repo = SourceRepository(cfg.download_root)
    a = repo.add_source("https://example.test/a-running").source
    first_started, release_first, second_started = asyncio.Event(), asyncio.Event(), asyncio.Event()
    calls, active, maximum = [], 0, 0

    class Runner:
        async def run(self, source):
            nonlocal active, maximum
            active += 1; maximum = max(maximum, active); calls.append(source.id)
            (source.path / ".last-check").touch()
            try:
                if source.id == a.id:
                    first_started.set()
                    await release_first.wait()
                else:
                    second_started.set()
                return 0
            finally:
                active -= 1

    scheduler = Scheduler(repo, cfg, runner=Runner())
    with file_lock(OWNER_LOCK):
        scheduler.start()
        async with TestClient(TestServer(create_app(cfg))) as client:
            try:
                await asyncio.wait_for(first_started.wait(), 3)
                response = await asyncio.wait_for(client.post("/api/sources", json={"url": "https://example.test/b-next?tracking=1"}, headers={"X-VDL-Request": "1"}), 2)
                assert response.status == 201
                data = await response.json()
                assert data["state"]["download"] == a.id and data["state"]["pending"] == 1
                assert calls == [a.id]
                other = repo.add_source("https://example.test/c-disable").source
                response = await client.post("/api/sources/disable", json={"id": other.id}, headers={"X-VDL-Request": "1"})
                assert response.status == 200
                repo.request_download(a)
                assert not (a.path / ".download-now").exists()
                # Disabling a running source must neither kill it nor block.
                repo.disable(a)
                assert snapshot(repo)["sources"][0]["state"] == "finishing"
                assert not second_started.is_set()
                # Even another scheduler object cannot start a second worker.
                assert await Scheduler(repo, cfg, runner=Runner()).run_once() is None
                release_first.set()
                await asyncio.wait_for(second_started.wait(), .8)
                assert calls == [a.id, data["id"]] and maximum == 1
            finally:
                release_first.set()
                await scheduler.stop()
    assert current_download() is None
    assert not next(s for s in repo.list_sources() if s.id == other.id).active


async def test_disabling_before_claim_prevents_launch_and_removes_request(tmp_path):
    cfg = config(tmp_path); repo = SourceRepository(cfg.download_root)
    source = repo.add_source("https://example.test/disabled").source
    repo.request_download(source); repo.disable(source)
    class Runner:
        async def run(self, source):
            pytest.fail("disabled source was launched")
    assert await Scheduler(repo, cfg, runner=Runner()).run_once() is None
    assert not (source.path / ".download-now").exists()


def test_process_wide_download_lock_not_only_one_asyncio_loop():
    code = "from vdl.runtime import DOWNLOAD_LOCK, file_lock\ntry:\n with file_lock(DOWNLOAD_LOCK, blocking=False): pass\nexcept BlockingIOError:\n raise SystemExit(7)\n"
    with file_lock(DOWNLOAD_LOCK):
        result = subprocess.run([sys.executable, "-c", code])
    assert result.returncode == 7


def test_stale_busy_file_is_not_reported_as_running(tmp_path):
    set_busy("old/worker")
    try:
        assert snapshot(SourceRepository(tmp_path))["download"] is None
    finally:
        set_busy(None)


async def test_http_validation_no_cache_and_stable_ids(tmp_path):
    cfg = config(tmp_path); repo = SourceRepository(cfg.download_root)
    async with TestClient(TestServer(create_app(cfg))) as client:
        response = await client.get("/")
        assert response.status == 200 and "no-store" in response.headers["Cache-Control"]
        assert "source-url" in await response.text()
        response = await client.post("/api/sources", json={"url": "https://example.test/b"})
        assert response.status == 403
        response = await client.post("/api/sources", json={"url": "https://example.test/b"},
                                     headers={"X-VDL-Request": "1", "Origin": "https://evil.test"})
        assert response.status == 403
        headers = {"X-VDL-Request": "1"}
        for body in [[], {"url": "ftp://example.test/a"}, {"url": 42}]:
            assert (await client.post("/api/sources", json=body, headers=headers)).status == 400
        assert (await client.post("/api/sources", data="{broken", headers={**headers, "Content-Type": "application/json"})).status == 400
        assert (await client.post("/api/sources", json={"url": "a" * 20000}, headers=headers)).status == 413
        first = await client.post("/api/sources", json={"url": "https://example.test/b?x=1"}, headers=headers)
        result = await first.json()
        second = await client.post("/api/sources", json={"url": "https://example.test/b?y=2"}, headers=headers)
        assert first.status == 201 and second.status == 200
        assert result["url"] == "https://example.test/b"
        repo.add_source("https://example.test/a")  # Changes displayed row numbers.
        assert (await client.post("/api/sources/disable", json={"id": result["id"]}, headers=headers)).status == 200
        sources = {s.account: s for s in repo.list_sources()}
        assert sources["a"].active and not sources["b"].active
        assert (await client.post("/api/sources/disable", json={"id": "../../etc/passwd"}, headers=headers)).status == 404
        assert (await client.get("/api/state")).headers["Cache-Control"] == "no-store"
        assert (await client.get("/../../etc/passwd")).status == 404


async def test_terminal_mouse_selection_and_live_refresh_keep_identity(tmp_path):
    cfg = config(tmp_path); repo = SourceRepository(cfg.download_root)
    b = repo.add_source("https://example.test/b").source
    c = repo.add_source("https://example.test/c").source
    app = VdlApp(cfg, owner=False, input=DummyInput(), output=DummyOutput())
    await app.refresh()
    app.table.create_content(90, 20)
    y = next(y for y, key in app.table.line_ids.items() if key == c.id)
    event = MouseEvent(Point(15, y), MouseEventType.MOUSE_UP, MouseButton.LEFT, frozenset())
    assert app.table.mouse_handler(event) is None
    assert app.selected == c.id
    app.ask_disable()
    repo.add_source("https://example.test/a")
    await app.refresh()
    assert app.selected == c.id and app.confirm_id == c.id
    await app.mutate("disable", app.confirm_id)
    assert next(s for s in repo.list_sources() if s.id == b.id).active
    assert not next(s for s in repo.list_sources() if s.id == c.id).active
    with file_lock(DOWNLOAD_LOCK):
        set_busy(b.id)
        try:
            app.url.text = "https://example.test/new?tracker=1"
            await app._add(app.url.text)
            assert any(row["account"] == "new" for row in app.rows)
            assert app.state["download"] == b.id
        finally:
            set_busy(None)


async def test_terminal_input_and_mouse_modes_are_restored_on_exit(tmp_path):
    class Output(DummyOutput):
        def __init__(self): self.events = []
        def get_size(self): return Size(rows=24, columns=100)
        def enable_mouse_support(self): self.events.append("mouse-on")
        def disable_mouse_support(self): self.events.append("mouse-off")
        def enter_alternate_screen(self): self.events.append("screen-on")
        def quit_alternate_screen(self): self.events.append("screen-off")
    output = Output()
    with create_pipe_input() as pipe:
        app = VdlApp(config(tmp_path), owner=False, input=pipe, output=output)
        task = asyncio.create_task(app.run_async())
        try:
            await asyncio.sleep(.15)
            pipe.send_text("https://example.test/typed?tracking=1\r")
            for _ in range(30):
                if app.rows: break
                await asyncio.sleep(.05)
            assert app.rows[0]["url"] == "https://example.test/typed"
            # SGR mouse: click Clear URL (second button on row four).
            app.url.text = "draft"
            pipe.send_text("\x1b[<0;18;4M\x1b[<0;18;4m")
            await asyncio.sleep(.15)
            assert app.url.text == ""
            pipe.send_text("\x11")  # Ctrl-Q.
            await asyncio.wait_for(task, 2)
        finally:
            if not task.done():
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
    assert "mouse-on" in output.events
    assert output.events[-1] == "mouse-off" or "mouse-off" in output.events[-3:]
    assert "screen-on" in output.events and "screen-off" in output.events


async def test_terminal_detach_uses_existing_tmux_server(tmp_path, monkeypatch):
    calls = []
    monkeypatch.setattr("vdl.ui.subprocess.run", lambda command, check: calls.append((command, check)))
    app = VdlApp(config(tmp_path), owner=False, input=DummyInput(), output=DummyOutput())
    await app.detach()
    assert calls == [(["tmux", "-L", "vdl", "detach-client", "-s", "main"], True)]


def test_attach_builds_interactive_tui_session(tmp_path, monkeypatch):
    tmux_config = tmp_path / "tmux.conf"
    tmux_config.write_text("set-option -g exit-empty on\n")
    monkeypatch.setattr(cli, "TMUX_CONFIG_PATH", tmux_config)
    monkeypatch.setattr(cli, "_vdl_command", lambda: ["/opt/vdl/bin/vdl"])
    argv = cli._attach_argv()
    assert argv[:6] == ["tmux", "-L", "vdl", "-f", str(tmux_config), "new-session"]
    assert argv[6:10] == ["-A", "-s", "main", "/opt/vdl/bin/vdl --tui-client"]


def test_cli_command_contract(tmp_path, monkeypatch, capsys):
    cfg = config(tmp_path)
    monkeypatch.setattr(cli, "load_config", lambda: cfg)
    assert cli.main(["add", "https://example.test/a?tracking=1"]) == 0
    assert "Added: example-test/a" in capsys.readouterr().out
    assert cli.main(["list"]) == 0 and "Account" in capsys.readouterr().out
    monkeypatch.setattr("builtins.input", lambda _: "y")
    assert cli.main(["del", "1"]) == 0
    assert not SourceRepository(cfg.download_root).list_sources()[0].active


async def test_downloader_inherits_lock_even_if_owner_is_killed(tmp_path):
    import signal
    cfg = config(tmp_path)
    SourceRepository(cfg.download_root).add_source("https://example.test/crash-test")
    pidfile = tmp_path / "downloader.pid"
    fake = tmp_path / "fake.py"
    fake.write_text("import os, time\nfrom pathlib import Path\n"
                    f"Path({str(pidfile)!r}).write_text(str(os.getpid()))\n"
                    "time.sleep(30)\n")
    code = (
        "import asyncio\nfrom pathlib import Path\n"
        "from vdl.config import Config\nfrom vdl.repository import SourceRepository\n"
        "from vdl.scheduler import Scheduler\n"
        f"cfg=Config(download_root=Path({str(cfg.download_root)!r}), "
        f"log_file=Path({str(cfg.log_file)!r}), yt_dlp_command=({sys.executable!r}, {str(fake)!r}))\n"
        "asyncio.run(Scheduler(SourceRepository(cfg.download_root), cfg).run_once())\n"
    )
    parent = subprocess.Popen([sys.executable, "-c", code])
    child_pid = None
    try:
        for _ in range(60):
            if pidfile.exists(): break
            await asyncio.sleep(.05)
        assert pidfile.exists()
        child_pid = int(pidfile.read_text())
        parent.kill()
        await asyncio.to_thread(parent.wait, timeout=2)
        assert current_download() == "example-test/crash-test"
        # A fresh scheduler cannot launch anything until the old child exits.
        assert await Scheduler(SourceRepository(cfg.download_root), cfg).run_once() is None
    finally:
        if parent.poll() is None:
            parent.kill()
            await asyncio.to_thread(parent.wait)
        if child_pid is not None:
            try:
                os.killpg(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            for _ in range(60):
                if current_download() is None: break
                await asyncio.sleep(.05)
        set_busy(None)
    assert current_download() is None
