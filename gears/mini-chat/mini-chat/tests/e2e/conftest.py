"""mini-chat E2E fixtures: mock provider + example server, both started per
session. The server is stopped by its PID.

Run: ``pytest gears/mini-chat/mini-chat/tests/e2e`` after building
``cf-gears-example-server`` with
``--features mini-chat,static-authn,static-authz,single-tenant,static-credstore``.
"""

from __future__ import annotations

import contextlib
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx
import pytest

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[4]
sys.path.insert(0, str(HERE))

import mc  # noqa: E402


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_http(url: str, headers: dict | None = None, timeout: float = 60.0) -> None:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            r = httpx.get(url, headers=headers or {}, timeout=2)
            if r.status_code < 500:
                return
            last = r.status_code
        except Exception as e:  # noqa: BLE001
            last = e
        time.sleep(0.3)
    raise RuntimeError(f"{url} not ready: {last}")


@contextlib.contextmanager
def start_stack(transform=None):
    """Start the mock provider and the server; `transform(cfg_text)` edits the config."""
    home = Path(tempfile.mkdtemp(prefix="mc-e2e-"))
    api_port = _free_port()
    mock_port = _free_port()
    cfg = (HERE / "config.template.yaml").read_text()
    cfg = cfg.replace("__HOME__", str(home)).replace("__API_PORT__", str(api_port)).replace("__MOCK_PORT__", str(mock_port))
    if transform:
        cfg = transform(cfg)
    cfg_path = home / "config.yaml"
    cfg_path.write_text(cfg)

    mock_log = open(home / "mock.log", "w")
    mock = subprocess.Popen([sys.executable, str(HERE / "mock_llm.py"), str(mock_port)], stdout=mock_log, stderr=subprocess.STDOUT)
    binary = Path(os.environ.get("MINI_CHAT_SERVER_BIN", REPO / "target/debug/cf-gears-example-server"))
    server_log = open(home / "server.out", "w")
    server = subprocess.Popen([str(binary), "--config", str(cfg_path), "run"], stdout=server_log, stderr=subprocess.STDOUT)
    (home / "server.pid").write_text(str(server.pid))
    try:
        _wait_http(f"http://127.0.0.1:{mock_port}/__mock/requests")
        base = f"http://127.0.0.1:{api_port}/mini-chat/v1"
        _wait_http(f"{base}/models", {"Authorization": "Bearer user-a"})
        # Wait for the outbox pipeline / provisioning.
        deadline = time.time() + 30
        while time.time() < deadline:
            if "mini-chat started" in (home / "logs" / "server.log").read_text(errors="ignore"):
                break
            time.sleep(0.3)
        yield mc.Env(base=base, mock=f"http://127.0.0.1:{mock_port}", home=home)
    finally:
        os.kill(server.pid, signal.SIGTERM)
        try:
            server.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.kill(server.pid, signal.SIGKILL)
        os.kill(mock.pid, signal.SIGTERM)
        try:
            mock.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.kill(mock.pid, signal.SIGKILL)


@pytest.fixture(scope="session")
def env():
    with start_stack() as e:
        yield e


@pytest.fixture
def api(env):
    return mc.Client(env, "user-a")


@pytest.fixture
def api_a2(env):
    return mc.Client(env, "user-a2")


@pytest.fixture
def api_b(env):
    return mc.Client(env, "user-b")


@pytest.fixture
def db(env):
    return mc.Db(env)


@pytest.fixture
def mock(env):
    return mc.Mock(env)
