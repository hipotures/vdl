"""Serve the Textual UI in a browser using textual-serve."""

from __future__ import annotations

import shlex
import socket
import sys

from aiohttp.web_request import Request
from textual_serve.server import Server

from .config import Config


class VdlServer(Server):
    """Use the browser's requested host for LAN-safe WebSocket URLs."""

    async def handle_index(self, request: Request):
        self.public_url = f"{request.scheme}://{request.host}"
        return await super().handle_index(request)


def serve(config: Config) -> None:
    command = shlex.join([sys.executable, "-m", "vdl", "--web-client"])
    public_host = socket.gethostname() if config.web_host in {"0.0.0.0", "::"} else config.web_host
    if ":" in public_host and not public_host.startswith("["):
        public_host = f"[{public_host}]"
    public_url = f"http://{public_host}:{config.web_port}"
    VdlServer(
        command,
        host=config.web_host,
        port=config.web_port,
        title="vdl",
        public_url=public_url,
    ).serve()
