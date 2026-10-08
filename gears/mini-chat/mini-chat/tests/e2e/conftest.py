"""Fixtures: a mock provider + server pair per configuration."""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Any, Callable, Iterator

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import Client, Mock, Server, TOKEN_A, TOKEN_A2, TOKEN_B  # noqa: E402


class Env:
    def __init__(self, overrides: dict[str, Any] | None = None) -> None:
        self.mock = Mock()
        self.mock.start()
        self.server = Server(self.mock, overrides)
        try:
            self.server.start()
        except Exception:
            self.mock.stop()
            raise
        self.a = Client(self.server, TOKEN_A)
        self.a2 = Client(self.server, TOKEN_A2)
        self.b = Client(self.server, TOKEN_B)

    def stop(self) -> None:
        self.server.stop()
        self.mock.stop()
        if not os.environ.get("MINI_CHAT_KEEP_E2E_HOME"):
            self.server.cleanup()


@pytest.fixture(scope="session")
def env() -> Iterator[Env]:
    e = Env()
    yield e
    e.stop()


@pytest.fixture(autouse=True)
def _reset_mock(request: pytest.FixtureRequest) -> None:
    if "env" in request.fixturenames:
        request.getfixturevalue("env").mock.reset()


@pytest.fixture(scope="module")
def make_env() -> Iterator[Callable[[dict[str, Any]], Env]]:
    started: list[Env] = []

    def factory(overrides: dict[str, Any]) -> Env:
        e = Env(overrides)
        started.append(e)
        return e

    yield factory
    for e in started:
        e.stop()
