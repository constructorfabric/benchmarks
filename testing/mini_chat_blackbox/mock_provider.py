#!/usr/bin/env python3
"""Scripted OpenAI-compatible provider for the mini-chat black-box suite.

Implements the subset of the OpenAI API the gear calls through OAGW:

- ``POST /v1/responses``: streams a scripted Responses SSE answer (``Hello from mock``,
  usage 12 in / 3 out). When the request text contains ``[slow]`` the stream holds before the
  first delta until ``POST /_mock/release`` (or ``SLOW_SAFETY_SECS`` as a safety net), which
  keeps a turn ``running`` for as long as a test needs. A release that arrives before the held
  request is kept and consumed by the next ``[slow]`` request (sticky release).
- ``POST /v1/files`` (multipart), ``DELETE /v1/files/{id}``.
- ``POST /v1/vector_stores``, ``POST /v1/vector_stores/{id}/files`` (answers ``in_progress``),
  ``GET /v1/vector_stores/{id}/files/{fid}`` (answers ``completed``),
  ``DELETE /v1/vector_stores/{id}``.

Every request is recorded; the test-only endpoints ``GET /_mock/requests`` (the records),
``POST /_mock/release`` (releases held ``[slow]`` streams) and ``GET /_mock/health`` are not
part of the provider surface.

Usage: ``mock_provider.py --port <port>``.
"""

import argparse
import asyncio
import itertools
import json

from aiohttp import web

# A held ``[slow]`` stream continues by itself after this long (safety net only).
SLOW_SAFETY_SECS = 60.0
ANSWER = ["Hello", " from", " mock"]
USAGE = {"input_tokens": 12, "output_tokens": 3}

_ids = itertools.count(1)
REQUESTS: list = []
# Gates of the held ``[slow]`` streams, opened by ``POST /_mock/release``.
HELD: list = []
# Set by a release that arrived while no ``[slow]`` stream was held; the next held stream
# consumes it instead of waiting, so a release that races ahead of its request is not lost.
# A flag, not a counter: at most one early release is kept, so repeated releases cannot pile up
# and let several later ``[slow]`` requests through.
PENDING_RELEASE = False


def _next_id(prefix: str) -> str:
    return f"{prefix}{next(_ids):06d}"


def _texts(value):
    """All string leaves of a JSON value."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for v in value.values():
            yield from _texts(v)
    elif isinstance(value, list):
        for v in value:
            yield from _texts(v)


@web.middleware
async def record(request: web.Request, handler):
    if not request.path.startswith("/_mock/"):
        entry = {
            "method": request.method,
            "path": request.path,
            "query": request.query_string,
            "content_type": request.content_type,
            "json": None,
        }
        if request.content_type == "application/json" and request.can_read_body:
            try:
                entry["json"] = await request.json()
            except (json.JSONDecodeError, UnicodeDecodeError):
                pass
        REQUESTS.append(entry)
        request["rec"] = entry
    return await handler(request)


def _sse(event: str, data: dict) -> bytes:
    data = dict(data, type=event)
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


async def responses(request: web.Request) -> web.StreamResponse:
    body = await request.json()
    slow = any("[slow]" in t for t in _texts(body.get("input", [])))
    resp = web.StreamResponse(
        status=200,
        headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"},
    )
    await resp.prepare(request)
    response_id = _next_id("resp_")
    await resp.write(_sse("response.created", {"response": {"id": response_id}}))
    if slow:
        await _hold()
    for piece in ANSWER:
        await resp.write(
            _sse(
                "response.output_text.delta",
                {"output_index": 0, "content_index": 0, "delta": piece},
            )
        )
        await asyncio.sleep(0.01)
    text = "".join(ANSWER)
    await resp.write(
        _sse(
            "response.output_text.done",
            {"output_index": 0, "content_index": 0, "text": text},
        )
    )
    await resp.write(
        _sse(
            "response.completed",
            {
                "response": {
                    "id": response_id,
                    "status": "completed",
                    "output": [
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                        }
                    ],
                    "usage": USAGE,
                }
            },
        )
    )
    await resp.write_eof()
    return resp


async def _hold() -> None:
    """Holds a ``[slow]`` stream until released (or a pending release is consumed)."""
    global PENDING_RELEASE
    if PENDING_RELEASE:
        PENDING_RELEASE = False
        return
    gate = asyncio.Event()
    HELD.append(gate)
    try:
        await asyncio.wait_for(gate.wait(), SLOW_SAFETY_SECS)
    except asyncio.TimeoutError:
        pass


async def upload_file(request: web.Request) -> web.Response:
    reader = await request.multipart()
    filename, size = None, 0
    async for part in reader:
        if part.name == "file":
            filename = part.filename
            size = len(await part.read())
        else:
            await part.read()
    request["rec"]["file"] = {"filename": filename, "bytes": size}
    return web.json_response(
        {
            "id": _next_id("file-"),
            "object": "file",
            "bytes": size,
            "filename": filename,
            "purpose": "assistants",
        }
    )


async def delete_file(request: web.Request) -> web.Response:
    return web.json_response(
        {"id": request.match_info["id"], "object": "file", "deleted": True}
    )


async def create_vector_store(_request: web.Request) -> web.Response:
    return web.json_response(
        {"id": _next_id("vs_"), "object": "vector_store", "status": "completed"}
    )


async def add_vector_store_file(request: web.Request) -> web.Response:
    body = await request.json()
    return web.json_response(
        {
            "id": body.get("file_id"),
            "object": "vector_store.file",
            "vector_store_id": request.match_info["id"],
            "status": "in_progress",
        }
    )


async def get_vector_store_file(request: web.Request) -> web.Response:
    return web.json_response(
        {
            "id": request.match_info["fid"],
            "object": "vector_store.file",
            "vector_store_id": request.match_info["id"],
            "status": "completed",
        }
    )


async def delete_vector_store(request: web.Request) -> web.Response:
    return web.json_response(
        {"id": request.match_info["id"], "object": "vector_store.deleted", "deleted": True}
    )


async def recorded(_request: web.Request) -> web.Response:
    return web.json_response(REQUESTS)


async def release(_request: web.Request) -> web.Response:
    """Releases every held stream; with none held, the next ``[slow]`` stream is not held."""
    global PENDING_RELEASE
    released = len(HELD)
    for gate in HELD:
        gate.set()
    HELD.clear()
    if released == 0:
        PENDING_RELEASE = True
    return web.json_response({"released": released, "pending": PENDING_RELEASE})


async def health(_request: web.Request) -> web.Response:
    return web.json_response({"ok": True})


def make_app() -> web.Application:
    app = web.Application(middlewares=[record], client_max_size=128 * 1024 * 1024)
    app.add_routes(
        [
            web.post("/v1/responses", responses),
            web.post("/v1/files", upload_file),
            web.delete("/v1/files/{id}", delete_file),
            web.post("/v1/vector_stores", create_vector_store),
            web.post("/v1/vector_stores/{id}/files", add_vector_store_file),
            web.get("/v1/vector_stores/{id}/files/{fid}", get_vector_store_file),
            web.delete("/v1/vector_stores/{id}", delete_vector_store),
            web.get("/_mock/requests", recorded),
            web.post("/_mock/release", release),
            web.get("/_mock/health", health),
        ]
    )
    return app


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    web.run_app(make_app(), host=args.host, port=args.port, print=None)


if __name__ == "__main__":
    main()
