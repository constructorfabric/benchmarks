"""Client helpers for the mini-chat E2E suite."""

from __future__ import annotations

import json
import sqlite3
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

import httpx


@dataclass
class Env:
    base: str
    mock: str
    home: Path


@dataclass
class Sse:
    status: int
    events: list[tuple[str, Any]] = field(default_factory=list)
    body: Any = None
    headers: dict = field(default_factory=dict)

    def names(self) -> list[str]:
        return [e for e, _ in self.events]

    def first(self, name: str) -> Any:
        for e, d in self.events:
            if e == name:
                return d
        raise AssertionError(f"no {name} event in {self.names()}")

    def all(self, name: str) -> list[Any]:
        return [d for e, d in self.events if e == name]

    def text(self) -> str:
        return "".join(d["content"] for e, d in self.events if e == "delta" and d.get("type") == "text")

    @property
    def terminal(self) -> tuple[str, Any]:
        return self.events[-1]


def parse_sse(lines) -> list[tuple[str, Any]]:
    events = []
    ev, data = None, []
    for raw in lines:
        line = raw.rstrip("\r")
        if line == "":
            if ev is not None or data:
                payload = "\n".join(data)
                try:
                    payload = json.loads(payload) if payload else None
                except json.JSONDecodeError:
                    pass
                events.append((ev or "message", payload))
            ev, data = None, []
            continue
        if line.startswith(":"):
            continue
        if line.startswith("event:"):
            ev = line[6:].strip()
        elif line.startswith("data:"):
            data.append(line[5:].lstrip())
    return events


class Client:
    def __init__(self, env: Env, token: str | None):
        self.env = env
        self.token = token
        self.http = httpx.Client(base_url=env.base, timeout=60)

    def h(self, extra: dict | None = None) -> dict:
        h = {}
        if self.token:
            h["Authorization"] = f"Bearer {self.token}"
        if extra:
            h.update(extra)
        return h

    def get(self, path: str, **kw) -> httpx.Response:
        return self.http.get(path, headers=self.h(kw.pop("headers", None)), **kw)

    def post(self, path: str, **kw) -> httpx.Response:
        return self.http.post(path, headers=self.h(kw.pop("headers", None)), **kw)

    def patch(self, path: str, **kw) -> httpx.Response:
        return self.http.patch(path, headers=self.h(kw.pop("headers", None)), **kw)

    def put(self, path: str, **kw) -> httpx.Response:
        return self.http.put(path, headers=self.h(kw.pop("headers", None)), **kw)

    def delete(self, path: str, **kw) -> httpx.Response:
        return self.http.delete(path, headers=self.h(kw.pop("headers", None)), **kw)

    # -- domain helpers --------------------------------------------------

    def create_chat(self, model: str | None = None, title: str | None = "t") -> dict:
        body: dict[str, Any] = {}
        if model is not None:
            body["model"] = model
        if title is not None:
            body["title"] = title
        r = self.post("/chats", json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def sse(self, method: str, path: str, json_body: Any = None, stop_after: Callable[[list], bool] | None = None) -> Sse:
        """Run an SSE request; `stop_after(events)` closes the connection early."""
        with self.http.stream(method, path, headers=self.h(), json=json_body, timeout=60) as r:
            if r.status_code != 200 or "text/event-stream" not in r.headers.get("content-type", ""):
                r.read()
                try:
                    body = r.json()
                except Exception:  # noqa: BLE001
                    body = r.text
                return Sse(status=r.status_code, body=body, headers=dict(r.headers))
            events: list[tuple[str, Any]] = []
            buf: list[str] = []
            for line in r.iter_lines():
                buf.append(line)
                if line == "":
                    events.extend(parse_sse(buf))
                    buf = []
                    if stop_after and stop_after(events):
                        break
            if buf:
                events.extend(parse_sse(buf + [""]))
            return Sse(status=200, events=events, headers=dict(r.headers))

    def send(self, chat_id: str, content: str, **extra) -> Sse:
        return self.sse("POST", f"/chats/{chat_id}/messages:stream", {"content": content, **extra})

    def send_stop(self, chat_id: str, content: str, stop_after: Callable[[list], bool], **extra) -> Sse:
        return self.sse("POST", f"/chats/{chat_id}/messages:stream", {"content": content, **extra}, stop_after)

    def messages(self, chat_id: str, **params) -> dict:
        r = self.get(f"/chats/{chat_id}/messages", params=params)
        assert r.status_code == 200, r.text
        return r.json()

    def turn(self, chat_id: str, request_id: str) -> httpx.Response:
        return self.get(f"/chats/{chat_id}/turns/{request_id}")

    def wait_turn(self, chat_id: str, request_id: str, states=("done", "error", "cancelled"), timeout=20) -> dict:
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            r = self.turn(chat_id, request_id)
            if r.status_code == 200:
                last = r.json()
                if last["state"] in states:
                    return last
            time.sleep(0.2)
        raise AssertionError(f"turn did not reach {states}: {last}")

    def upload(self, chat_id: str, filename: str, content: bytes, content_type: str | None = "text/plain") -> httpx.Response:
        files = {"file": (filename, content, content_type)} if content_type else {"file": (filename, content)}
        return self.post(f"/chats/{chat_id}/attachments", files=files, timeout=60)


class Mock:
    def __init__(self, env: Env):
        self.env = env

    def requests(self) -> list[dict]:
        return httpx.get(f"{self.env.mock}/__mock/requests").json()

    def chat_requests(self) -> list[dict]:
        return [r for r in self.requests() if r["path"].endswith("/responses")]

    def mark(self) -> int:
        return len(self.requests())

    def since(self, mark: int) -> list[dict]:
        return self.requests()[mark:]

    def config(self, **kw) -> None:
        httpx.post(f"{self.env.mock}/__mock/config", json=kw)

    def state(self) -> dict:
        return httpx.get(f"{self.env.mock}/__mock/state").json()


def uuid_blob(u: str) -> bytes:
    return uuid.UUID(u).bytes


def blob_uuid(b: bytes) -> str:
    return str(uuid.UUID(bytes=b))


class Db:
    def __init__(self, env: Env):
        self.path = env.home / "mini-chat" / "mini_chat.db"

    def conn(self) -> sqlite3.Connection:
        c = sqlite3.connect(self.path, timeout=10)
        c.row_factory = sqlite3.Row
        return c

    def q(self, sql: str, *args) -> list[sqlite3.Row]:
        with self.conn() as c:
            return c.execute(sql, args).fetchall()

    def x(self, sql: str, *args) -> int:
        for _ in range(50):
            try:
                with self.conn() as c:
                    cur = c.execute(sql, args)
                    c.commit()
                    return cur.rowcount
            except sqlite3.OperationalError as e:
                if "locked" not in str(e):
                    raise
                time.sleep(0.1)
        raise RuntimeError("database locked")

    def turn(self, request_id: str) -> sqlite3.Row:
        rows = self.q("select * from chat_turns where request_id = ?", uuid_blob(request_id))
        assert rows, f"no turn {request_id}"
        return rows[0]

    def quota(self, user_id: str) -> dict[tuple[str, str], sqlite3.Row]:
        rows = self.q("select * from quota_usage where user_id = ?", uuid_blob(user_id))
        return {(r["period_type"], r["bucket"]): r for r in rows}


def server_log(env: Env) -> list[dict]:
    out = []
    for line in (env.home / "logs" / "server.log").read_text(errors="ignore").splitlines():
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return out


def usage_events(env: Env) -> list[dict]:
    return [
        e["fields"]
        for e in server_log(env)
        if e.get("fields", {}).get("message") == "usage event published"
    ]


def audit_events(env: Env) -> list[dict]:
    out = []
    for e in server_log(env):
        f = e.get("fields", {})
        if f.get("message") == "audit event":
            out.append(json.loads(f["event"]))
    return out


def wait_for(pred: Callable[[], Any], timeout: float = 15, interval: float = 0.2):
    deadline = time.time() + timeout
    while time.time() < deadline:
        v = pred()
        if v:
            return v
        time.sleep(interval)
    raise AssertionError("condition not met in time")


def in_thread(fn: Callable[[], Any]) -> tuple[threading.Thread, dict]:
    out: dict = {}

    def run():
        try:
            out["result"] = fn()
        except Exception as e:  # noqa: BLE001
            out["error"] = e

    t = threading.Thread(target=run, daemon=True)
    t.start()
    return t, out


USER_A = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
USER_A2 = "44444444-6a88-4768-9dfc-6bcd5187d9ed"
USER_QUOTA = "55555555-6a88-4768-9dfc-6bcd5187d9ed"
TENANT_A = "00000000-df51-5b42-9538-d2b56b7ee953"


def problem_reason(body: dict) -> Any:
    ctx = body.get("context") or {}
    if "reason" in ctx:
        return ctx["reason"]
    if ctx.get("field_violations"):
        return ctx["field_violations"][0].get("reason")
    if ctx.get("violations"):
        v = ctx["violations"][0]
        return v.get("type") or v.get("subject")
    if "resource_name" in ctx:
        return ctx["resource_name"]
    return None
