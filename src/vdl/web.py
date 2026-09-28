"""Small HTTP/JSON client of the filesystem repository; never a downloader."""

from __future__ import annotations

import asyncio
from pathlib import Path
from urllib.parse import urlsplit

from aiohttp import web

from .config import Config
from .repository import SourceRepository
from .runner import get_logger
from .state import apply_action, snapshot

ASSETS = Path(__file__).with_name("static")


def create_app(config: Config) -> web.Application:
    repository = SourceRepository(config.download_root)
    logger = get_logger(config.log_file)

    @web.middleware
    async def boundary(request: web.Request, handler):
        try:
            if request.method not in {"GET", "HEAD"}:
                origin = request.headers.get("Origin")
                if origin:
                    parsed = urlsplit(origin)
                    if parsed.scheme not in {"http", "https"} or parsed.netloc.lower() != request.host.lower():
                        raise web.HTTPForbidden(reason="Cross-origin requests are not allowed")
                if request.headers.get("X-VDL-Request") != "1" or request.content_type != "application/json":
                    raise web.HTTPForbidden(reason="A same-origin JSON request is required")
            response = await handler(request)
        except web.HTTPException as exc:
            response = web.json_response({"error": exc.reason}, status=exc.status)
        except FileNotFoundError as exc:
            response = web.json_response({"error": str(exc)}, status=404)
        except FileExistsError as exc:
            response = web.json_response({"error": str(exc)}, status=409)
        except (ValueError, TypeError) as exc:
            response = web.json_response({"error": str(exc)}, status=400)
        except OSError:
            logger.exception("HTTP filesystem operation failed")
            response = web.json_response({"error": "Filesystem operation failed; check the vdl log"}, status=500)
        response.headers.update({
            "Cache-Control": "no-store",
            "X-Content-Type-Options": "nosniff",
            "Referrer-Policy": "no-referrer",
            "Content-Security-Policy": "default-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        })
        return response

    app = web.Application(middlewares=[boundary], client_max_size=16 * 1024)

    async def state(request):
        return web.json_response(await asyncio.to_thread(snapshot, repository))

    async def mutate(request):
        data = await request.json()
        if not isinstance(data, dict):
            raise ValueError("request body must be a JSON object")
        if request.path == "/api/sources":
            result = await asyncio.to_thread(repository.add_source, data.get("url"))
            payload = {"created": result.created, "id": result.source.id,
                       "url": result.source.url, "active": result.source.active}
            status = 201 if result.created else 200
        else:
            await asyncio.to_thread(apply_action, repository, request.match_info["action"], data.get("id"))
            payload, status = {}, 200
        payload["state"] = await asyncio.to_thread(snapshot, repository)
        return web.json_response(payload, status=status)

    def asset(name, content_type):
        async def serve_asset(request):
            content = await asyncio.to_thread((ASSETS / name).read_bytes)
            return web.Response(body=content, content_type=content_type, charset="utf-8")
        return serve_asset

    for route, name, mime in [("/", "index.html", "text/html"),
                              ("/app.css", "app.css", "text/css"),
                              ("/app.js", "app.js", "text/javascript"),
                              ("/url.js", "url.js", "text/javascript")]:
        app.router.add_get(route, asset(name, mime))
    app.router.add_get("/api/state", state)
    app.router.add_post("/api/sources", mutate)
    app.router.add_post("/api/sources/{action:disable|download-now}", mutate)
    return app


def serve(config: Config) -> None:
    web.run_app(create_app(config), host=config.web_host, port=config.web_port,
                access_log=None)
