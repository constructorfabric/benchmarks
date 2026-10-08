"""Scriptable OpenAI-compatible mock provider for the mini-chat E2E suite.

Implements the parts of the OpenAI / Azure OpenAI API that mini-chat calls
through OAGW:

* ``POST {prefix}/responses`` (streaming SSE and non-streaming JSON)
* ``POST {prefix}/files``, ``DELETE {prefix}/files/{id}``
* ``POST {prefix}/vector_stores``, ``DELETE {prefix}/vector_stores/{id}``
* ``POST {prefix}/vector_stores/{id}/files``,
  ``GET {prefix}/vector_stores/{id}/files/{file_id}``
* ``POST {prefix}/vector_stores/{id}/search`` (knowledge search)

``prefix`` is ``/v1`` (OpenAI) or ``/openai`` / ``/openai/v1`` (Azure).

Every request is recorded and can be read back through ``GET /__mock/requests``.
Responses to ``/responses`` are taken from a FIFO script queue
(``POST /__mock/responses``); when the queue is empty a default completion is
streamed. Behaviour of the file and vector-store endpoints is configured with
``POST /__mock/config``.
"""

from __future__ import annotations

import asyncio
import itertools
import json
import socket
import threading
import time
from typing import Any

from aiohttp import web

_ids = itertools.count(1)


def _new_id(prefix: str) -> str:
    return f"{prefix}{next(_ids):012d}abcdef"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


DEFAULT_CONFIG: dict[str, Any] = {
    # status returned by POST vector_stores/{id}/files and the GET poll
    "vs_file_status": "completed",
    # list of statuses returned by consecutive GET polls (overrides vs_file_status)
    "vs_file_poll_statuses": [],
    "files_upload_status": 200,
    "files_delete_status": 200,
    "vector_store_create_status": 200,
    "vector_store_delete_status": 200,
    "vs_file_add_status": 200,
    # POST vector_stores/{id}/search (knowledge search)
    "vs_search_status": 200,
    "vs_search_results": [
        {
            "file_id": "file-kb0000000000001",
            "filename": "handbook.pdf",
            "score": 0.91,
            "content": [{"type": "text", "text": "Employees get 25 vacation days per year."}],
        }
    ],
}


class MockState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.reset()

    def reset(self) -> None:
        with self.lock:
            self.requests: list[dict[str, Any]] = []
            self.scripts: list[dict[str, Any]] = []
            self.summary_scripts: list[dict[str, Any]] = []
            self.config: dict[str, Any] = dict(DEFAULT_CONFIG)
            self.config["vs_file_poll_statuses"] = []
            self.active_streams = 0
            self.closed_streams = 0
            self.files: dict[str, dict[str, Any]] = {}

    def record(self, entry: dict[str, Any]) -> None:
        with self.lock:
            self.requests.append(entry)

    def next_script(self, summary: bool = False, chat_id: str | None = None) -> dict[str, Any] | None:
        with self.lock:
            queue = self.summary_scripts if summary else self.scripts
            for i, item in enumerate(queue):
                # an entry with "for_chat" only answers requests of that chat
                if item.get("for_chat") in (None, chat_id):
                    return queue.pop(i)
            return None


STATE = MockState()


async def _record(request: web.Request, body: Any) -> None:
    STATE.record(
        {
            "method": request.method,
            "path": request.path,
            "query": dict(request.query),
            "headers": {k.lower(): v for k, v in request.headers.items()},
            "json": body if isinstance(body, (dict, list)) else None,
            "raw": body if isinstance(body, str) else None,
            "ts": time.time(),
        }
    )


def _sse(event: str, data: dict[str, Any]) -> bytes:
    data = dict(data)
    data.setdefault("type", event)
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


def _usage(script: dict[str, Any]) -> dict[str, Any] | None:
    if "usage" in script:
        return script["usage"]
    return {
        "input_tokens": 42,
        "output_tokens": 7,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0},
    }


def _response_obj(script: dict[str, Any], text: str, status: str) -> dict[str, Any]:
    content: dict[str, Any] = {"type": "output_text", "text": text, "annotations": script.get("annotations", [])}
    obj: dict[str, Any] = {
        "id": script.get("response_id", _new_id("resp_")),
        "object": "response",
        "status": status,
        "output": [{"type": "message", "role": "assistant", "content": [content]}],
    }
    usage = _usage(script)
    if usage is not None:
        obj["usage"] = usage
    if status == "incomplete":
        obj["incomplete_details"] = {"reason": script.get("incomplete_reason", "max_output_tokens")}
    return obj


async def handle_responses(request: web.Request) -> web.StreamResponse:
    body = await request.json()
    await _record(request, body)
    is_summary = (body.get("metadata") or {}).get("request_type") == "summary"
    chat_id = (body.get("metadata") or {}).get("chat_id")
    script = STATE.next_script(summary=is_summary, chat_id=chat_id) or {}
    if is_summary and "text" not in script and "http_status" not in script:
        script["text"] = "<analysis>reviewing</analysis>\n<summary>Mock summary of the conversation.</summary>"
    if script.get("initial_delay_ms"):
        await asyncio.sleep(script["initial_delay_ms"] / 1000)
    http_status = script.get("http_status", 200)
    if http_status != 200:
        headers = {}
        if "retry_after" in script:
            headers["Retry-After"] = str(script["retry_after"])
        err = script.get("error", {"message": "mock failure", "type": "server_error", "code": "mock_error"})
        return web.json_response({"error": err}, status=http_status, headers=headers)

    text = script.get("text", "Hello from the mock provider.")
    if not body.get("stream"):
        return web.json_response(_response_obj(script, text, "completed"))

    resp = web.StreamResponse(status=200, headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"})
    await resp.prepare(request)
    with STATE.lock:
        STATE.active_streams += 1
    try:
        resp_id = script.get("response_id", _new_id("resp_"))
        script["response_id"] = resp_id
        await resp.write(_sse("response.created", {"response": {"id": resp_id, "status": "in_progress"}}))
        for ev in script.get("events_before", []):
            await resp.write(_sse(ev["type"], ev))
            if ev.get("delay_ms"):
                await asyncio.sleep(ev["delay_ms"] / 1000)
        chunks = script.get("chunks")
        if chunks is None:
            chunks = [text[i : i + 8] for i in range(0, len(text), 8)] if text else []
        delay = script.get("chunk_delay_ms", 0) / 1000
        for chunk in chunks:
            await resp.write(_sse("response.output_text.delta", {"delta": chunk, "output_index": 0, "content_index": 0}))
            if delay:
                await asyncio.sleep(delay)
        for ev in script.get("events_after", []):
            await resp.write(_sse(ev["type"], ev))
        terminal = script.get("terminal", "completed")
        full = "".join(chunks)
        if terminal == "completed":
            await resp.write(_sse("response.completed", {"response": _response_obj(script, full, "completed")}))
        elif terminal == "incomplete":
            await resp.write(_sse("response.incomplete", {"response": _response_obj(script, full, "incomplete")}))
        elif terminal == "failed":
            obj = _response_obj(script, full, "failed")
            obj["error"] = script.get("error", {"code": "server_error", "message": "The model failed"})
            if not script.get("failed_usage", False):
                obj.pop("usage", None)
            await resp.write(_sse("response.failed", {"response": obj}))
        elif terminal == "error_event":
            await resp.write(_sse("error", script.get("error", {"code": "server_error", "message": "boom"})))
        elif terminal == "hang":
            # SSE comments keep probing the connection so a closed client is noticed.
            end = time.time() + script.get("hang_secs", 600)
            while time.time() < end:
                await asyncio.sleep(0.2)
                await resp.write(b": waiting\n\n")
        # terminal == "none": close the stream without a terminal event
        await resp.write_eof()
    except (ConnectionResetError, asyncio.CancelledError):
        pass
    finally:
        with STATE.lock:
            STATE.active_streams -= 1
            STATE.closed_streams += 1
    return resp


async def handle_files_upload(request: web.Request) -> web.Response:
    reader = await request.multipart()
    fields: dict[str, Any] = {}
    size = 0
    filename = None
    async for part in reader:
        if part.name == "file":
            filename = part.filename
            data = await part.read()
            size = len(data)
            fields["file_content_type"] = part.headers.get("Content-Type")
        else:
            fields[part.name] = (await part.read()).decode()
    fields["filename"] = filename
    fields["size"] = size
    await _record(request, fields)
    status = STATE.config["files_upload_status"]
    if status != 200:
        return web.json_response({"error": {"message": "upload failed"}}, status=status)
    fid = _new_id("file-")
    with STATE.lock:
        STATE.files[fid] = {"filename": filename, "bytes": size}
    return web.json_response({"id": fid, "object": "file", "bytes": size, "filename": filename, "purpose": fields.get("purpose")})


async def handle_files_delete(request: web.Request) -> web.Response:
    await _record(request, None)
    status = STATE.config["files_delete_status"]
    if status != 200:
        return web.json_response({"error": {"message": "delete failed"}}, status=status)
    fid = request.match_info["file_id"]
    return web.json_response({"id": fid, "object": "file", "deleted": True})


async def handle_vs_create(request: web.Request) -> web.Response:
    try:
        body = await request.json()
    except Exception:  # noqa: BLE001
        body = {}
    await _record(request, body)
    status = STATE.config["vector_store_create_status"]
    if status != 200:
        return web.json_response({"error": {"message": "vs create failed"}}, status=status)
    return web.json_response({"id": _new_id("vs_"), "object": "vector_store", "status": "completed"})


async def handle_vs_delete(request: web.Request) -> web.Response:
    await _record(request, None)
    status = STATE.config["vector_store_delete_status"]
    if status != 200:
        return web.json_response({"error": {"message": "vs delete failed"}}, status=status)
    return web.json_response({"id": request.match_info["vs_id"], "deleted": True})


def _vs_status() -> str:
    with STATE.lock:
        polls = STATE.config.get("vs_file_poll_statuses") or []
        if polls:
            return polls.pop(0) if len(polls) > 1 else polls[0]
        return STATE.config["vs_file_status"]


async def handle_vs_file_add(request: web.Request) -> web.Response:
    body = await request.json()
    await _record(request, body)
    status = STATE.config["vs_file_add_status"]
    if status != 200:
        return web.json_response({"error": {"message": "vs add failed"}}, status=status)
    return web.json_response({"id": body.get("file_id"), "object": "vector_store.file", "status": _vs_status()})


async def handle_vs_file_get(request: web.Request) -> web.Response:
    await _record(request, None)
    return web.json_response({"id": request.match_info["file_id"], "object": "vector_store.file", "status": _vs_status()})


async def handle_vs_search(request: web.Request) -> web.Response:
    body = await request.json()
    await _record(request, body)
    status = STATE.config["vs_search_status"]
    if status != 200:
        return web.json_response({"error": {"message": "search failed"}}, status=status)
    return web.json_response({"object": "vector_store.search_results.page", "data": STATE.config["vs_search_results"]})


# ── control API ────────────────────────────────────────────────────────────


async def ctl_requests(request: web.Request) -> web.Response:
    with STATE.lock:
        return web.json_response(list(STATE.requests))


async def ctl_reset(request: web.Request) -> web.Response:
    STATE.reset()
    return web.json_response({"ok": True})


async def ctl_scripts(request: web.Request) -> web.Response:
    body = await request.json()
    items = body if isinstance(body, list) else [body]
    with STATE.lock:
        STATE.scripts.extend(items)
    return web.json_response({"queued": len(items)})


async def ctl_summary_scripts(request: web.Request) -> web.Response:
    body = await request.json()
    items = body if isinstance(body, list) else [body]
    with STATE.lock:
        STATE.summary_scripts.extend(items)
    return web.json_response({"queued": len(items)})


async def ctl_config(request: web.Request) -> web.Response:
    body = await request.json()
    with STATE.lock:
        STATE.config.update(body)
    return web.json_response(STATE.config)


async def ctl_stats(request: web.Request) -> web.Response:
    with STATE.lock:
        return web.json_response({"active_streams": STATE.active_streams, "closed_streams": STATE.closed_streams})


def build_app() -> web.Application:
    app = web.Application(client_max_size=128 * 1024 * 1024)
    for prefix in ("/v1", "/openai", "/openai/v1"):
        app.router.add_post(f"{prefix}/responses", handle_responses)
        app.router.add_post(f"{prefix}/files", handle_files_upload)
        app.router.add_delete(prefix + "/files/{file_id}", handle_files_delete)
        app.router.add_post(f"{prefix}/vector_stores", handle_vs_create)
        app.router.add_delete(prefix + "/vector_stores/{vs_id}", handle_vs_delete)
        app.router.add_post(prefix + "/vector_stores/{vs_id}/files", handle_vs_file_add)
        app.router.add_get(prefix + "/vector_stores/{vs_id}/files/{file_id}", handle_vs_file_get)
        app.router.add_post(prefix + "/vector_stores/{vs_id}/search", handle_vs_search)
    app.router.add_get("/__mock/requests", ctl_requests)
    app.router.add_post("/__mock/reset", ctl_reset)
    app.router.add_post("/__mock/responses", ctl_scripts)
    app.router.add_post("/__mock/summary_responses", ctl_summary_scripts)
    app.router.add_post("/__mock/config", ctl_config)
    app.router.add_get("/__mock/stats", ctl_stats)
    return app


class MockLlmServer:
    """Runs the mock in a background thread with its own event loop."""

    def __init__(self, port: int | None = None) -> None:
        self.port = port or free_port()
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: threading.Thread | None = None
        self._runner: web.AppRunner | None = None
        self._ready = threading.Event()

    def start(self) -> None:
        def run() -> None:
            loop = asyncio.new_event_loop()
            self._loop = loop
            asyncio.set_event_loop(loop)
            self._runner = web.AppRunner(build_app())
            loop.run_until_complete(self._runner.setup())
            site = web.TCPSite(self._runner, "127.0.0.1", self.port)
            loop.run_until_complete(site.start())
            try:  # also answer on ::1 so "localhost" upstreams work on IPv6-first hosts
                site6 = web.TCPSite(self._runner, "::1", self.port)
                loop.run_until_complete(site6.start())
            except OSError:
                pass
            self._ready.set()
            loop.run_forever()

        self._thread = threading.Thread(target=run, daemon=True)
        self._thread.start()
        self._ready.wait(10)

    def stop(self) -> None:
        if self._loop is not None:
            self._loop.call_soon_threadsafe(self._loop.stop)


if __name__ == "__main__":  # manual run: python mock_llm.py 18080
    import sys

    srv = MockLlmServer(int(sys.argv[1]) if len(sys.argv) > 1 else 18080)
    srv.start()
    print(f"mock llm on {srv.port}")
    while True:
        time.sleep(3600)
