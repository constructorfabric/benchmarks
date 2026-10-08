"""Fixtures of the mini-chat e2e tests.

The tests start the mock provider and the example server themselves (see
``harness.Stack``); set ``MC_SERVER_BIN`` to use another binary.
"""

import json
import re
import sys
import tempfile
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import harness  # noqa: E402


def _stack(tmp_root, name, **kw):
    s = harness.Stack(Path(tmp_root) / name, **kw)
    try:
        s.start()
    except Exception:
        s.stop()
        raise
    return s


@pytest.fixture(scope="session")
def tmp_root():
    d = tempfile.mkdtemp(prefix="mc-e2e-")
    return d


@pytest.fixture(scope="session")
def stack(tmp_root):
    s = _stack(tmp_root, "default")
    yield s
    s.stop()


@pytest.fixture
def mock(stack):
    stack.mock_reset()
    yield stack
    stack.mock_reset()


@pytest.fixture
def client(stack):
    return harness.Client(stack, "token-a1")


@pytest.fixture
def client_a2(stack):
    return harness.Client(stack, "token-a2")


@pytest.fixture
def client_b1(stack):
    return harness.Client(stack, "token-b1")


@pytest.fixture
def chat(client):
    return client.create_chat(title="test chat")


def log_events(stack, kind):
    """Parse usage / audit events logged by the static plugins."""
    text = (stack.workdir / "server.log").read_text(errors="replace")
    marker = {
        "usage": "usage event published",
        "audit": "turn audit event",
        "mutation": "turn mutation audit event",
    }[kind]
    out = []
    for line in text.splitlines():
        if marker not in line:
            continue
        if kind == "audit" and "mutation" in line:
            continue
        m = re.search(r"event=(\{.*\})", line)
        if m:
            try:
                out.append(json.loads(m.group(1)))
            except ValueError:
                pass
    return out


@pytest.fixture
def logs(stack):
    return lambda kind: log_events(stack, kind)
