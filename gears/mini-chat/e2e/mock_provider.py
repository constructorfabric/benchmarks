"""Scriptable OpenAI-compatible mock provider for the mini-chat black-box tests.

Serves the subset of the OpenAI / Azure OpenAI API that mini-chat uses:

* ``POST {prefix}/responses``          Responses API (streaming SSE or JSON)
* ``POST {prefix}/files``              Files API upload (multipart)
* ``DELETE {prefix}/files/{id}``       Files API delete
* ``POST {prefix}/vector_stores``      create vector store
* ``POST {prefix}/vector_stores/{id}/files``       add file
* ``GET {prefix}/vector_stores/{id}/files/{fid}``  file indexing status
* ``DELETE {prefix}/vector_stores/{id}``           delete vector store
* ``POST {prefix}/vector_stores/{id}/search``      vector store search (knowledge search)

``prefix`` may be ``/v1``, ``/openai/v1`` or ``/openai`` (Azure flavours).

Control API (not proxied by the gear; tests call it directly):

* ``POST /__control/reset``      clear recorded requests, scripts and config
* ``POST /__control/responses``  append scripted responses for ``/responses``. A script may carry
  ``match`` (dotted body path → expected value, e.g. ``{"stream": true}`` or
  ``{"metadata.request_type": "summary"}``): it is then only used for matching requests (first
  matching script wins; scripts without ``match`` match any request). ``pre_delay_ms`` delays the
  response before any byte is sent (gateway timeouts); stream scripts support ``delay_ms`` per event,
  ``{"sleep_ms": n}`` pseudo-events, ``{"raw": "..."}`` frames and ``hang_ms`` before EOF.
* ``POST /__control/config``     update behaviour knobs (see ``DEFAULT_CONFIG``)
* ``GET  /__control/requests``   list recorded requests
* ``GET  /__control/stats``      ``{cancelled, scripts_left}`` (cancelled = streams dropped by the client)
"""

from __future__ import annotations

import asyncio
import json
import random
import string
import threading
from typing import Any

from aiohttp import web

DEFAULT_CONFIG: dict[str, Any] = {
    # status returned by POST/GET vector_stores/{id}/files
    "vector_store_file_status": "completed",
    # HTTP status for POST /files (200 = success)
    "file_upload_status": 200,
    # HTTP status for DELETE /files/{id} and DELETE /vector_stores/{id}
    "file_delete_status": 200,
    "vector_store_delete_status": 200,
    # default streamed text and usage
    "default_text": ["Hello", " from", " mock"],
    "default_usage": {"input_tokens": 100, "output_tokens": 50},
    "default_summary": "<analysis>thinking</analysis><summary>Conversation summary text.</summary>",
    # POST vector_stores/{id}/search (knowledge search): HTTP status and result items
    # (OpenAI vector store search format: file_id, filename, score, content[{type, text}])
    "vector_store_search_status": 200,
    "vector_store_search_results": [],
}


def _rand(n: int) -> str:
    return "".join(random.choice(string.ascii_lowercase + string.digits) for _ in range(n))


def _sse(event: str, data: Any) -> bytes:
    payload = data if isinstance(data, str) else json.dumps(data)
    return f"event: {event}\ndata: {payload}\n\n".encode()


def default_stream_events(text: list[str], usage: dict[str, int], resp_id: str | None = None) -> list[dict]:
    resp_id = resp_id or f"resp_{_rand(24)}"
    events: list[dict] = [
        {"event": "response.created", "data": {"type": "response.created", "response": {"id": resp_id, "status": "in_progress"}}},
    ]
    for chunk in text:
        events.append({"event": "response.output_text.delta", "data": {"type": "response.output_text.delta", "delta": chunk}})
    events.append(
        {
            "event": "response.completed",
            "data": {
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "status": "completed",
                    "output": [
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "".join(text), "annotations": []}],
                        }
                    ],
                    "usage": usage,
                },
            },
        }
    )
    return events


class MockProvider:
    def __init__(self) -> None:
        self.requests: list[dict] = []
        self.scripts: list[dict] = []
        self.config: dict[str, Any] = dict(DEFAULT_CONFIG)
        self.files: dict[str, dict] = {}
        self.vector_stores: dict[str, dict] = {}
        self.inflight_cancelled = 0
        self._lock = threading.Lock()

    # ── helpers ────────────────────────────────────────────────────────
    @staticmethod
    def _normalize(path: str) -> str:
        for prefix in ("/openai/v1", "/openai", "/v1"):
            if path.startswith(prefix + "/"):
                return path[len(prefix):]
        return path

    async def _record(self, request: web.Request, body: Any) -> None:
        self.requests.append(
            {
                "method": request.method,
                "path": request.path,
                "query": dict(request.query),
                "headers": {k.lower(): v for k, v in request.headers.items()},
                "body": body,
            }
        )

    # ── control ────────────────────────────────────────────────────────
    async def control(self, request: web.Request) -> web.StreamResponse:
        action = request.match_info["action"]
        if action == "reset" and request.method == "POST":
            self.requests.clear()
            self.scripts.clear()
            self.config = dict(DEFAULT_CONFIG)
            self.inflight_cancelled = 0
            return web.json_response({"ok": True})
        if action == "responses" and request.method == "POST":
            body = await request.json()
            items = body if isinstance(body, list) else [body]
            self.scripts.extend(items)
            return web.json_response({"queued": len(self.scripts)})
        if action == "config" and request.method == "POST":
            self.config.update(await request.json())
            return web.json_response(self.config)
        if action == "requests" and request.method == "GET":
            return web.json_response(self.requests)
        if action == "stats" and request.method == "GET":
            return web.json_response({"cancelled": self.inflight_cancelled, "scripts_left": len(self.scripts)})
        return web.json_response({"error": "unknown control action"}, status=404)

    # ── proxied API ────────────────────────────────────────────────────
    async def handle(self, request: web.Request) -> web.StreamResponse:
        path = self._normalize(request.path)
        if path == "/responses" and request.method == "POST":
            return await self._responses(request)
        if path == "/files" and request.method == "POST":
            return await self._upload_file(request)
        if path.startswith("/files/") and request.method == "DELETE":
            await self._record(request, None)
            status = int(self.config["file_delete_status"])
            if status >= 300:
                return web.json_response({"error": {"message": "delete failed"}}, status=status)
            return web.json_response({"id": path.split("/")[2], "object": "file", "deleted": True})
        if path == "/vector_stores" and request.method == "POST":
            body = await request.json()
            await self._record(request, body)
            vs_id = f"vs_{_rand(24)}"
            self.vector_stores[vs_id] = {"files": {}}
            return web.json_response({"id": vs_id, "object": "vector_store", "status": "completed"})
        parts = path.split("/")
        if len(parts) >= 3 and parts[1] == "vector_stores":
            vs_id = parts[2]
            if request.method == "DELETE" and len(parts) == 3:
                await self._record(request, None)
                status = int(self.config["vector_store_delete_status"])
                if status >= 300:
                    return web.json_response({"error": {"message": "vs delete failed"}}, status=status)
                return web.json_response({"id": vs_id, "deleted": True})
            if request.method == "POST" and len(parts) == 4 and parts[3] == "files":
                body = await request.json()
                await self._record(request, body)
                return web.json_response(
                    {"id": body.get("file_id"), "object": "vector_store.file", "status": self.config["vector_store_file_status"]}
                )
            if request.method == "GET" and len(parts) == 5 and parts[3] == "files":
                await self._record(request, None)
                return web.json_response(
                    {"id": parts[4], "object": "vector_store.file", "status": self.config["vector_store_file_status"]}
                )
            if request.method == "POST" and len(parts) == 4 and parts[3] == "search":
                body = await request.json()
                await self._record(request, body)
                status = int(self.config["vector_store_search_status"])
                if status >= 300:
                    return web.json_response({"error": {"message": "search failed"}}, status=status)
                return web.json_response(
                    {
                        "object": "vector_store.search_results.page",
                        "search_query": body.get("query"),
                        "data": list(self.config["vector_store_search_results"]),
                        "has_more": False,
                        "next_page": None,
                    }
                )
        await self._record(request, await request.text())
        return web.json_response({"error": {"message": f"mock: unsupported {request.method} {request.path}"}}, status=404)

    async def _upload_file(self, request: web.Request) -> web.StreamResponse:
        reader = await request.multipart()
        fields: dict[str, Any] = {}
        while True:
            part = await reader.next()
            if part is None:
                break
            if part.filename:
                data = await part.read()
                fields[part.name] = {"filename": part.filename, "size": len(data), "content_type": part.headers.get("Content-Type")}
            else:
                fields[part.name] = (await part.read()).decode(errors="replace")
        await self._record(request, fields)
        status = int(self.config["file_upload_status"])
        if status >= 300:
            return web.json_response({"error": {"message": "upload failed for file-abcdefghijklmnopqrstu"}}, status=status)
        file_id = f"file-{_rand(24)}"
        info = fields.get("file", {})
        self.files[file_id] = info
        return web.json_response(
            {"id": file_id, "object": "file", "bytes": info.get("size", 0), "filename": info.get("filename"), "purpose": fields.get("purpose")}
        )

    async def _responses(self, request: web.Request) -> web.StreamResponse:
        body = await request.json()
        await self._record(request, body)
        streaming = bool(body.get("stream"))
        script = self._take_script(body)
        if script is None:
            if streaming:
                script = {"kind": "stream", "events": default_stream_events(self.config["default_text"], self.config["default_usage"])}
            else:
                script = {"kind": "json", "body": self._summary_body()}
        kind = script.get("kind", "stream")
        if script.get("pre_delay_ms"):
            # Delay before any response bytes (e.g. to trigger a gateway timeout).
            await asyncio.sleep(script["pre_delay_ms"] / 1000)
        if kind == "http_error":
            return web.json_response(
                script.get("body", {"error": {"message": "mock error", "type": "server_error"}}),
                status=int(script.get("status", 500)),
                headers=script.get("headers", {}),
            )
        if kind == "json":
            await asyncio.sleep(script.get("delay_ms", 0) / 1000)
            return web.json_response(script.get("body") or self._summary_body())
        # streaming
        resp = web.StreamResponse(status=200, headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"})
        await resp.prepare(request)
        delay = script.get("delay_ms", 0) / 1000
        events = script.get("events")
        if events is None:
            events = default_stream_events(script.get("text", self.config["default_text"]), script.get("usage", self.config["default_usage"]))
        try:
            for ev in events:
                if ev.get("sleep_ms"):
                    await asyncio.sleep(ev["sleep_ms"] / 1000)
                    continue
                if delay:
                    await asyncio.sleep(delay)
                if "raw" in ev:
                    await resp.write(ev["raw"].encode())
                else:
                    await resp.write(_sse(ev["event"], ev["data"]))
            if script.get("hang_ms"):
                await asyncio.sleep(script["hang_ms"] / 1000)
            await resp.write_eof()
        except (ConnectionResetError, asyncio.CancelledError):
            self.inflight_cancelled += 1
            raise
        return resp

    @staticmethod
    def _lookup(body: Any, dotted: str) -> Any:
        cur = body
        for part in dotted.split("."):
            if not isinstance(cur, dict) or part not in cur:
                return None
            cur = cur[part]
        return cur

    def _take_script(self, body: dict) -> dict | None:
        """Pops the first queued script whose optional ``match`` matches the request body.

        ``match`` maps dotted body paths to expected values, e.g. ``{"stream": true}`` or
        ``{"metadata.request_type": "summary"}``. Scripts without ``match`` match any request.
        """
        for i, script in enumerate(self.scripts):
            cond = script.get("match") or {}
            if all(self._lookup(body, k) == v for k, v in cond.items()):
                return self.scripts.pop(i)
        return None

    def _summary_body(self) -> dict:
        return {
            "id": f"resp_{_rand(24)}",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": self.config["default_summary"], "annotations": []}],
                }
            ],
            "usage": {"input_tokens": 500, "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 0}},
        }


def build_app(provider: MockProvider) -> web.Application:
    app = web.Application(client_max_size=64 * 1024 * 1024)
    app.router.add_route("*", "/__control/{action}", provider.control)
    app.router.add_route("*", "/{tail:.*}", provider.handle)
    return app


class MockProviderServer:
    """Runs the mock in a background thread with its own event loop."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0) -> None:
        self.host = host
        self.port = port
        self.provider = MockProvider()
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: threading.Thread | None = None
        self._runner: web.AppRunner | None = None
        self._ready = threading.Event()

    def start(self) -> None:
        def run() -> None:
            self._loop = asyncio.new_event_loop()
            asyncio.set_event_loop(self._loop)
            self._runner = web.AppRunner(build_app(self.provider))
            self._loop.run_until_complete(self._runner.setup())
            site = web.TCPSite(self._runner, self.host, self.port)
            self._loop.run_until_complete(site.start())
            sockets = site._server.sockets  # noqa: SLF001
            self.port = sockets[0].getsockname()[1]
            self._ready.set()
            self._loop.run_forever()

        self._thread = threading.Thread(target=run, daemon=True)
        self._thread.start()
        self._ready.wait(10)

    def stop(self) -> None:
        if self._loop and self._runner:
            fut = asyncio.run_coroutine_threadsafe(self._runner.cleanup(), self._loop)
            try:
                fut.result(5)
            except Exception:  # noqa: BLE001
                pass
            self._loop.call_soon_threadsafe(self._loop.stop)


if __name__ == "__main__":
    import sys

    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18090
    web.run_app(build_app(MockProvider()), host="127.0.0.1", port=port)
