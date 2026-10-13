#!/usr/bin/env python3
"""Black-box smoke test of the mini-chat gear.

Runs the REAL ``cf-gears-example-server`` binary against the in-process mock
OpenAI-compatible provider (``mock_openai.py``) through OAGW, once per provider
flavour (``openai``: ``/v1/...`` + Bearer auth; ``azure``: ``/openai/...?api-version``
+ ``api-key`` auth). It only talks HTTP/SSE to the server, inspects the requests the
mock received and reads the gear's SQLite DB with ``sqlite3``.

Usage (from anywhere; stdlib only):
    python3 gears/mini-chat/e2e-smoke/run_smoke.py [--flavour openai|azure ...]
        [--binary PATH] [--build] [--keep]

Exits 0 when every check passes. On failure the server log tail is printed and the
run directory is kept. The server is stopped by pid on every exit path.
"""
import argparse
import glob
import json
import os
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import traceback
import urllib.error
import urllib.request
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
sys.dont_write_bytecode = True  # keep the source tree clean
sys.path.insert(0, str(HERE))
import mock_openai  # noqa: E402

DEFAULT_BINARY = REPO / "target" / "debug" / "cf-gears-example-server"
BUILD_CMD = [
    "cargo", "build", "--offline", "--bin", "cf-gears-example-server", "--no-default-features",
    "--features", "mini-chat,static-authn,static-authz,single-tenant,static-credstore",
]

TOKEN_A1 = "smoke-token-tenant-a"          # tenant A user 1 (= S2S identity)
TOKEN_A2 = "smoke-token-tenant-a-user2"    # tenant A user 2
TOKEN_B = "smoke-token-tenant-b"           # tenant B
TENANT_A = uuid.UUID("00000000-df51-5b42-9538-d2b56b7ee953")
USER_A1 = uuid.UUID("11111111-6a88-4768-9dfc-6bcd5187d9ed")
SECRET_VALUE = "sk-mocksmokekey000000001"
MODEL = "gpt-4.1"
MOCK_IDS = ("resp_mock", "msg_mock", "file-mock", "vs_mock", "sk-mock")
AZURE_API_VERSION = "2025-03-01-preview"
APIKEY_PLUGIN = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"


def flavour_settings(name, mock_port):
    """Provider entry, secret and expected wire details of one flavour."""
    common = {"kind": "openai_responses", "host": "127.0.0.1", "port": mock_port,
              "use_http": True, "upstream_alias": "127.0.0.1",
              "auth_plugin_type": APIKEY_PLUGIN}
    if name == "openai":
        return {
            "provider_id": "openai", "secret": "openai-key",
            "entry": {**common, "storage_kind": "openai", "api_path": "/v1/responses",
                      "auth_config": {"header": "authorization", "prefix": "Bearer ",
                                      "secret_ref": "cred://openai-key"}},
            "prefix": "/v1", "query": "",
            "auth_header": "authorization", "auth_value": f"Bearer {SECRET_VALUE}",
        }
    if name == "azure":
        return {
            "provider_id": "azure_openai", "secret": "azure-openai-key",
            "entry": {**common, "storage_kind": "azure",
                      "api_path": f"/openai/v1/responses?api-version={AZURE_API_VERSION}",
                      "api_version": AZURE_API_VERSION,
                      "auth_config": {"header": "api-key", "prefix": "",
                                      "secret_ref": "cred://azure-openai-key"}},
            "prefix": "/openai", "query": f"api-version={AZURE_API_VERSION}",
            "auth_header": "api-key", "auth_value": SECRET_VALUE,
        }
    raise ValueError(f"unknown flavour {name}")


# ---------------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------------

class SmokeFailure(AssertionError):
    pass


def check(cond, what, detail=None):
    if not cond:
        raise SmokeFailure(what if detail is None else f"{what}: {detail}")


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Http:
    """Minimal JSON/SSE client; remembers every body to scan for provider ids."""

    def __init__(self, base):
        self.base = base
        self.bodies = []

    def call(self, method, path, token=None, body=None, raw=None, ctype=None, timeout=60):
        headers = {}
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["content-type"] = "application/json"
        elif raw is not None:
            data = raw
            headers["content-type"] = ctype
        if token:
            headers["authorization"] = f"Bearer {token}"
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                status, hdrs, payload = resp.status, dict(resp.headers), resp.read()
        except urllib.error.HTTPError as e:
            status, hdrs, payload = e.code, dict(e.headers), e.read()
        text = payload.decode("utf-8", "replace")
        self.bodies.append((f"{method} {path}", text))
        return status, {k.lower(): v for k, v in hdrs.items()}, text

    def json(self, method, path, token, body=None, expect=200):
        status, hdrs, text = self.call(method, path, token, body)
        check(status == expect, f"{method} {path} -> {expect}", f"got {status}: {text[:400]}")
        return (json.loads(text) if text else None), hdrs

    def stream(self, path, token, body=None, method="POST"):
        """Call an SSE endpoint; returns (status, [(event, data)]) without pings."""
        status, hdrs, text = self.call(method, path, token, body)
        if status != 200:
            return status, text
        check(hdrs.get("content-type", "").startswith("text/event-stream"),
              f"POST {path} content-type", hdrs.get("content-type"))
        events = []
        for block in text.replace("\r\n", "\n").split("\n\n"):
            name, data = None, []
            for line in block.split("\n"):
                if line.startswith("event:"):
                    name = line[6:].strip()
                elif line.startswith("data:"):
                    data.append(line[5:].lstrip())
            if name and name != "ping":
                events.append((name, json.loads("\n".join(data)) if data else None))
        return status, events


def multipart(filename, content_type, content):
    boundary = uuid.uuid4().hex
    payload = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; "
               f"filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").encode()
    payload += content + f"\r\n--{boundary}--\r\n".encode()
    return payload, f"multipart/form-data; boundary={boundary}"


def wait_until(pred, timeout, what, interval=0.2):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = pred()
        if value:
            return value
        time.sleep(interval)
    raise SmokeFailure(f"timed out after {timeout}s waiting for {what}")


# ---------------------------------------------------------------------------
# server lifecycle
# ---------------------------------------------------------------------------

class Server:
    def __init__(self, binary, config_path, log_path):
        self.binary, self.config_path, self.log_path = binary, config_path, log_path
        self.proc = None
        self.pid_file = Path(config_path).with_name("server.pid")

    def start(self):
        log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            [str(self.binary), "--config", str(self.config_path), "run"],
            cwd=str(REPO), stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        log.close()
        self.pid_file.write_text(str(self.proc.pid))

    def stop(self):
        """Stop by pid only: SIGTERM, then SIGKILL after 15 s."""
        if self.proc is None or self.proc.poll() is not None:
            return
        os.kill(self.proc.pid, signal.SIGTERM)
        try:
            self.proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.kill(self.proc.pid, signal.SIGKILL)
            self.proc.wait(timeout=5)

    def log_tail(self, n=15):
        """WARN/ERROR lines (last 30) plus the last ``n`` lines of the server log."""
        try:
            lines = Path(self.log_path).read_text(errors="replace").splitlines()
        except OSError:
            return ""
        problems = [ln for ln in lines[:-n] if (" WARN " in ln or " ERROR " in ln)
                    and "canonical error response (client)" not in ln][-30:]
        return "\n".join(problems + ["..."] + lines[-n:])


def render_config(home, api_port, fl):
    text = (HERE / "config.template.yaml").read_text()
    values = {
        "@@HOME_DIR@@": str(home),
        "@@API_PORT@@": str(api_port),
        "@@PROVIDERS@@": json.dumps({fl["provider_id"]: fl["entry"]}),
        "@@PROVIDER_ID@@": fl["provider_id"],
    }
    # Substitute outside the comment lines, which document the placeholders.
    lines = []
    for line in text.splitlines(keepends=True):
        if not line.lstrip().startswith("#"):
            for key, value in values.items():
                line = line.replace(key, value)
            check("@@" not in line, "unknown config placeholder", line)
        lines.append(line)
    text = "".join(lines)
    path = Path(home) / "config.yaml"
    path.write_text(text)
    return path


# ---------------------------------------------------------------------------
# the scenario
# ---------------------------------------------------------------------------

class Scenario:
    def __init__(self, flavour, http, recorder, home):
        self.name = flavour["provider_id"]
        self.fl = flavour
        self.http = http
        self.rec = recorder
        self.home = Path(home)
        self.chat = "/mini-chat/v1/chats"

    def step(self, title):
        print(f"  [{self.name}] {title}", flush=True)

    # -- mock inspection ---------------------------------------------------
    def provider_requests(self, suffix, method="POST"):
        return [e for e in self.rec.entries()
                if e["method"] == method and e["path"].split("?")[0].endswith(suffix)]

    def responses_calls(self):
        return [e for e in self.provider_requests("/responses") if (e.get("json") or {}).get("stream")]

    def check_wire(self, entry):
        path, _, query = entry["path"].partition("?")
        check(path.startswith(self.fl["prefix"] + "/"), "provider path prefix", entry["path"])
        check(query == self.fl["query"], "provider query string", entry["path"])
        check(entry["headers"].get(self.fl["auth_header"]) == self.fl["auth_value"],
              f"provider auth header {self.fl['auth_header']}", entry["headers"])

    # -- steps ---------------------------------------------------------------
    def run(self):
        self.step("create provider secret (S2S identity)")
        status, _, text = self.http.call(
            "POST", "/credstore/v1/secrets", TOKEN_A1,
            {"reference": self.fl["secret"], "value": SECRET_VALUE, "sharing": "tenant"})
        check(status == 201, "credstore secret created", f"{status} {text[:300]}")

        self.step("models list and quota status")
        models, _ = self.http.json("GET", "/mini-chat/v1/models", TOKEN_A1)
        ids = [m["model_id"] for m in models["items"]]
        check(MODEL in ids and "gpt-4.1-mini" in ids, "models list has the catalog models", ids)
        for m in models["items"]:
            check("provider_id" not in m and "provider_model_id" not in m,
                  "models do not expose internal fields", m)
        quota0, _ = self.http.json("GET", "/mini-chat/v1/quota/status", TOKEN_A1)
        check(isinstance(quota0.get("tiers"), list) and quota0["tiers"], "quota tiers", quota0)

        self.step("create chat")
        chat, hdrs = self.http.json("POST", self.chat, TOKEN_A1, {"title": "smoke"}, expect=201)
        chat_id = chat["id"]
        check(chat.get("model") == MODEL, "chat gets the default model", chat)
        check(hdrs.get("location", "").endswith(f"/v1/chats/{chat_id}"), "Location header", hdrs)
        base = f"{self.chat}/{chat_id}"

        self.step("stream first message")
        r1 = str(uuid.uuid4())
        status, ev = self.http.stream(f"{base}/messages:stream", TOKEN_A1,
                                      {"content": "hi", "request_id": r1})
        check(status == 200, "stream 200", ev)
        names = [n for n, _ in ev]
        check(names[0] == "stream_started" and names[-1] == "done", "event order", names)
        check(set(names[1:-1]) == {"delta"}, "only deltas between start and done", names)
        started, done = ev[0][1], ev[-1][1]
        check(started["request_id"] == r1 and started["is_new_turn"] is True, "stream_started", started)
        check("".join(d["content"] for n, d in ev if n == "delta") == "Hello world", "delta text", ev)
        check(done["usage"] == {"input_tokens": 12, "output_tokens": 2}, "done.usage", done)
        check(done["effective_model"] == MODEL and done["selected_model"] == MODEL,
              "done.effective_model", done)
        check(done["quota_decision"] == "allow", "done.quota_decision", done)
        msg1 = started["message_id"]
        calls = self.responses_calls()
        check(len(calls) == 1, "one provider stream request", len(calls))
        req1 = calls[0]
        self.check_wire(req1)
        body = req1["json"]
        check(body["model"] == MODEL and body["stream"] is True and body.get("store") is False,
              "provider request model/stream/store", {k: body.get(k) for k in ("model", "stream", "store")})
        check(body.get("user") == TENANT_A.hex + USER_A1.hex, "provider user field", body.get("user"))
        check(body.get("metadata", {}).get("chat_id") == chat_id, "provider metadata.chat_id",
              body.get("metadata"))
        check(not body.get("tools"), "no tools without attachments or web search", body.get("tools"))

        self.step("replay the same request_id")
        status, ev = self.http.stream(f"{base}/messages:stream", TOKEN_A1,
                                      {"content": "hi", "request_id": r1})
        check(status == 200, "replay 200", ev)
        names = [n for n, _ in ev]
        check(names == ["stream_started", "delta", "done"], "replay events", names)
        check(ev[0][1]["is_new_turn"] is False and ev[0][1]["request_id"] == r1, "replay start", ev[0])
        check(ev[0][1]["message_id"] == msg1, "replay message_id is the persisted one", ev[0])
        check(ev[2][1]["usage"] == {"input_tokens": 12, "output_tokens": 2}, "replay usage", ev[2])
        check(len(self.responses_calls()) == 1, "replay makes no provider request",
              len(self.responses_calls()))

        self.step("messages list and turn status")
        msgs, _ = self.http.json("GET", f"{base}/messages", TOKEN_A1)
        items = msgs["items"]
        check(len(items) == 2, "two messages", items)
        check([m["role"] for m in items] == ["user", "assistant"], "message roles", items)
        check({m["request_id"] for m in items} == {r1}, "messages share the request_id", items)
        check(items[1]["id"] == msg1 and items[1]["content"] == "Hello world", "assistant message", items[1])
        check(items[1].get("model") == MODEL, "assistant message model", items[1])
        for m in items:
            check(m["attachments"] == [] and m["my_reaction"] is None, "message contract", m)
        turn, _ = self.http.json("GET", f"{base}/turns/{r1}", TOKEN_A1)
        check(turn["state"] == "done" and turn.get("assistant_message_id") == msg1, "turn status", turn)

        self.step("isolation: tenant B and user A2 get 404")
        for token in (TOKEN_B, TOKEN_A2):
            for path in (base, f"{base}/messages", f"{base}/turns/{r1}"):
                status, _, text = self.http.call("GET", path, token)
                check(status == 404, f"foreign GET {path} -> 404", f"{status} {text[:200]}")
            status, _, text = self.http.call("DELETE", base, token)
            check(status == 404, "foreign DELETE chat -> 404", f"{status} {text[:200]}")
            status, _, text = self.http.call("POST", f"{base}/messages:stream", token,
                                             {"content": "intrusion"})
            check(status == 404, "foreign stream -> 404", f"{status} {text[:200]}")
        check(len(self.responses_calls()) == 1, "foreign requests reach no provider",
              len(self.responses_calls()))

        self.step("upload a text document")
        payload, ctype = multipart("note.txt", "text/plain", b"The smoke test secret word is pelican.\n")
        status, _, text = self.http.call("POST", f"{base}/attachments", TOKEN_A1, raw=payload,
                                         ctype=ctype)
        check(status == 201, "upload 201", f"{status} {text[:400]}")
        att = json.loads(text)
        check(att["status"] == "ready" and att["kind"] == "document", "attachment ready document", att)
        att_id = att["id"]
        uploads = [e for e in self.provider_requests("/files") if "/vector_stores/" not in e["path"]]
        check(len(uploads) == 1, "one provider file upload", uploads)
        self.check_wire(uploads[0])
        stores = self.provider_requests("/vector_stores")
        check(len(stores) == 1, "one vector store created", stores)
        self.check_wire(stores[0])
        vs_id = None
        for e in self.rec.entries():
            p = e["path"].split("?")[0]
            if e["method"] == "POST" and "/vector_stores/" in p and p.endswith("/files"):
                vs_id = p.split("/vector_stores/")[1].split("/")[0]
                self.check_wire(e)
        check(vs_id and vs_id.startswith("vs_mock"), "file added to the vector store", vs_id)
        got, _ = self.http.json("GET", f"{base}/attachments/{att_id}", TOKEN_A1)
        check(got["status"] == "ready" and got["filename"] == "note.txt", "GET attachment", got)

        self.step("stream with the document (file_search)")
        r2 = str(uuid.uuid4())
        status, ev = self.http.stream(f"{base}/messages:stream", TOKEN_A1,
                                      {"content": "what is the secret word?", "request_id": r2,
                                       "attachment_ids": [att_id]})
        check(status == 200 and ev[-1][0] == "done", "stream with attachment done", ev)
        msg2 = ev[0][1]["message_id"]
        body = self.responses_calls()[-1]["json"]
        fs = [t for t in body.get("tools") or [] if t.get("type") == "file_search"]
        check(len(fs) == 1 and fs[0].get("vector_store_ids") == [vs_id], "file_search tool", body.get("tools"))
        check(fs[0].get("max_num_results") == 5, "file_search max_num_results", fs[0])
        check(body.get("max_tool_calls") == 10, "max_tool_calls from the catalog", body.get("max_tool_calls"))
        msgs, _ = self.http.json("GET", f"{base}/messages", TOKEN_A1)
        user2 = [m for m in msgs["items"] if m["request_id"] == r2 and m["role"] == "user"]
        check(len(user2) == 1 and [a["attachment_id"] for a in user2[0]["attachments"]] == [att_id],
              "user message lists its attachment", user2)

        self.step("reaction on the assistant message")
        reaction, _ = self.http.json("PUT", f"{base}/messages/{msg2}/reaction", TOKEN_A1,
                                     {"reaction": "like"})
        check(reaction["message_id"] == msg2 and reaction["reaction"] == "like", "reaction", reaction)
        again, _ = self.http.json("PUT", f"{base}/messages/{msg2}/reaction", TOKEN_A1,
                                  {"reaction": "like"})
        check(again["reaction"] == "like", "reaction is idempotent", again)
        user_msg = user2[0]["id"]
        status, _, text = self.http.call("PUT", f"{base}/messages/{user_msg}/reaction", TOKEN_A1,
                                         {"reaction": "like"})
        check(status == 400, "reaction on a user message -> 400", f"{status} {text[:200]}")
        msgs, _ = self.http.json("GET", f"{base}/messages", TOKEN_A1)
        mine = {m["id"]: m["my_reaction"] for m in msgs["items"]}
        check(mine.get(msg2) == "like", "my_reaction in the message list", mine)

        self.step("web search (kill switch off)")
        r4 = str(uuid.uuid4())
        status, ev = self.http.stream(f"{base}/messages:stream", TOKEN_A1,
                                      {"content": "search the web", "request_id": r4,
                                       "web_search": {"enabled": True}})
        check(status == 200 and ev[-1][0] == "done", "web search stream done", ev)
        tools = self.responses_calls()[-1]["json"].get("tools") or []
        ws = [t for t in tools if t.get("type") == "web_search"]
        check(len(ws) == 1 and ws[0].get("search_context_size") == "low", "web_search tool", tools)
        check(any(t.get("type") == "file_search" for t in tools), "file_search next to web_search", tools)

        self.step("retry the last turn (reuses web search and file search)")
        before = len(self.responses_calls())
        status, ev = self.http.stream(f"{base}/turns/{r4}/retry", TOKEN_A1)
        check(status == 200, "retry 200", ev)
        names = [n for n, _ in ev]
        check(names[0] == "stream_started" and names[-1] == "done", "retry events", names)
        r3 = ev[0][1]["request_id"]
        check(r3 not in (r1, r2, r4) and ev[0][1]["is_new_turn"] is True,
              "retry gets a new server request_id", ev[0])
        check(len(self.responses_calls()) == before + 1, "retry calls the provider once",
              len(self.responses_calls()))
        retry_tools = {t.get("type") for t in self.responses_calls()[-1]["json"].get("tools") or []}
        check(retry_tools == {"web_search", "file_search"}, "retry keeps the turn's tools", retry_tools)
        status, _, text = self.http.call("GET", f"{base}/turns/{r4}", TOKEN_A1)
        check(status == 404, "the replaced turn is gone", f"{status} {text[:200]}")
        turn, _ = self.http.json("GET", f"{base}/turns/{r3}", TOKEN_A1)
        check(turn["state"] == "done", "retried turn done", turn)
        msgs, _ = self.http.json("GET", f"{base}/messages", TOKEN_A1)
        check([m["request_id"] for m in msgs["items"]] == [r1, r1, r2, r2, r3, r3],
              "the replaced turn's messages are hidden", [m["request_id"] for m in msgs["items"]])
        check(msgs["items"][4]["content"] == "search the web", "retry re-sends the user message",
              msgs["items"][4])

        self.step("provider error -> SSE error provider_error (sanitized)")
        r5 = str(uuid.uuid4())
        status, ev = self.http.stream(f"{base}/messages:stream", TOKEN_A1,
                                      {"content": f"please fail {mock_openai.ERROR_MARKER}",
                                       "request_id": r5})
        check(status == 200, "error stream 200", ev)
        names = [n for n, _ in ev]
        check(names[0] == "stream_started" and names[-1] == "error" and "done" not in names,
              "error is terminal", names)
        err = ev[-1][1]
        check(err["code"] == "provider_error", "error code", err)
        check("resp_" not in err["message"] and "https://" not in err["message"]
              and "sk-" not in err["message"], "error message is sanitized", err)
        check("The server had an error processing" in err["message"], "error message kept", err)
        print(f"    sanitized provider error: {err['message']!r}", flush=True)
        turn = wait_until(lambda: (lambda t: t if t["state"] == "error" else None)(
            self.http.json("GET", f"{base}/turns/{r5}", TOKEN_A1)[0]), 10, "turn r5 error")
        check(turn.get("error_code") == "provider_error", "failed turn error_code", turn)

        self.step("quota status reflects usage")
        quota1, _ = self.http.json("GET", "/mini-chat/v1/quota/status", TOKEN_A1)
        check({t["tier"] for t in quota1["tiers"]} == {"premium", "total"}, "quota tiers", quota1)
        for tier in quota0["tiers"]:
            for p in tier["periods"]:
                check(p["used_credits_micro"] == 0, "no usage before the first turn", quota0)

        self.step("DB checks")
        self.db_checks(chat_id, r1, r2, r3, r4, r5, msg1, quota1)

        self.step("delete chat and provider cleanup")
        status, _, text = self.http.call("DELETE", base, TOKEN_A1)
        check(status == 204, "delete chat 204", f"{status} {text[:200]}")
        status, _, _ = self.http.call("GET", base, TOKEN_A1)
        check(status == 404, "deleted chat -> 404", status)
        file_id = None
        for e in self.rec.entries():
            if e["method"] == "POST" and "/vector_stores/" in e["path"]:
                file_id = (e.get("json") or {}).get("file_id")
        check(file_id and file_id.startswith("file-mock"), "file id sent to the vector store", file_id)

        def deleted():
            paths = [e["path"].split("?")[0] for e in self.rec.entries() if e["method"] == "DELETE"]
            return (any(p.endswith(f"/files/{file_id}") for p in paths)
                    and any(p.endswith(f"/vector_stores/{vs_id}") for p in paths))
        wait_until(deleted, 30, "provider file and vector store DELETEs")
        for e in self.rec.entries():
            if e["method"] == "DELETE":
                self.check_wire(e)
        db = self.open_db()
        try:
            row = db.execute("SELECT deleted_at FROM chats WHERE id = ?", (uuid.UUID(chat_id).bytes,)).fetchone()
            check(row and row[0] is not None, "chat soft-deleted in DB", row)
            n = db.execute(
                "SELECT COUNT(*) FROM mini_chat_outbox_outgoing o JOIN mini_chat_outbox_partitions p "
                "ON p.id = o.partition_id WHERE p.queue = 'mini-chat.chat_cleanup'").fetchone()[0]
            check(n == 1, "one chat cleanup outbox event", n)
            n = db.execute("SELECT COUNT(*) FROM chat_vector_stores WHERE chat_id = ?",
                           (uuid.UUID(chat_id).bytes,)).fetchone()[0]
            check(n == 0, "chat_vector_stores row removed after the provider delete", n)
        finally:
            db.close()

        self.surface_checks()

        self.step("no provider identifiers in any API response")
        for what, text in self.http.bodies:
            for marker in MOCK_IDS:
                check(marker not in text, f"{what} leaks a provider identifier ({marker})", text[:300])

    def surface_checks(self):
        """Second chat: list/patch chat, get model, edit and delete the last turn."""
        self.step("second chat: list, patch, edit and delete turn, get model")
        chat, _ = self.http.json("POST", self.chat, TOKEN_A1, {"title": "second"}, expect=201)
        base = f"{self.chat}/{chat['id']}"
        model, _ = self.http.json("GET", f"/mini-chat/v1/models/{MODEL}", TOKEN_A1)
        check(model["model_id"] == MODEL and model["tier"] == "premium", "GET model", model)
        status, _, _ = self.http.call("GET", "/mini-chat/v1/models/no-such-model", TOKEN_A1)
        check(status == 404, "unknown model -> 404", status)
        patched, _ = self.http.json("PATCH", base, TOKEN_A1, {"title": "  renamed  "})
        check(patched["title"] == "renamed", "PATCH trims the title", patched)
        r1 = str(uuid.uuid4())
        status, ev = self.http.stream(f"{base}/messages:stream", TOKEN_A1,
                                      {"content": "first", "request_id": r1})
        check(status == 200 and ev[-1][0] == "done", "second chat stream", ev)
        listed, _ = self.http.json("GET", self.chat, TOKEN_A1)
        check(listed["items"][0]["id"] == chat["id"] and listed["items"][0]["message_count"] == 2,
              "most recently active chat listed first", listed["items"][:1])
        listed_b, _ = self.http.json("GET", self.chat, TOKEN_B)
        check(listed_b["items"] == [], "tenant B lists no chats of tenant A", listed_b)

        status, ev = self.http.stream(f"{base}/turns/{r1}", TOKEN_A1, {"content": "edited"},
                                      method="PATCH")
        check(status == 200 and ev[0][0] == "stream_started" and ev[-1][0] == "done", "edit stream", ev)
        r2 = ev[0][1]["request_id"]
        check(r2 != r1, "edit gets a new request_id", ev[0])
        last_input = self.responses_calls()[-1]["json"]["input"]
        check("edited" in json.dumps(last_input[-1]) and "first" not in json.dumps(last_input),
              "edit sends the new content without the replaced turn", last_input)
        msgs, _ = self.http.json("GET", f"{base}/messages", TOKEN_A1)
        check([m["content"] for m in msgs["items"]] == ["edited", "Hello world"], "edited messages", msgs)

        status, _, text = self.http.call("DELETE", f"{base}/turns/{r2}", TOKEN_A1)
        check(status == 204, "delete last turn 204", f"{status} {text[:200]}")
        msgs, _ = self.http.json("GET", f"{base}/messages", TOKEN_A1)
        check(msgs["items"] == [], "deleted turn's messages are hidden", msgs)
        status, _, text = self.http.call("DELETE", f"{base}/turns/{r2}", TOKEN_A1)
        check(status == 409 and json.loads(text)["context"]["reason"] == "NOT_LATEST_TURN",
              "deleting it again -> 409 NOT_LATEST_TURN", f"{status} {text[:300]}")

    # -- DB ------------------------------------------------------------------
    def open_db(self):
        found = glob.glob(str(self.home / "**" / "mini_chat.db"), recursive=True)
        check(len(found) == 1, "gear DB file", found)
        return sqlite3.connect(f"file:{found[0]}?mode=ro", uri=True, timeout=10)

    def db_checks(self, chat_id, r1, r2, r3, r4, r5, msg1, quota):
        db = self.open_db()
        try:
            cid = uuid.UUID(chat_id).bytes

            def turn(rid):
                return db.execute(
                    "SELECT state, error_code, deleted_at, replaced_by_request_id, effective_model "
                    "FROM chat_turns WHERE chat_id = ? AND request_id = ?",
                    (cid, uuid.UUID(rid).bytes)).fetchone()

            for rid in (r1, r2, r3):
                t = turn(rid)
                check(t and t[0] == "completed" and t[2] is None and t[4] == MODEL,
                      f"chat_turns {rid} completed", t)
            t = turn(r4)
            check(t and t[0] == "completed" and t[2] is not None
                  and t[3] == uuid.UUID(r3).bytes, "retried turn soft-deleted and replaced", t)
            ws = db.execute("SELECT web_search_enabled FROM chat_turns WHERE chat_id = ? AND request_id = ?",
                            (cid, uuid.UUID(r3).bytes)).fetchone()
            check(ws == (1,), "retried turn keeps web_search_enabled", ws)
            t = turn(r5)
            check(t and t[0] == "failed" and t[1] == "provider_error", "failed turn", t)
            n_turns = db.execute("SELECT COUNT(*) FROM chat_turns WHERE chat_id = ?", (cid,)).fetchone()[0]
            check(n_turns == 5, "five turns in the chat", n_turns)

            rows = db.execute(
                "SELECT role, model, input_tokens, output_tokens, provider_response_id FROM messages "
                "WHERE chat_id = ? AND id = ?", (cid, uuid.UUID(msg1).bytes)).fetchall()
            check(rows == [("assistant", MODEL, 12, 2, "resp_mock0000000001")],
                  "assistant message row (model, usage, provider response id)", rows)
            models = db.execute(
                "SELECT DISTINCT model FROM messages WHERE chat_id = ? AND role = 'assistant'",
                (cid,)).fetchall()
            check(models == [(MODEL,)], "messages.model on every assistant message", models)

            def quota_rows():
                return db.execute(
                    "SELECT period_type, calls, spent_credits_micro, reserved_credits_micro, "
                    "input_tokens, output_tokens FROM quota_usage "
                    "WHERE tenant_id = ? AND user_id = ? AND bucket = 'total' ORDER BY period_type",
                    (TENANT_A.bytes, USER_A1.bytes)).fetchall()
            q = quota_rows()
            check({r[0] for r in q} >= {"daily", "monthly"}, "quota_usage total rows per period", q)
            for r in q:
                # Four completed provider turns (r1, r2, r4, r3 = retry of r4) + the failed r5.
                check(r[1] == 5 and r[2] > 0 and r[3] == 0, "quota_usage total row settled", r)
                check(r[4] >= 4 * 12 and r[5] >= 4 * 2, "quota_usage tokens", r)

            # Quota status = spent + reserved of the matching quota_usage bucket.
            buckets = {"total": "total", "premium": "tier:premium"}
            for tier in quota["tiers"]:
                for p in tier["periods"]:
                    row = db.execute(
                        "SELECT spent_credits_micro + reserved_credits_micro FROM quota_usage "
                        "WHERE tenant_id = ? AND user_id = ? AND bucket = ? AND period_type = ?",
                        (TENANT_A.bytes, USER_A1.bytes, buckets[tier["tier"]], p["period"])).fetchone()
                    check(row and row[0] > 0 and p["used_credits_micro"] == row[0],
                          f"quota status {tier['tier']}/{p['period']} matches quota_usage", (p, row))
                    check(p["remaining_credits_micro"]
                          == max(0, p["limit_credits_micro"] - p["used_credits_micro"]),
                          "quota remaining = limit - used", p)

            # Outbox: exactly one usage event per provider turn (r1, r2, r3, r4, r5),
            # all dispatched, nothing dead-lettered.
            def queue_count(queue):
                return db.execute(
                    "SELECT COUNT(*) FROM mini_chat_outbox_outgoing o "
                    "JOIN mini_chat_outbox_partitions p ON p.id = o.partition_id WHERE p.queue = ?",
                    (queue,)).fetchone()[0]
            check(queue_count("mini-chat.usage_snapshot") == 5, "five usage outbox events",
                  queue_count("mini-chat.usage_snapshot"))
            wait_until(lambda: db.execute(
                "SELECT COUNT(*) FROM mini_chat_outbox_partitions p JOIN mini_chat_outbox_processor r "
                "ON r.partition_id = p.id WHERE r.processed_seq < p.sequence").fetchone()[0] == 0,
                15, "outbox fully processed")
            dead = db.execute("SELECT COUNT(*) FROM mini_chat_outbox_dead_letters").fetchone()[0]
            check(dead == 0, "no dead-lettered outbox messages", dead)
        finally:
            db.close()


# ---------------------------------------------------------------------------
# driver
# ---------------------------------------------------------------------------

def run_flavour(name, binary, keep):
    home = Path(tempfile.mkdtemp(prefix=f"mini-chat-smoke-{name}-"))
    mock, recorder = mock_openai.start_mock("127.0.0.1", 0, str(home / "mock_requests.jsonl"))
    fl = flavour_settings(name, mock.server_address[1])
    api_port = free_port()
    server = Server(binary, home / "config.yaml", home / "server.log")
    ok = False
    started = time.monotonic()
    print(f"[{name}] run dir {home}", flush=True)
    try:
        render_config(home, api_port, fl)
        server.start()
        http = Http(f"http://127.0.0.1:{api_port}")

        def healthy():
            check(server.proc.poll() is None, "server process is running",
                  f"exit code {server.proc.returncode}")
            try:
                return http.call("GET", "/health", timeout=2)[0] == 200
            except OSError:
                return False
        wait_until(healthy, 60, "server /health", interval=0.5)
        http.bodies.clear()
        Scenario(fl, http, recorder, home).run()
        ok = True
    except Exception as e:  # report and fall through to cleanup
        print(f"[{name}] FAILED: {e}", flush=True)
        if not isinstance(e, SmokeFailure):
            traceback.print_exc()
        print(f"[{name}] --- server log tail ---\n{server.log_tail()}", flush=True)
    finally:
        server.stop()
        mock.shutdown()
        mock.server_close()
    print(f"[{name}] {'PASS' if ok else 'FAIL'} in {time.monotonic() - started:.1f}s", flush=True)
    if ok and not keep:
        shutil.rmtree(home, ignore_errors=True)
    elif not ok:
        print(f"[{name}] run dir kept: {home}", flush=True)
    return ok


def main():
    ap = argparse.ArgumentParser(description="mini-chat black-box smoke test")
    ap.add_argument("--flavour", action="append", choices=["openai", "azure"],
                    help="provider flavour to run (repeatable; default: both)")
    ap.add_argument("--binary", default=str(DEFAULT_BINARY), help="server binary")
    ap.add_argument("--build", action="store_true", help="cargo build the server first")
    ap.add_argument("--keep", action="store_true", help="keep the run directory on success")
    args = ap.parse_args()
    if args.build or not Path(args.binary).exists():
        print("building:", " ".join(BUILD_CMD), flush=True)
        subprocess.run(BUILD_CMD, cwd=str(REPO), check=True)
    results = {f: run_flavour(f, Path(args.binary), args.keep)
               for f in (args.flavour or ["openai", "azure"])}
    print("summary:", ", ".join(f"{k}={'PASS' if v else 'FAIL'}" for k, v in results.items()))
    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
