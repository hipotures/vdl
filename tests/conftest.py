"""Isolate every test process from the user's live vdl runtime and services."""
import os
import tempfile

_runtime = tempfile.TemporaryDirectory(prefix="vdl-test-runtime-")
os.environ["XDG_RUNTIME_DIR"] = _runtime.name
