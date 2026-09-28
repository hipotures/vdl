"""Command-line entry point for vdl."""

from __future__ import annotations

import argparse
import os
import sys

from .config import ConfigError, load_config
from .domain import add_source, disable_sources, format_age, list_sources, resolve_source_numbers
from .install import deinstall_services, install_services
from .repository import Source, SourceRepository, display_service
from .runtime import DOWNLOAD_LOCK, OWNER_LOCK, file_lock, lock_is_held, set_busy


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="vdl")
    parser.add_argument("--no-mouse", action="store_true", help="start the terminal without mouse capture (F2 toggles it)")
    subparsers = parser.add_subparsers(dest="command")
    subparsers.add_parser("attach", help="attach to the persistent tmux session")
    subparsers.add_parser("list", help="list sources")
    add_parser = subparsers.add_parser("add", help="add a source")
    add_parser.add_argument("url")
    del_parser = subparsers.add_parser("del", help="disable sources")
    del_parser.add_argument("ids", nargs="+")
    subparsers.add_parser("install", help="install user services and configuration")
    subparsers.add_parser("deinstall", help="remove vdl user services")
    return parser


def _source_rows(sources: list[Source], numbers: list[int] | None = None) -> list[list[str]]:
    numbers = numbers or list(range(1, len(sources) + 1))
    return [
        [str(number), display_service(source.service), source.account,
         format_age(source.last_check), "active" if source.active else "inactive"]
        for number, source in zip(numbers, sources, strict=True)
    ]


def print_sources(sources: list[Source], numbers: list[int] | None = None) -> None:
    headers = ["#", "Service", "Account", "Last check", "State"]
    rows = _source_rows(sources, numbers)
    widths = [
        max(len(headers[column]), *(len(row[column]) for row in rows))
        for column in range(len(headers))
    ] if rows else [len(header) for header in headers]

    def line(values: list[str]) -> str:
        return "  ".join(value.ljust(widths[index]) for index, value in enumerate(values)).rstrip()

    print(line(headers))
    for row in rows:
        print(line(row))


def _disable(repository: SourceRepository, ids: list[str]) -> int:
    sources = list_sources(repository)
    selected = resolve_source_numbers(sources, ids)
    targets = [source for source in selected if source.active]
    if not targets:
        print("The selected sources are already inactive.")
        return 0
    print("Sources to disable:")
    print_sources(targets, [sources.index(source) + 1 for source in targets])
    try:
        answer = input("Disable these sources? [y/N] ").strip().casefold()
    except EOFError:
        answer = ""
    if answer not in {"y", "yes"}:
        print("No changes made.")
        return 1
    disabled = disable_sources(repository, targets)
    print(f"Disabled {len(disabled)} source(s).")
    return 0


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if arguments == ["--serve-web"]:
        from .web import serve
        try:
            serve(load_config())
            return 0
        except (ConfigError, OSError, ValueError) as exc:
            print(f"vdl: {exc}", file=sys.stderr)
            return 2
    args = _parser().parse_args(arguments)
    try:
        if args.command == "attach":
            os.execvp("tmux", ["tmux", "-L", "vdl", "attach-session", "-t", "main"])
        if args.command == "deinstall":
            deinstall_services()
            return 0
        if args.command == "install":
            install_services()
            return 0
        config = load_config()
        repository = SourceRepository(config.download_root)
        match args.command:
            case None:
                from .ui import VdlApp
                try:
                    with file_lock(OWNER_LOCK, blocking=False):
                        if not lock_is_held(DOWNLOAD_LOCK):
                            set_busy(None)
                        VdlApp(config, owner=True).run(mouse=False if args.no_mouse else None)
                except BlockingIOError as exc:
                    raise RuntimeError("another vdl owner is already running") from exc
            case "list":
                print_sources(list_sources(repository))
            case "add":
                result = add_source(repository, args.url)
                verb = "Added" if result.created else "Already exists"
                print(f"{verb}: {result.source.service}/{result.source.account}")
            case "del":
                return _disable(repository, args.ids)
        return 0
    except (ConfigError, OSError, RuntimeError, ValueError) as exc:
        print(f"vdl: {exc}", file=sys.stderr)
        return 2
