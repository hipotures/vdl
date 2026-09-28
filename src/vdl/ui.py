"""Rich rendering with prompt_toolkit input, mouse events and terminal cleanup.

Only prompt_toolkit owns the terminal. A competing Rich.Live refresh thread
would corrupt the input line, mouse coordinates and alternate screen.
"""

from __future__ import annotations

import asyncio
import io
import os
import subprocess

from prompt_toolkit import Application
from prompt_toolkit.data_structures import Point
from prompt_toolkit.filters import Condition
from prompt_toolkit.key_binding import KeyBindings
from prompt_toolkit.layout import HSplit, VSplit, Window, Layout, ConditionalContainer
from prompt_toolkit.layout.controls import UIControl, UIContent
from prompt_toolkit.layout.margins import ScrollbarMargin
from prompt_toolkit.mouse_events import MouseButton, MouseEventType
from prompt_toolkit.styles import Style
from prompt_toolkit.widgets import Button, Label, TextArea
from rich import box
from rich.console import Console
from rich.style import Style as RichStyle
from rich.table import Table
from rich.text import Text

from .config import Config
from .domain import format_age
from .repository import SourceRepository, normalize_url
from .scheduler import Scheduler
from .state import apply_action, snapshot


def terminal_text(value: str) -> str:
    return "".join(char if char.isprintable() else " " for char in value)


def fragment(segment) -> tuple[str, str]:
    """Convert Rich's rendered segment styles, not ANSI escape strings."""
    style, parts = segment.style, []
    if style:
        for color, prefix in [(style.color, "fg:"), (style.bgcolor, "bg:")]:
            if color is not None:
                parts.append(prefix + color.get_truecolor().hex)
        for attr in ("bold", "italic", "underline", "reverse"):
            if getattr(style, attr):
                parts.append(attr)
    return " ".join(parts), segment.text


class SourceControl(UIControl):
    def __init__(self, owner: VdlApp):
        self.owner = owner
        self.line_ids: dict[int, str] = {}

    def is_focusable(self):
        return True

    def create_content(self, width, height):
        console = Console(width=max(1, width), file=io.StringIO(), force_terminal=True)
        table = Table(box=box.SIMPLE_HEAD, expand=True, show_edge=False, pad_edge=False)
        table.add_column("#", width=4, no_wrap=True)
        table.add_column("Source", ratio=1, overflow="ellipsis", no_wrap=True)
        if width >= 72:
            table.add_column("Last attempt", width=14, no_wrap=True)
        table.add_column("State", width=12, no_wrap=True)
        for number, row in enumerate(self.owner.rows, 1):
            cells = [str(number), terminal_text(f"{row['service']} / {row['account']}")]
            if width >= 72:
                cells.append(format_age(row["last_check"]))
            cells.append(row["state"])
            selected = row["id"] == self.owner.selected
            style = RichStyle(bold=selected, reverse=selected, meta={"source_id": row["id"]})
            table.add_row(*(Text(cell) for cell in cells), style=style)
        rendered = console.render_lines(table, console.options, pad=True)
        lines, self.line_ids = [], {}
        for index, line in enumerate(rendered):
            lines.append([fragment(segment) for segment in line if not segment.control])
            for segment in line:
                key = segment.style.meta.get("source_id") if segment.style else None
                if key:
                    self.line_ids[index] = key
                    break
        if not lines:
            lines = [[("", "No sources. Paste a URL above.")]]
        cursor = next((index for index, key in self.line_ids.items() if key == self.owner.selected), 0)
        return UIContent(get_line=lambda index: lines[index], line_count=len(lines),
                         cursor_position=Point(0, cursor), show_cursor=False)

    def mouse_handler(self, event):
        if event.event_type in {MouseEventType.SCROLL_UP, MouseEventType.SCROLL_DOWN}:
            self.owner.move(-3 if event.event_type == MouseEventType.SCROLL_UP else 3)
            return None
        if event.event_type == MouseEventType.MOUSE_UP and event.button == MouseButton.LEFT:
            key = self.line_ids.get(event.position.y)
            if key is not None:
                self.owner.selected = key
                self.owner.application.layout.focus(self)
                self.owner.application.invalidate()
                return None
        return NotImplemented


class VdlApp:
    """A single owner TUI; HTTP sessions never instantiate this class."""

    def __init__(self, config: Config, *, owner: bool = True, input=None, output=None):
        self.config, self.owner = config, owner
        self.repository = SourceRepository(config.download_root)
        self.state = {"sources": [], "download": None, "pending": 0, "owner_running": owner}
        self.selected: str | None = None
        self.confirm_id: str | None = None
        self.message = "Add sources at any time. F2 toggles mouse capture for text selection."
        self.mouse_enabled = os.environ.get("VDL_MOUSE", "1") != "0"
        self.adding = False
        self.pending: set[str] = set()
        self._refresh_lock = asyncio.Lock()
        self.scheduler = Scheduler(self.repository, config, on_start=self._job_changed,
                                   on_complete=self._job_changed) if owner else None
        self.url = TextArea(multiline=False, height=1, prompt="URL: ",
                            accept_handler=lambda _: self.submit())
        self.table = SourceControl(self)
        self.table_window = Window(self.table, wrap_lines=False,
                                   right_margins=[ScrollbarMargin(display_arrows=True)])
        controls = HSplit([
            VSplit([Button("Add", self.submit, width=12),
                    Button("Clear URL", self.clear_url, width=14),
                    Button("Refresh", lambda: self.spawn(self.refresh()), width=12)], padding=1),
            VSplit([Button("Download now", self.download, width=16),
                    Button("Disable", self.ask_disable, width=12),
                    Button("Detach", lambda: self.spawn(self.detach()), width=12),
                    Button("Quit", self.quit, width=10)], padding=1),
        ])
        confirmation = ConditionalContainer(HSplit([
            Label(lambda: terminal_text(f"Disable {self.confirm_id}? A running download will finish; files are kept.")),
            VSplit([Button("Confirm", self.confirm_disable, width=12),
                    Button("Cancel", self.cancel_disable, width=12)], padding=1),
        ]), filter=Condition(lambda: self.confirm_id is not None))
        root = HSplit([
            Label(self.status, style="class:title"), self.url,
            Label(self.preview, style="class:muted"), controls, confirmation,
            self.table_window,
            Label(lambda: terminal_text(self.message), style="class:message"),
            Label(lambda: f"Tab: focus  Arrows: select  Ctrl-Q: quit  F6: detach  F2: mouse {'ON' if self.mouse_enabled else 'OFF'}", style="class:muted"),
        ])
        self.application = Application(
            layout=Layout(root, focused_element=self.url), full_screen=True,
            mouse_support=Condition(lambda: self.mouse_enabled),
            key_bindings=self.bindings(), refresh_interval=1.0,
            style=Style.from_dict({"title": "bold", "muted": "fg:#8899aa",
                                  "message": "fg:#66bbee", "button.focused": "reverse"}),
            input=input, output=output,
        )

    @property
    def rows(self):
        return self.state["sources"]

    def status(self):
        busy = self.state["download"]
        status = f"Downloading {busy}" if busy else "Ready" if self.state["owner_running"] else "Downloader stopped"
        return terminal_text(f" vdl  |  {status}  |  {self.state['pending']} pending")

    def preview(self):
        try:
            return terminal_text(f"Will save: {normalize_url(self.url.text)}")
        except ValueError as error:
            return str(error) if self.url.text else "Paste a profile/channel URL. Query parameters will be removed."

    def bindings(self):
        keys = KeyBindings()
        not_typing = Condition(lambda: not self.application.layout.has_focus(self.url))
        for key, callback in [("tab", lambda: self.application.layout.focus_next()),
                              ("s-tab", lambda: self.application.layout.focus_previous()),
                              ("c-q", self.quit), ("c-c", self.quit),
                              ("f2", self.toggle_mouse), ("f6", lambda: self.spawn(self.detach()))]:
            keys.add(key)(lambda event, callback=callback: callback())
        for key, callback in [("q", self.quit), ("a", lambda: self.application.layout.focus(self.url)),
                              ("n", self.download), ("d", self.ask_disable),
                              ("r", lambda: self.spawn(self.refresh()))]:
            keys.add(key, filter=not_typing)(lambda event, callback=callback: callback())
        for key, delta in [("up", -1), ("down", 1), ("pageup", -10), ("pagedown", 10)]:
            keys.add(key, filter=Condition(lambda: self.application.layout.has_focus(self.table)))(
                lambda event, delta=delta: self.move(delta))
        keys.add("escape")(lambda event: self.cancel_disable())
        return keys

    def spawn(self, coroutine):
        return self.application.create_background_task(coroutine)

    def move(self, delta):
        ids = [row["id"] for row in self.rows]
        if ids:
            index = ids.index(self.selected) if self.selected in ids else 0
            self.selected = ids[min(len(ids) - 1, max(0, index + delta))]
            self.application.invalidate()

    def toggle_mouse(self):
        self.mouse_enabled = not self.mouse_enabled
        self.application.invalidate()

    def quit(self):
        self.application.exit()

    def clear_url(self):
        self.url.text = ""
        self.application.layout.focus(self.url)

    def submit(self):
        if not self.adding:
            self.adding = True
            self.spawn(self._add(self.url.text))
        return True  # Keep input until the asynchronous operation succeeds.

    async def _add(self, raw):
        try:
            result = await asyncio.to_thread(self.repository.add_source, raw)
            verb = "Added" if result.created else "Already exists"
            self.message = f"{verb}: {result.source.id}" + (" (inactive)" if not result.source.active else "")
            if self.url.text == raw:
                self.url.text = ""
            if self.scheduler:
                self.scheduler.wake()
            await self.refresh()
        except (OSError, ValueError) as error:
            self.message = str(error)
        finally:
            self.adding = False
            self.application.invalidate()

    def download(self):
        if self.selected:
            self.spawn(self.mutate("download-now", self.selected))

    def ask_disable(self):
        if any(row["id"] == self.selected and row["active"] for row in self.rows):
            self.confirm_id = self.selected
            self.application.invalidate()

    def cancel_disable(self):
        self.confirm_id = None
        self.application.layout.focus(self.table)
        self.application.invalidate()

    def confirm_disable(self):
        source_id = self.confirm_id
        self.cancel_disable()
        if source_id:
            self.spawn(self.mutate("disable", source_id))

    async def mutate(self, action, source_id):
        if source_id in self.pending:
            return
        self.pending.add(source_id)
        try:
            await asyncio.to_thread(apply_action, self.repository, action, source_id)
            self.message = f"{'Disabled' if action == 'disable' else 'Download requested'}: {source_id}"
            if self.scheduler:
                self.scheduler.wake()
            await self.refresh()
        except (OSError, ValueError) as error:
            self.message = str(error)
        finally:
            self.pending.discard(source_id)
            self.application.invalidate()

    async def detach(self):
        try:
            await asyncio.to_thread(subprocess.run,
                                    ["tmux", "-L", "vdl", "detach-client", "-s", "main"], check=True)
        except (OSError, subprocess.CalledProcessError) as error:
            self.message = f"Could not detach: {error}"
            self.application.invalidate()

    async def refresh(self):
        async with self._refresh_lock:
            self.state = await asyncio.to_thread(snapshot, self.repository)
            if not any(row["id"] == self.selected for row in self.rows):
                self.selected = self.rows[0]["id"] if self.rows else None
            self.application.invalidate()

    async def _job_changed(self, *_):
        await self.refresh()

    async def _poll(self):
        while True:
            await asyncio.sleep(1)
            try:
                await self.refresh()
            except OSError as error:
                self.message = f"Refresh failed: {error}"
                self.application.invalidate()

    async def run_async(self):
        await self.refresh()
        if self.scheduler:
            self.scheduler.start()
        poll = asyncio.create_task(self._poll())
        try:
            await self.application.run_async()
        finally:
            poll.cancel()
            await asyncio.gather(poll, return_exceptions=True)
            if self.scheduler:
                await self.scheduler.stop()

    def run(self, *, mouse: bool | None = None):
        if mouse is not None:
            self.mouse_enabled = mouse
        asyncio.run(self.run_async())
