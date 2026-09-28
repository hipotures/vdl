"""The shared terminal and browser Textual interface."""

from __future__ import annotations

from textual.app import App, ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import Button, DataTable, Input, Label, Static

from .config import Config
from .domain import add_source, disable_sources, format_age, list_sources
from .repository import Source, SourceRepository, display_service
from .runtime import read_busy
from .scheduler import Scheduler


class AddSourceScreen(ModalScreen[str | None]):
    """Ask for a source URL."""

    CSS = """
    AddSourceScreen { align: center middle; }
    AddSourceScreen > Vertical { width: 72; height: auto; padding: 1 2; background: $panel; }
    AddSourceScreen Horizontal { height: auto; margin-top: 1; align-horizontal: right; }
    AddSourceScreen Button { margin-left: 1; }
    """

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Label("Source URL")
            yield Input(placeholder="https://…", id="url")
            with Horizontal():
                yield Button("Cancel", id="cancel")
                yield Button("Add", variant="primary", id="submit")

    def on_mount(self) -> None:
        self.query_one(Input).focus()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        self.dismiss(event.value.strip() or None)

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "submit":
            value = self.query_one(Input).value.strip()
            self.dismiss(value or None)
        else:
            self.dismiss(None)


class ConfirmDisableScreen(ModalScreen[bool]):
    """Confirm disabling one selected source."""

    CSS = """
    ConfirmDisableScreen { align: center middle; }
    ConfirmDisableScreen > Vertical { width: 64; height: auto; padding: 1 2; background: $panel; }
    ConfirmDisableScreen Horizontal { height: auto; margin-top: 1; align-horizontal: right; }
    ConfirmDisableScreen Button { margin-left: 1; }
    """

    def __init__(self, source: Source) -> None:
        super().__init__()
        self.source = source

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Label(
                f"Disable {display_service(self.source.service)} / "
                f"{self.source.account}?"
            )
            with Horizontal():
                yield Button("Cancel", id="cancel")
                yield Button("Disable", variant="warning", id="disable")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "disable")


class VdlApp(App[None]):
    """Compact source list shared by the owner and client-only processes."""

    TITLE = "vdl"
    CSS = """
    Screen { layout: vertical; }
    #title { height: 1; padding-left: 1; text-style: bold; }
    #sources { height: 1fr; }
    #status { height: 1; padding-left: 1; color: $text-muted; }
    #actions { height: 3; align-horizontal: right; padding-right: 1; }
    #actions Button { min-width: 12; margin-left: 1; }
    """
    BINDINGS = [("q", "quit", "Quit"), ("r", "refresh_sources", "Refresh")]

    def __init__(self, config: Config, *, owner: bool) -> None:
        super().__init__()
        self.config = config
        self.owner = owner
        self.repository = SourceRepository(config.download_root)
        self.sources: list[Source] = []
        self.scheduler: Scheduler | None = None

    def compose(self) -> ComposeResult:
        yield Static("vdl sources", id="title")
        yield DataTable(id="sources", cursor_type="row", zebra_stripes=True)
        yield Static("Idle", id="status")
        with Horizontal(id="actions"):
            yield Button("Add", id="add", variant="primary")
            yield Button("Disable", id="disable", disabled=True)
            yield Button("Refresh", id="refresh")
            yield Button("Quit", id="quit")

    def on_mount(self) -> None:
        self.refresh_sources()
        if self.owner:
            self.scheduler = Scheduler(
                self.repository,
                self.config,
                on_start=self._download_started,
                on_complete=self._download_completed,
            )
            self.scheduler.start()
        else:
            self._sync_runtime_busy()
            self.set_interval(1.0, self._sync_runtime_busy)

    async def on_unmount(self) -> None:
        if self.scheduler is not None:
            await self.scheduler.stop()

    def refresh_sources(self) -> None:
        """Reload the table from the filesystem."""

        table = self.query_one("#sources", DataTable)
        table.clear(columns=True)
        table.add_columns("#", "Service", "Account", "Last check", "State")
        self.sources = list_sources(self.repository)
        for number, source in enumerate(self.sources, 1):
            table.add_row(
                str(number),
                display_service(source.service),
                source.account,
                format_age(source.last_check),
                "active" if source.active else "inactive",
                key=str(source.path),
            )
        self._update_mutation_buttons()

    def _selected_source(self) -> Source | None:
        table = self.query_one("#sources", DataTable)
        if not self.sources or table.cursor_row < 0 or table.cursor_row >= len(self.sources):
            return None
        return self.sources[table.cursor_row]

    def _update_mutation_buttons(self) -> None:
        source = self._selected_source()
        busy = self._worker_busy()
        self.query_one("#add", Button).disabled = busy
        self.query_one("#disable", Button).disabled = busy or source is None or not source.active

    def _worker_busy(self) -> bool:
        return bool(read_busy()) if not self.owner else bool(
            self.scheduler is not None and self.scheduler.busy
        )

    def on_data_table_row_highlighted(self, _event: DataTable.RowHighlighted) -> None:
        self._update_mutation_buttons()

    def _sync_runtime_busy(self) -> None:
        label = read_busy()
        self.query_one("#status", Static).update(
            f"Downloading {label}" if label else "Idle"
        )
        self._update_mutation_buttons()

    def on_button_pressed(self, event: Button.Pressed) -> None:
        match event.button.id:
            case "add":
                self.push_screen(AddSourceScreen(), self._add_from_screen)
            case "disable":
                source = self._selected_source()
                if source is not None and source.active:
                    self.push_screen(
                        ConfirmDisableScreen(source),
                        lambda confirmed: self._disable_from_screen(source, confirmed),
                    )
            case "refresh":
                self.refresh_sources()
            case "quit":
                self.exit()

    def _add_from_screen(self, url: str | None) -> None:
        if url is None:
            return
        if self._worker_busy():
            self.notify("A download is running; try again when it finishes", severity="warning")
            return
        try:
            result = add_source(self.repository, url)
        except (OSError, ValueError) as exc:
            self.notify(str(exc), severity="error")
            return
        self.refresh_sources()
        message = "Source added" if result.created else "Source already exists"
        self.notify(f"{message}: {result.source.service}/{result.source.account}")

    def _disable_from_screen(self, source: Source, confirmed: bool) -> None:
        if not confirmed:
            return
        if self._worker_busy():
            self.notify("A download is running; try again when it finishes", severity="warning")
            return
        try:
            disable_sources(self.repository, [source])
        except OSError as exc:
            self.notify(str(exc), severity="error")
        self.refresh_sources()

    async def _download_started(self, source: Source) -> None:
        self.query_one("#status", Static).update(
            f"Downloading {display_service(source.service)} / {source.account}"
        )
        self.query_one("#add", Button).disabled = True
        self.query_one("#disable", Button).disabled = True

    async def _download_completed(self, _source: Source, _exit_code: int | None) -> None:
        self.query_one("#status", Static).update("Idle")
        self.query_one("#add", Button).disabled = False
        self.refresh_sources()

    def action_refresh_sources(self) -> None:
        self.refresh_sources()
