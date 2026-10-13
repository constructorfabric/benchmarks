"""Fixtures of the mini-chat black-box suite (`launcher: pytest`, self-managed).

One session-scoped stack per run:

1. a scriptable mock OpenAI-compatible provider on a free loopback port
   (``mock_provider``);
2. a generated server config (temp ``home_dir``, SQLite, api-gateway with auth
   on, ``static_tokens`` authn, static authz, single-tenant resolver, static
   credstore, OAGW with plain-HTTP upstreams allowed and SSRF checks off,
   ``mini-chat`` with ``providers.openai`` pointing at the mock, the bundled
   static model-policy / audit plugins);
3. the example server started from that config (``server``), stopped by pid at
   teardown - never by process-name matching.

Fixtures: ``server``, ``base_url``, ``api`` (client relative to the gear URL
prefix), ``mock_provider`` (reset before every test), ``stack_factory`` (extra
servers with their own mock provider and config overrides, e.g. a kill switch or a
tiny-context catalog; cached per name for the session), ``auth_headers`` /
``headers_a`` / ``headers_a_reviewer`` / ``headers_b``, ``mini_chat_db_path`` and
``mini_chat_db`` / ``seed`` (read-only / read-write views of the gear database).

Environment:

* ``E2E_BINARY``    - server binary to use (set by ``tools/scripts/run_e2e.py``);
                      otherwise ``target/debug/cf-gears-example-server``.
* ``E2E_SKIP_BUILD`` - never run ``cargo build``; skip the suite if the binary is missing.
* ``E2E_KEEP_WORKDIR`` - keep the temp dir (config, server log, SQLite files) after the run.
"""

from __future__ import annotations

import copy
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, List, Optional

import httpx
import pytest

from . import catalog
from .attachment_support import AttachmentSeeder, killed_overrides, small_overrides
from .mock_openai_provider import MockOpenAIProvider
from .support import (
    OPENAI_SECRET_KEY,
    OPENAI_SECRET_VALUE,
    TENANT_A_ID,
    TENANT_B_ID,
    TOKENS,
    URL_PREFIX,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    USER_B_ID,
    GearDb,
    GearSeeder,
    MiniChatApi,
    bearer,
)

SUITE_DIR = Path(__file__).resolve().parent
REPO_ROOT = SUITE_DIR.parents[3]  # suites/mini_chat_local -> suites -> e2e -> testing -> repo

BINARY_NAME = "cf-gears-example-server"
BUILD_COMMAND = [
    "cargo", "build", "--bin", BINARY_NAME, "--no-default-features",
    "--features", "mini-chat,static-authn,static-authz,single-tenant,static-credstore",
]
# Source trees whose changes make an existing binary stale.
_SOURCE_ROOTS = [REPO_ROOT / "gears" / "mini-chat", REPO_ROOT / "apps" / BINARY_NAME]
_SKIP_DIRS = {"tests", "docs", "target", ".git", "__pycache__"}

PER_TEST_TIMEOUT_S = 120
HEALTH_TIMEOUT_S = 90

_binary_state: Dict[str, Any] = {"path": None, "error": None}


# --------------------------------------------------------------------------- #
# Binary resolution / build
# --------------------------------------------------------------------------- #


def _binary_path() -> Path:
    explicit = os.environ.get("E2E_BINARY")
    if explicit:
        return Path(explicit)
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(REPO_ROOT / "target")))
    return target / "debug" / BINARY_NAME


def _newest_source_mtime() -> float:
    newest = (REPO_ROOT / "Cargo.lock").stat().st_mtime if (REPO_ROOT / "Cargo.lock").exists() else 0.0
    for root in _SOURCE_ROOTS:
        for path in root.rglob("*"):
            if not path.is_file() or any(part in _SKIP_DIRS for part in path.relative_to(root).parts):
                continue
            if path.suffix in {".rs", ".toml"} and not path.name.endswith("_tests.rs"):
                newest = max(newest, path.stat().st_mtime)
    return newest


def _needs_build(binary: Path) -> bool:
    if not binary.exists():
        return True
    if os.environ.get("E2E_BINARY"):
        return False  # an explicit binary is used as given
    return _newest_source_mtime() > binary.stat().st_mtime


def _ensure_binary() -> None:
    """Resolve (and, when missing or stale, build) the server binary exactly once."""
    if _binary_state["path"] is not None or _binary_state["error"] is not None:
        return
    binary = _binary_path()
    if _needs_build(binary):
        if os.environ.get("E2E_SKIP_BUILD") or os.environ.get("E2E_BINARY"):
            if not binary.exists():
                _binary_state["error"] = (
                    f"server binary not found at {binary} and building is disabled "
                    f"(unset E2E_SKIP_BUILD / E2E_BINARY or run: {' '.join(BUILD_COMMAND)})")
                return
        else:
            print(f"\n[mini_chat_local] building the server: {' '.join(BUILD_COMMAND)}", flush=True)
            result = subprocess.run(BUILD_COMMAND, cwd=str(REPO_ROOT))
            if result.returncode != 0 and not binary.exists():
                _binary_state["error"] = (
                    f"`{' '.join(BUILD_COMMAND)}` failed with exit code {result.returncode} "
                    f"and no binary exists at {binary}")
                return
            if result.returncode != 0:
                print("[mini_chat_local] build failed; using the existing binary", flush=True)
    _binary_state["path"] = binary


def pytest_collection_modifyitems(config: pytest.Config, items: List[pytest.Item]) -> None:
    """Bound every test of this suite and build the server once, before the first test runs
    (never during ``--collect-only``)."""
    mine = [i for i in items if SUITE_DIR in Path(str(i.path)).resolve().parents]
    if not mine:
        return
    for item in mine:
        item.add_marker(pytest.mark.timeout(PER_TEST_TIMEOUT_S))
    if not config.getoption("collectonly", default=False):
        _ensure_binary()


# --------------------------------------------------------------------------- #
# Ports, config
# --------------------------------------------------------------------------- #


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def deep_merge(base: Dict[str, Any], patch: Dict[str, Any]) -> Dict[str, Any]:
    for key, value in patch.items():
        if isinstance(value, dict) and isinstance(base.get(key), dict):
            deep_merge(base[key], value)
        else:
            base[key] = copy.deepcopy(value)
    return base


def build_server_config(*, home_dir: Path, api_port: int, provider_port: int,
                        grpc_socket: Path,
                        overrides: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """The server configuration as a plain dict (JSON is valid YAML, so it is written as is).

    Structure: ``config/e2e-local.yaml`` (static_tokens users, throttling zones, OAGW
    test switches) on top of the gear set of ``config/mini-chat.yaml``.
    """
    cfg: Dict[str, Any] = {
        "server": {"home_dir": str(home_dir)},
        "database": {
            "servers": {
                "sqlite_mini_chat": {
                    "engine": "sqlite",
                    "params": {"WAL": "true", "synchronous": "NORMAL", "busy_timeout": "5000"},
                    "pool": {"max_conns": 5, "acquire_timeout": "30s"},
                }
            }
        },
        "logging": {
            "default": {"console_level": "info", "file": "logs/cf-gears.log", "file_level": "info"},
            "mini_chat": {"console_level": "debug", "file": "logs/mini-chat.log",
                          "file_level": "debug"},
        },
        "gears": {
            "api-gateway": {"config": {
                "bind_addr": f"127.0.0.1:{api_port}",
                "enable_docs": False,
                "cors_enabled": False,
                "openapi": {"title": "Mini-Chat E2E API", "version": "0.1.0",
                            "description": "mini-chat black-box suite"},
                # 25 MiB document + 64 KiB multipart overhead (DESIGN B.8).
                "defaults": {"body_limit_bytes": 26_279_936},
                "auth_disabled": False,
                "require_auth_by_default": True,
                # Throttling zones referenced by the mini-chat chat routes; the gateway
                # refuses to start when a referenced zone is not defined.
                "rate_limit_zones": {"rl_mini_chat_chat": {
                    "rate_limit": "1000/s", "burst_limit": 1000, "response_status_code": 429,
                    "response_retry_after": "auto", "key": {"type": "ip"}, "max_keys": 10000}},
                "in_flight_limit_zones": {"ifl_mini_chat_chat": {
                    "in_flight_limit": 100, "backlog_limit": 0, "backlog_timeout": "0s",
                    "response_status_code": 429, "key": {"type": "ip"}, "max_keys": 10000}},
            }},
            "gear-orchestrator": {"config": {}},
            "grpc-hub": {"config": {"listen_addr": f"uds://{grpc_socket}"}},
            "types-registry": {"config": {}},
            "tenant-resolver": {"config": {"vendor": "constructorfabric"}},
            "single-tenant-tr-plugin": {"config": {"vendor": "constructorfabric"}},
            "authn-resolver": {"config": {"vendor": "constructorfabric"}},
            "static-authn-plugin": {"config": {
                "vendor": "constructorfabric",
                "priority": 100,
                "mode": "static_tokens",
                "tokens": [
                    _token("a", USER_A_ID, TENANT_A_ID),
                    _token("a_reviewer", USER_A_REVIEWER_ID, TENANT_A_ID),
                    _token("b", USER_B_ID, TENANT_B_ID),
                ],
                "s2s_credentials": [
                    {"client_id": "mini-chat", "client_secret": "mini-chat-dev-secret"}],
            }},
            "authz-resolver": {"config": {"vendor": "constructorfabric"}},
            "static-authz-plugin": {"config": {"vendor": "constructorfabric", "priority": 100}},
            "credstore": {
                "database": {"server": "sqlite_mini_chat", "file": "credstore.db"},
                "config": {"vendor": "constructorfabric"},
            },
            "static-credstore-plugin": {"config": {"secrets": [
                {"key": OPENAI_SECRET_KEY, "value": OPENAI_SECRET_VALUE}]}},
            "oagw": {"config": {
                "proxy_timeout_secs": 60,
                "allow_http_upstream": True,  # the mock provider speaks plain HTTP
                "ssrf_policy": {"enabled": False},  # ... on 127.0.0.1
            }},
            "mini-chat": {
                "database": {"server": "sqlite_mini_chat", "file": "mini_chat.db"},
                "config": {
                    "vendor": "constructorfabric",
                    "client_credentials": {"client_id": "mini-chat",
                                           "client_secret": "mini-chat-dev-secret"},
                    # off by default: tests seed `running` turns dating from 2025 that must stay
                    # running; the orphan watchdog tests enable it on their own stacks
                    "orphan_watchdog": {"enabled": False, "scan_interval_secs": 5,
                                        "timeout_secs": 90},
                    # off by default: tests seed `pending` / `uploaded` attachments dating from 2025
                    # that must stay as they are; the upload reaper tests enable it on their own
                    # stack (``test_attachment_cleanup.py``)
                    "upload_reaper": {"enabled": False, "scan_interval_secs": 5,
                                      "stale_after_secs": 300},
                    # shortest valid keep-alive: one `ping` is observable within a few seconds
                    "streaming": {"sse_ping_interval_seconds": 5},
                    "providers": {
                        "openai": {
                            "kind": "openai_responses",
                            "storage_kind": "openai",
                            "host": "127.0.0.1",
                            "port": provider_port,
                            "use_http": True,
                            "api_path": "/v1/responses",
                            "auth_plugin_type":
                                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                            "auth_config": {"header": "Authorization", "prefix": "Bearer ",
                                            "secret_ref": OPENAI_SECRET_KEY},
                        }
                    },
                },
            },
            "static-mini-chat-model-policy-plugin": {"config": {
                "vendor": "constructorfabric",
                "priority": 100,
                "model_catalog": catalog.catalog_entries(),
            }},
            "static-mini-chat-audit-plugin": {"config": {
                "vendor": "constructorfabric", "priority": 100, "enabled": True}},
        },
        "opentelemetry": {"tracing": {"enabled": False}, "metrics": {"enabled": False}},
    }
    if overrides:
        deep_merge(cfg, overrides)
    return cfg


def _token(user: str, subject_id: str, tenant_id: str) -> Dict[str, Any]:
    return {
        "token": TOKENS[user],
        "identity": {
            "subject_id": subject_id,
            "subject_tenant_id": tenant_id,
            "subject_type": "gts.cf.core.security.subject_user.v1~",
            "token_scopes": ["*"],
        },
    }


# --------------------------------------------------------------------------- #
# Server handle
# --------------------------------------------------------------------------- #


@dataclass
class ServerHandle:
    process: subprocess.Popen
    api_port: int
    home_dir: Path
    config_path: Path
    log_path: Path
    config: Dict[str, Any] = field(default_factory=dict)

    @property
    def pid(self) -> int:
        return self.process.pid

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.api_port}"

    @property
    def mini_chat_url(self) -> str:
        return self.base_url + URL_PREFIX

    @property
    def db_path(self) -> Path:
        """SQLite file of the gear: ``<home_dir>/<gear name>/<database.file>``."""
        expected = self.home_dir / "mini-chat" / "mini_chat.db"
        if expected.exists():
            return expected
        found = sorted(self.home_dir.rglob("mini_chat.db"))
        return found[0] if found else expected

    def log_tail(self, lines: int = 40, width: int = 300) -> str:
        try:
            text = self.log_path.read_text(errors="replace")
        except OSError:
            return "<no server log>"
        return "\n".join(line[:width] for line in text.splitlines()[-lines:])

    def alive(self) -> bool:
        return self.process.poll() is None

    def stop(self) -> None:
        """Stop exactly the process this handle started (by pid, never by name)."""
        if self.process.poll() is not None:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)


def _wait_until_ready(handle: ServerHandle) -> None:
    deadline = time.monotonic() + HEALTH_TIMEOUT_S
    last_error = "no response"
    with httpx.Client(timeout=2.0) as client:
        while time.monotonic() < deadline:
            if not handle.alive():
                pytest.fail(f"server exited with code {handle.process.returncode} before "
                            f"becoming healthy\n--- server log ---\n{handle.log_tail()}")
            try:
                if client.get(f"{handle.base_url}/healthz").status_code == 200:
                    break
            except httpx.HTTPError as exc:
                last_error = str(exc)
            time.sleep(0.25)
        else:
            handle.stop()
            pytest.fail(f"/healthz not ready within {HEALTH_TIMEOUT_S}s ({last_error})\n"
                        f"--- server log ---\n{handle.log_tail()}")

        # The gear's routes are mounted at startup; wait until the Models API answers so
        # tests never race the gear's init (and fail loudly if the binary lacks mini-chat).
        models_deadline = time.monotonic() + 30
        last_status = 0
        while time.monotonic() < models_deadline:
            if not handle.alive():
                pytest.fail(f"server exited with code {handle.process.returncode}\n"
                            f"--- server log ---\n{handle.log_tail()}")
            resp = client.get(f"{handle.mini_chat_url}/v1/models", headers=bearer("a"))
            last_status = resp.status_code
            if last_status == 200:
                return
            time.sleep(0.25)
    handle.stop()
    hint = (f" The binary does not serve the mini-chat gear; build with: {' '.join(BUILD_COMMAND)}"
            if last_status == 404 else "")
    pytest.fail(f"GET {URL_PREFIX}/v1/models stayed at HTTP {last_status} after /healthz was up."
                f"{hint}\n--- server log ---\n{handle.log_tail()}")


PROVISIONING_TIMEOUT_S = 90
_PROVISIONED_MARKERS = ('provisioned=["openai"]', "OAGW provisioning complete")


def _provision_provider_secret(handle: ServerHandle) -> None:
    """Make the provider API key readable and wait for the OAGW provisioning.

    The credstore gateway reads a secret's metadata from its own database, so the key seeded in
    the static plugin is not readable on its own (``config/quickstart.yaml``, issue #5094): the
    secret is created through the credstore API after every start and mini-chat's background
    reconcile (first retry 2 s after start, doubling up to 60 s; DESIGN 3.2 "OAGW provisioning")
    then registers the provider upstream and routes. Tests start once that happened.
    """
    with httpx.Client(timeout=10.0) as client:
        resp = client.post(
            f"{handle.base_url}/credstore/v1/secrets", headers=bearer("a"),
            json={"reference": OPENAI_SECRET_KEY, "value": OPENAI_SECRET_VALUE, "sharing": "tenant"})
    if resp.status_code not in (201, 409):
        pytest.fail(f"creating the provider secret failed: {resp.status_code} {resp.text}")
    deadline = time.monotonic() + PROVISIONING_TIMEOUT_S
    while time.monotonic() < deadline:
        if not handle.alive():
            pytest.fail(f"server exited with code {handle.process.returncode}\n"
                        f"--- server log ---\n{handle.log_tail()}")
        log = handle.log_path.read_text(errors="replace")
        if any(marker in log for marker in _PROVISIONED_MARKERS):
            return
        time.sleep(0.25)
    pytest.fail(f"OAGW provisioning did not complete within {PROVISIONING_TIMEOUT_S}s\n"
                f"--- server log ---\n{handle.log_tail()}")


# --------------------------------------------------------------------------- #
# Fixtures
# --------------------------------------------------------------------------- #


@pytest.fixture(scope="session")
def mock_provider_server() -> Iterator[MockOpenAIProvider]:
    provider = MockOpenAIProvider().start()
    try:
        yield provider
    finally:
        provider.stop()


@pytest.fixture(scope="session")
def server_config_overrides() -> Dict[str, Any]:
    """Deep-merged over the generated server config; override in a more specific
    ``conftest.py`` of a later slice to change e.g. quota limits for the whole session."""
    return {}


def _launch_server(mock: MockOpenAIProvider,
                   overrides: Optional[Dict[str, Any]]) -> "tuple[ServerHandle, Path]":
    """Write a config for ``mock``, start the example server and wait until it is healthy.

    Returns the handle and the temp work dir (the caller removes it after ``handle.stop()``).
    """
    _ensure_binary()
    if _binary_state["error"]:
        pytest.skip(_binary_state["error"])
    binary: Path = _binary_state["path"]

    workdir = Path(tempfile.mkdtemp(prefix="mini-chat-e2e-"))
    home_dir = workdir / "home"
    home_dir.mkdir()
    api_port = free_port()
    config = build_server_config(
        home_dir=home_dir, api_port=api_port, provider_port=mock.port,
        grpc_socket=workdir / "grpc.sock", overrides=overrides)
    config_path = workdir / "config.yaml"
    config_path.write_text(json.dumps(config, indent=2))
    log_path = workdir / "server.log"

    env = os.environ.copy()
    env.update({"HOME": str(workdir), "RUST_BACKTRACE": "1"})
    with log_path.open("wb") as log:
        process = subprocess.Popen(
            [str(binary), "--config", str(config_path), "run"],
            cwd=str(workdir), env=env, stdout=log, stderr=subprocess.STDOUT)
    (workdir / "server.pid").write_text(str(process.pid))
    handle = ServerHandle(process=process, api_port=api_port, home_dir=home_dir,
                          config_path=config_path, log_path=log_path, config=config)
    print(f"\n[mini_chat_local] server pid={handle.pid} port={api_port} "
          f"home={home_dir} log={log_path}", file=sys.stderr)
    try:
        _wait_until_ready(handle)
        _provision_provider_secret(handle)
    except BaseException:
        handle.stop()
        raise
    return handle, workdir


@pytest.fixture(scope="session")
def server(mock_provider_server: MockOpenAIProvider,
           server_config_overrides: Dict[str, Any]) -> Iterator[ServerHandle]:
    handle, workdir = _launch_server(mock_provider_server, server_config_overrides)
    try:
        yield handle
    finally:
        handle.stop()
        if not os.environ.get("E2E_KEEP_WORKDIR"):
            shutil.rmtree(workdir, ignore_errors=True)


@dataclass
class Stack:
    """An extra server (own mock provider, own database) started with config overrides."""

    server: ServerHandle
    mock: MockOpenAIProvider

    def api(self) -> MiniChatApi:
        return MiniChatApi(self.server.base_url)

    def db(self) -> GearDb:
        return GearDb(self.server.db_path)

    def seeder(self) -> GearSeeder:
        return GearSeeder(self.server.db_path)


@pytest.fixture(scope="session")
def stack_factory() -> Iterator[Callable[[str, Dict[str, Any]], Stack]]:
    """``stack_factory(name, overrides)`` starts (once per ``name``) an additional server whose
    config is the generated one deep-merged with ``overrides`` (lists are replaced, so a whole
    ``model_catalog`` can be swapped). Every stack is stopped at the end of the session."""
    stacks: Dict[str, Stack] = {}
    workdirs: List[Path] = []
    mocks: List[MockOpenAIProvider] = []

    def make(name: str, overrides: Dict[str, Any]) -> Stack:
        if name in stacks:
            return stacks[name]
        mock = MockOpenAIProvider().start()
        mocks.append(mock)
        handle, workdir = _launch_server(mock, overrides)
        workdirs.append(workdir)
        stacks[name] = Stack(server=handle, mock=mock)
        return stacks[name]

    try:
        yield make
    finally:
        for stack in stacks.values():
            stack.server.stop()
        for mock in mocks:
            mock.stop()
        if not os.environ.get("E2E_KEEP_WORKDIR"):
            for workdir in workdirs:
                shutil.rmtree(workdir, ignore_errors=True)


@pytest.fixture(scope="session")
def restricted_stack(stack_factory: Callable[[str, Dict[str, Any]], Stack]) -> Stack:
    """Extra server with the budget-probing catalog (``catalog.restricted_catalog``) and the
    ``disable_web_search`` kill switch on."""
    return stack_factory("restricted", {"gears": {"static-mini-chat-model-policy-plugin": {
        "config": {"model_catalog": catalog.restricted_catalog(),
                   "kill_switches": {"disable_web_search": True}}}}})


@pytest.fixture()
def restricted(restricted_stack: Stack) -> Iterator[Stack]:
    """The restricted stack with its mock provider reset for this test."""
    restricted_stack.mock.reset()
    yield restricted_stack


# --------------------------------------------------------------------------- #
# Quota slice: stacks with small limits (the shared server keeps the plugin defaults)
# --------------------------------------------------------------------------- #


@pytest.fixture(scope="session")
def quota_stack(stack_factory: Callable[[str, Dict[str, Any]], Stack]) -> Stack:
    """Extra server with small plugin limits (``catalog.QUOTA_LIMITS``), warning threshold 75 %,
    daily web search quota 3, per-message web search / code interpreter limit 2."""
    return stack_factory("quota", catalog.quota_overrides())


@pytest.fixture()
def quota(quota_stack: Stack) -> Iterator[Stack]:
    """The quota stack with its mock provider reset and every ``quota_usage`` row removed."""
    quota_stack.mock.reset()
    quota_stack.seeder().clear_quota()
    yield quota_stack
    quota_stack.mock.release()


@pytest.fixture(scope="session")
def force_standard_stack(stack_factory: Callable[[str, Dict[str, Any]], Stack]) -> Stack:
    """The quota stack with the ``force_standard_tier`` kill switch on."""
    return stack_factory("quota_force_standard",
                         catalog.quota_overrides(kill_switches={"force_standard_tier": True}))


@pytest.fixture()
def force_standard(force_standard_stack: Stack) -> Iterator[Stack]:
    force_standard_stack.mock.reset()
    force_standard_stack.seeder().clear_quota()
    yield force_standard_stack


@pytest.fixture(scope="session")
def no_premium_stack(stack_factory: Callable[[str, Dict[str, Any]], Stack]) -> Stack:
    """The quota stack with the ``disable_premium_tier`` kill switch on."""
    return stack_factory("quota_no_premium",
                         catalog.quota_overrides(kill_switches={"disable_premium_tier": True}))


@pytest.fixture()
def no_premium(no_premium_stack: Stack) -> Iterator[Stack]:
    no_premium_stack.mock.reset()
    no_premium_stack.seeder().clear_quota()
    yield no_premium_stack


@pytest.fixture(scope="session")
def base_url(server: ServerHandle) -> str:
    """Origin of the api-gateway (no gear prefix; ``prefix_path`` is empty)."""
    return server.base_url


@pytest.fixture(scope="session")
def mini_chat_db_path(server: ServerHandle) -> Path:
    return server.db_path


@pytest.fixture()
def mini_chat_db(mini_chat_db_path: Path) -> GearDb:
    return GearDb(mini_chat_db_path)


@pytest.fixture()
def seed(mini_chat_db_path: Path) -> GearSeeder:
    """Read-write view of the gear database for seeding rows the API cannot create yet
    (messages, attachments, reactions) and for reading back what the API wrote."""
    return GearSeeder(mini_chat_db_path)


@pytest.fixture()
def mock_provider(mock_provider_server: MockOpenAIProvider) -> MockOpenAIProvider:
    """The mock provider, with recorded requests and scripts cleared for this test."""
    mock_provider_server.reset()
    return mock_provider_server


@pytest.fixture()
def api(server: ServerHandle) -> Iterator[MiniChatApi]:
    client = MiniChatApi(server.base_url)
    try:
        yield client
    finally:
        client.close()


@pytest.fixture()
def auth_headers() -> Dict[str, str]:
    """Overrides the shared e2e fixture: tenant A's user."""
    return bearer("a")


@pytest.fixture()
def headers_a() -> Dict[str, str]:
    return bearer("a")


@pytest.fixture()
def headers_a_reviewer() -> Dict[str, str]:
    return bearer("a_reviewer")


@pytest.fixture()
def headers_b() -> Dict[str, str]:
    return bearer("b")


# --------------------------------------------------------------------------- #
# Attachments slice: a seeder on the shared database and two extra stacks
# --------------------------------------------------------------------------- #


@pytest.fixture()
def attach(mini_chat_db_path: Path) -> AttachmentSeeder:
    """Read-write view of the shared gear database with the S6 seeds and readers."""
    return AttachmentSeeder(mini_chat_db_path)


@pytest.fixture(scope="session")
def small_stack(stack_factory: Callable[[str, Dict[str, Any]], Stack]) -> Stack:
    """Extra server with tiny attachment limits (``attachment_support.small_overrides``): 64 KiB
    documents, 32 KiB images, three documents per chat, two images per message, two concurrent
    uploads, the upload reaper on (1 s scans), two cleanup attempts, a daily code interpreter quota
    of three calls and the quota stack's limits."""
    return stack_factory("attachments_small", small_overrides())


@pytest.fixture()
def small(small_stack: Stack) -> Iterator[Stack]:
    """The small stack with its mock provider reset and every ``quota_usage`` row removed."""
    small_stack.mock.reset()
    small_stack.seeder().clear_quota()
    yield small_stack
    small_stack.mock.release()


@pytest.fixture(scope="session")
def killed_stack(stack_factory: Callable[[str, Dict[str, Any]], Stack]) -> Stack:
    """Extra server with the ``disable_images`` / ``disable_code_interpreter`` /
    ``disable_file_search`` kill switches on."""
    return stack_factory("attachments_killed", killed_overrides())


@pytest.fixture()
def killed(killed_stack: Stack) -> Iterator[Stack]:
    killed_stack.mock.reset()
    killed_stack.seeder().clear_quota()
    yield killed_stack
