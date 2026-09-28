# Validation of the Rich and mobile web redesign

## Executed checks

The redesign was tested with Python 3.13.5, Rich 15.0.0,
prompt_toolkit 3.0.52, aiohttp 3.13.3, pytest 9.0.2,
pytest-asyncio 1.3.0, Playwright 1.57.0, Chromium 144.0.7559.96,
and Node.js 22.16.0.

```console
PYTHONPATH=src VDL_BROWSER_OFFLINE=1 VDL_SCREENSHOTS=/mnt/data pytest
node --test tests/test_web.mjs
```

Result: **42 Python tests passed; 3 JavaScript tests passed**.

Coverage includes existing source naming, URL normalization, archive/media
preservation, recurring scheduling policy, CLI commands, and generated
systemd/tmux configuration. Runner tests use real subprocesses to check argv,
working directory, log handling, attempt markers, and process-group cleanup.

Concurrency checks cover adding through the real HTTP API during an active
job, disabling waiting and running sources, duplicate requests, immediate
handoff to the next source, and exclusion between separate schedulers and
processes. A crash test kills the owner while its fake downloader is alive and
verifies that the inherited download lock still prevents another launch.

Terminal tests exercise Rich row hit-testing and stable selection. A
prompt_toolkit input-pipe test sends actual SGR mouse reports, clicks a button,
types a URL, submits it, and checks that mouse reporting and alternate-screen
state are restored on exit.

Browser checks cover 320x568, 390x844, 768x1024, and 1280x900 viewports: no
horizontal page overflow, visible URL actions, sticky composer after scrolling,
long pasted tracking URLs, immediate add feedback, external source changes,
focus/draft/scroll preservation, a reduced-height viewport, clipboard success
and denial paths, and offline recovery. The screenshots use test data, not real
user downloads.

## Browser network limitation

The available Chromium environment blocks HTTP navigation by administrator
policy, including loopback. No policy was changed. With
`VDL_BROWSER_OFFLINE=1`, the test loads the actual packaged HTML/CSS/JavaScript
into a browser document and bridges its fetch calls through a Python HTTP
client to a real local aiohttp test server. This tests the actual DOM and API,
but is **not** a native browser-to-server network or real-device test.

The clipboard is simulated in that harness. It verifies application behavior,
not actual mobile OS clipboard permissions. Test the normal HTTP paste fallback
and HTTPS clipboard permission prompt on the deployment phone before merging.
Native iOS/Safari, the user's terminal/SSH/tmux combination, real yt-dlp network
downloads, and live systemd installation were not exercised here.

## Running the normal browser tests

With package-registry access and a browser environment that permits loopback:

```console
uv sync --group dev --group browser
uv run playwright install chromium
uv run --group dev --group browser pytest
node --test tests/test_web.mjs
```

The browser tests use native browser navigation by default. A system Chromium
at `/usr/bin/chromium` is used when available; otherwise Playwright's installed
Chromium is used. Do not set `VDL_BROWSER_OFFLINE` for native network coverage.

## Dependency resolution

Registry access was unavailable in the execution environment. Installed
compatible dependencies were used for testing; a clean wheel build, fresh
installation, and dependency lock regeneration were not executed. The old
Textual lockfile is deliberately removed instead of claiming it matches the
new dependencies. Regenerate and review `uv.lock` with `uv lock` on a connected
machine. No dependency hashes or resolved versions were fabricated.
