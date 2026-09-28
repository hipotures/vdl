"""Optional real-browser tests. Run with `uv run --group browser pytest`."""
from __future__ import annotations

from contextlib import ExitStack
import os
from pathlib import Path
import shutil
import re

from aiohttp import ClientSession

from aiohttp.test_utils import TestServer
import pytest

playwright = pytest.importorskip("playwright.async_api")
from playwright.async_api import async_playwright, expect

from vdl.config import Config
from vdl.repository import SourceRepository
from vdl.runtime import DOWNLOAD_LOCK, OWNER_LOCK, file_lock, set_busy
from vdl.web import create_app, ASSETS


def browser_options():
    executable = os.environ.get("VDL_CHROMIUM") or shutil.which("chromium")
    return {"executable_path": executable} if executable else {}


async def load_ui(page, server):
    """Native HTTP by default; optional offline DOM harness in restricted CI.

    The offline harness renders the unchanged CSS and application JavaScript.
    Its fetch shim forwards JSON to the actual aiohttp server using a Python
    test client, without asking the browser to make network connections.
    Clipboard access is simulated there, not asserted as a real OS operation.
    """
    if os.environ.get("VDL_BROWSER_OFFLINE") != "1":
        await page.goto(str(server.make_url("/")))
        return

    async def bridge(path, options):
        assert path in {"/api/state", "/api/sources", "/api/sources/disable", "/api/sources/download-now"}
        async with ClientSession() as client:
            async with client.request(options.get("method", "GET"), server.make_url(path),
                                      headers=options.get("headers"), data=options.get("body")) as response:
                return {"status": response.status, "body": await response.text()}

    await page.expose_function("__vdl_test_request", bridge)
    html = (ASSETS / "index.html").read_text()
    html = re.sub(r'<link[^>]+rel="stylesheet"[^>]*>', "", html)
    html = re.sub(r'<script[^>]*>.*?</script>', "", html)
    await page.set_content(html)
    await page.add_style_tag(content=(ASSETS / "app.css").read_text())
    await page.add_script_tag(content="""
        window.__vdlOffline = false;
        window.fetch = async (path, options = {}) => {
            if (window.__vdlOffline && path === '/api/state') throw new TypeError('Test offline');
            const {signal, ...serializable} = options;
            const result = await window.__vdl_test_request(path, serializable);
            return new Response(result.body, {status: result.status, headers: {'Content-Type': 'application/json'}});
        };
        let clipboard = '';
        Object.defineProperty(window, 'isSecureContext', {value: true, configurable: true});
        Object.defineProperty(navigator, 'clipboard', {value: {
            readText: async () => clipboard,
            writeText: async text => { clipboard = text; }
        }, configurable: true});
    """)
    code = (ASSETS / "url.js").read_text().replace("export function", "function")
    code += "\n" + (ASSETS / "app.js").read_text().replace('import { normalizeURL, age } from "./url.js";', "")
    await page.add_script_tag(content="{\n" + code + "\n}")


async def network_mode(page, offline):
    if os.environ.get("VDL_BROWSER_OFFLINE") == "1":
        await page.evaluate("value => { window.__vdlOffline = value; }", offline)
    elif offline:
        await page.route("**/api/state", lambda route: route.abort())
    else:
        await page.unroute("**/api/state")


@pytest.mark.parametrize("width,height", [(320, 568), (390, 844), (768, 1024), (1280, 900)])
async def test_mobile_first_add_live_updates_and_sticky_composer(tmp_path, width, height):
    cfg = Config(download_root=tmp_path / "downloads", log_file=tmp_path / "web.log", web_host="127.0.0.1")
    repo = SourceRepository(cfg.download_root)
    current = repo.add_source("https://www.youtube.com/@northlight").source
    (current.path / ".last-check").touch()
    for number in range(18):
        source = repo.add_source(f"https://www.instagram.com/studio-{number:02}").source
        (source.path / ".last-check").touch()
    with ExitStack() as locks:
        locks.enter_context(file_lock(OWNER_LOCK))
        locks.enter_context(file_lock(DOWNLOAD_LOCK))
        set_busy(current.id)
        try:
            async with TestServer(create_app(cfg)) as server, async_playwright() as pw:
                browser = await pw.chromium.launch(**browser_options(), args=["--no-sandbox"])
                context = await browser.new_context(viewport={"width": width, "height": height},
                                                    is_mobile=width < 680, has_touch=width < 680)
                page = await context.new_page()
                errors = []
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.on("console", lambda item: errors.append(item.text) if item.type == "error" else None)
                await load_ui(page, server)
                await expect(page.locator("#connection")).to_have_text("Live · 1s")
                assert await page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                assert (await page.locator("#add").bounding_box())["y"] < height
                url = "https://www.tiktok.com/@" + "long-profile-" * 8
                await page.locator("#source-url").fill(url + "?tracking=" + "x" * 1500)
                await expect(page.locator("#url-preview")).to_have_text("Will save: " + url)
                await page.locator("#add").click()
                await expect(page.locator("#message")).to_contain_text("Added tiktok/")
                await expect(page.locator("#count")).to_have_text("20")
                assert len(repo.list_sources()) == 20
                assert all("tracking=" not in item.url for item in repo.list_sources())
                assert await page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                # A never-checked source is Pending, but Download now must still
                # be available. Manual forcing bypasses normal scheduling even
                # while another source is currently downloading.
                pending_source = next(item for item in repo.list_sources() if item.service == "tiktok")
                force = page.locator(f'[data-id="{pending_source.id}"] .download')
                await expect(force).to_be_enabled()
                await force.click()
                await expect(page.locator("#message")).to_contain_text("Download requested")
                assert (pending_source.path / ".download-now").exists()
                # The next poll must not overwrite unfinished input or focus.
                await page.locator("#source-url").fill("https://example.test/unfinished")
                repo.add_source("https://example.test/from-cli")
                await expect(page.locator("#count")).to_have_text("21", timeout=3500)
                await expect(page.locator("#source-url")).to_have_value("https://example.test/unfinished")
                await expect(page.locator("#source-url")).to_be_focused()
                await page.locator("#source-url").fill("")
                if os.environ.get("VDL_SCREENSHOTS") and width in {390, 1280}:
                    destination = Path(os.environ["VDL_SCREENSHOTS"])
                    destination.mkdir(parents=True, exist_ok=True)
                    await page.screenshot(path=str(destination / ("vdl-mobile.png" if width == 390 else "vdl-desktop.png")))
                await page.evaluate("window.scrollTo(0, document.body.scrollHeight)")
                position = await page.locator("#add").bounding_box()
                assert 0 <= position["y"] and position["y"] + position["height"] <= height
                scroll_before = await page.evaluate("scrollY")
                await page.wait_for_timeout(1200)
                assert abs(await page.evaluate("scrollY") - scroll_before) <= 1
                # Simulate the smaller visual area available above a keyboard.
                if width < 680:
                    await page.set_viewport_size({"width": width, "height": 330})
                    position = await page.locator("#add").bounding_box()
                    assert position["y"] + position["height"] <= 330
                    assert await page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                assert errors == []
                await browser.close()
        finally:
            set_busy(None)


async def test_clipboard_confirmation_fallback_and_offline_recovery(tmp_path):
    cfg = Config(download_root=tmp_path / "downloads", log_file=tmp_path / "web.log")
    async with TestServer(create_app(cfg)) as server, async_playwright() as pw:
        browser = await pw.chromium.launch(**browser_options(), args=["--no-sandbox"])
        context = await browser.new_context(viewport={"width": 390, "height": 844},
                                            permissions=["clipboard-read", "clipboard-write"])
        page = await context.new_page()
        await load_ui(page, server)
        await expect(page.locator("#connection")).to_have_text("Live · 1s")
        raw = "https://www.youtube.com/@clipboard?tracking=test"
        await page.evaluate("text => navigator.clipboard.writeText(text)", raw)
        await page.locator("#paste").click()
        await expect(page.locator("#source-url")).to_have_value("https://www.youtube.com/@clipboard")
        assert len(SourceRepository(cfg.download_root).list_sources()) == 0  # Confirmation is required.
        assert await page.evaluate("navigator.clipboard.readText()") == raw  # Never erase the user's clipboard.
        await page.locator("#add").click()
        await expect(page.locator("#count")).to_have_text("1")
        # Simulate the unavailable Clipboard API on ordinary LAN HTTP.
        await page.evaluate("Object.defineProperty(navigator, 'clipboard', {value: undefined, configurable: true})")
        await page.locator("#paste").click()
        await expect(page.locator("#source-url")).to_be_focused()
        await expect(page.locator("#message")).to_contain_text("choose Paste")
        await network_mode(page, True)
        await page.locator("#refresh").click()
        await expect(page.locator("#connection")).to_have_text("Offline · retrying")
        await expect(page.locator("#count")).to_have_text("1")
        await network_mode(page, False)
        await page.locator("#refresh").click()
        await expect(page.locator("#connection")).to_have_text("Live · 1s")
        await browser.close()
