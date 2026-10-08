"""OpenAI-compatible mock provider for the mini-chat E2E suite.

Serves the Responses API (SSE and JSON), the Files API and the Vector Stores
API under both ``/v1`` and ``/openai`` prefixes. Every request is recorded and
can be read back through ``GET /__mock/requests``.

Behaviour of a chat request is selected by directives in the text of the
last user message:

    #slow          stream 40 deltas, 100 ms apart
    #hang          send response.created, then nothing (connection kept open)
    #fail          response.failed with a message containing provider ids
    #http429       HTTP 429 with Retry-After: 7
    #http500       HTTP 500 with a JSON error body
    #incomplete    response.incomplete (reason max_output_tokens)
    #noterminal    end the stream without a terminal event
    #websearch     one web_search call and a url_citation annotation
    #websearch3    three web_search calls (exceeds the default limit of 2)
    #filecite      a file_search call and a file_citation for every
                   vector-store file of the request
    #ci            a code_interpreter call with logs output
    #ci11          eleven code_interpreter calls (exceeds the default 10)
    #nousage       response.completed without usage
    #fn            a function_call (search_knowledge) instead of text
    #empty         response.completed without any text
"""

from __future__ import annotations

import asyncio
import json
import secrets
import time
import sys
from typing import Any

from aiohttp import web

REQUESTS: list[dict[str, Any]] = []
FILES: dict[str, dict[str, Any]] = {}
VECTOR_STORES: dict[str, dict[str, Any]] = {}
# Status overrides per file content marker.
CONFIG: dict[str, Any] = {"files_fail": False, "vs_create_fail": False, "delete_fail": False}


def _rand(prefix: str, n: int = 24) -> str:
    return prefix + secrets.token_hex(n // 2)


async def _record(request: web.Request, body: Any) -> None:
    REQUESTS.append(
        {
            "method": request.method,
            "path": request.path,
            "query": dict(request.query),
            "headers": {k.lower(): v for k, v in request.headers.items()},
            "body": body,
        }
    )


def _last_user_text(body: dict[str, Any]) -> str:
    items = body.get("input") or []
    text = ""
    for it in items:
        if not isinstance(it, dict) or it.get("role") != "user":
            continue
        content = it.get("content")
        if isinstance(content, str):
            text = content
        elif isinstance(content, list):
            text = " ".join(c.get("text", "") for c in content if isinstance(c, dict))
    return text


def _sse(event: str, data: dict[str, Any]) -> bytes:
    data = dict(data)
    data.setdefault("type", event)
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


def _usage(inp: int = 21, out: int = 7) -> dict[str, Any]:
    return {
        "input_tokens": inp,
        "output_tokens": out,
        "total_tokens": inp + out,
        "input_tokens_details": {"cached_tokens": 3},
        "output_tokens_details": {"reasoning_tokens": 1},
    }


async def responses(request: web.Request) -> web.StreamResponse:
    try:
        body = await request.json()
    except Exception:  # noqa: BLE001
        body = None
    await _record(request, body)
    body = body or {}
    text = _last_user_text(body)
    resp_id = _rand("resp_")

    if "#http429" in text:
        return web.json_response(
            {"error": {"message": "Rate limit reached for requests", "code": "rate_limit_exceeded"}},
            status=429,
            headers={"Retry-After": "7"},
        )
    if "#http500" in text:
        return web.json_response(
            {"error": {"message": f"Internal failure in {resp_id}", "code": "server_error"}}, status=500
        )

    if not body.get("stream"):
        # Non-streaming (thread summary).
        if "#sumfail" in json.dumps(body.get("input")):
            return web.json_response({"error": {"message": "summary failed"}}, status=500)
        out = "<analysis>reviewing</analysis>\n<summary>\nSUMMARY: the user talked about testing.\n</summary>"
        return web.json_response(
            {
                "id": resp_id,
                "object": "response",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": out, "annotations": []}],
                    }
                ],
                "usage": _usage(500, 120),
            }
        )

    resp = web.StreamResponse(status=200, headers={"Content-Type": "text/event-stream"})
    await resp.prepare(request)

    async def send(ev: str, data: dict[str, Any]) -> None:
        await resp.write(_sse(ev, data))

    await send("response.created", {"response": {"id": resp_id, "status": "in_progress"}})

    if "#hang" in text:
        try:
            await asyncio.sleep(3600)
        finally:
            return resp

    if "#fail" in text:
        await send("response.output_text.delta", {"delta": "partial "})
        await send(
            "response.failed",
            {
                "response": {
                    "id": resp_id,
                    "status": "failed",
                    "error": {
                        "code": "server_error",
                        "message": f"Upstream exploded for {resp_id} file-abcdefabcdef1234 see https://status.example.com/x key sk-abcdefghijklmnop",
                    },
                    "usage": _usage(30, 2),
                }
            },
        )
        return resp

    if "#fn" in text:
        await send(
            "response.output_item.done",
            {"item": {"type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": "{\"query\":\"x\"}"}},
        )
        await send("response.completed", {"response": {"id": resp_id, "status": "completed", "usage": _usage()}})
        return resp

    annotations: list[dict[str, Any]] = []
    answer = "Hello from the mock provider."
    if "#empty" in text:
        answer = ""

    if "#websearch3" in text:
        for i in range(3):
            await send("response.web_search_call.searching", {"item_id": f"ws_{i}"})
            await send("response.web_search_call.completed", {"item_id": f"ws_{i}"})
    elif "#websearch" in text:
        await send("response.web_search_call.searching", {"item_id": "ws_1"})
        await send("response.web_search_call.completed", {"item_id": "ws_1"})
        answer = "According to the web, the sky is blue."
        annotations.append(
            {
                "type": "url_citation",
                "url": "https://example.com/sky",
                "title": "Sky facts",
                "start_index": 15,
                "end_index": 37,
            }
        )

    if "#ci11" in text:
        for i in range(11):
            await send("response.code_interpreter_call.in_progress", {"item_id": f"ci_{i}"})
    elif "#ci" in text:
        await send("response.code_interpreter_call.in_progress", {"item_id": "ci_1"})
        await send(
            "response.output_item.done",
            {"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "42"}]}},
        )

    if "#filecite" in text:
        await send("response.file_search_call.searching", {"item_id": "fs_1"})
        await send("response.file_search_call.completed", {"item_id": "fs_1"})
        vs_ids: list[str] = []
        for t in body.get("tools") or []:
            if t.get("type") == "file_search":
                vs_ids.extend(t.get("vector_store_ids") or [])
        for vs in vs_ids:
            for fid in VECTOR_STORES.get(vs, {}).get("files", {}):
                annotations.append(
                    {"type": "file_citation", "file_id": fid, "filename": FILES.get(fid, {}).get("filename", "x"), "index": 3}
                )
        # A citation of an unknown file must be dropped.
        annotations.append({"type": "file_citation", "file_id": "file-unknown0000000000", "filename": "zz", "index": 1})
        answer = "The document says hello."

    if "#think" in text:
        for chunk in ["<thi", "nk>ponder", "ing</think>The ", "answer."]:
            await send("response.output_text.delta", {"delta": chunk})
        answer = ""
    if "#slow" in text:
        for i in range(40):
            await send("response.output_text.delta", {"delta": f"w{i} "})
            await asyncio.sleep(0.1)
        answer = ""
    elif answer:
        for chunk in [answer[: len(answer) // 2], answer[len(answer) // 2 :]]:
            await send("response.output_text.delta", {"delta": chunk})

    if "#noterminal" in text:
        return resp

    output = [
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": answer, "annotations": annotations}],
        }
    ]
    if "#incomplete" in text:
        await send(
            "response.incomplete",
            {
                "response": {
                    "id": resp_id,
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": _usage(),
                }
            },
        )
        return resp
    final: dict[str, Any] = {"id": resp_id, "status": "completed", "output": output}
    if "#nousage" not in text:
        final["usage"] = _usage()
    await send("response.completed", {"response": final})
    return resp


async def chat_completions(request: web.Request) -> web.StreamResponse:
    body = await request.json()
    await _record(request, body)
    text = ""
    for m in body.get("messages") or []:
        if m.get("role") == "user":
            text = m.get("content") if isinstance(m.get("content"), str) else json.dumps(m.get("content"))
    if "#http429" in text:
        return web.json_response({"error": {"message": "slow down"}}, status=429, headers={"Retry-After": "3"})
    cid = _rand("chatcmpl-")
    resp = web.StreamResponse(status=200, headers={"Content-Type": "text/event-stream"})
    await resp.prepare(request)

    async def chunk(data: dict[str, Any]) -> None:
        await resp.write(f"data: {json.dumps(data)}\n\n".encode())

    for part in ["Hello from ", "chat completions."]:
        await chunk({"id": cid, "choices": [{"index": 0, "delta": {"content": part}, "finish_reason": None}]})
    finish = "length" if "#incomplete" in text else "stop"
    await chunk({"id": cid, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
    await chunk({"id": cid, "choices": [], "usage": {"prompt_tokens": 13, "completion_tokens": 4, "total_tokens": 17}})
    await resp.write(b"data: [DONE]\n\n")
    return resp


async def files_create(request: web.Request) -> web.Response:
    reader = await request.multipart()
    fields: dict[str, Any] = {}
    content = b""
    filename = None
    content_type = None
    while True:
        part = await reader.next()
        if part is None:
            break
        if part.name == "file":
            filename = part.filename
            content_type = part.headers.get("Content-Type")
            content = await part.read()
        else:
            fields[part.name] = (await part.read()).decode()
    await _record(
        request,
        {"fields": fields, "filename": filename, "content_type": content_type, "size": len(content)},
    )
    if CONFIG["files_fail"] or b"UPLOAD_FAIL" in content:
        return web.json_response({"error": {"message": "upload failed"}}, status=500)
    fid = _rand("file-")
    FILES[fid] = {"filename": filename, "content": content, "content_type": content_type}
    return web.json_response({"id": fid, "object": "file", "filename": filename, "bytes": len(content)})


async def files_delete(request: web.Request) -> web.Response:
    await _record(request, None)
    fid = request.match_info["fid"]
    if CONFIG["delete_fail"]:
        return web.json_response({"error": {"message": "delete failed"}}, status=500)
    if fid not in FILES:
        return web.json_response({"error": {"message": "not found"}}, status=404)
    FILES.pop(fid, None)
    return web.json_response({"id": fid, "deleted": True})


async def vs_create(request: web.Request) -> web.Response:
    body = await request.json()
    await _record(request, body)
    if CONFIG["vs_create_fail"]:
        return web.json_response({"error": {"message": "vs failed"}}, status=500)
    vs = _rand("vs_")
    VECTOR_STORES[vs] = {"files": {}, "name": body.get("name")}
    return web.json_response({"id": vs, "object": "vector_store"})


def _initial_status(fid: str) -> str:
    content = FILES.get(fid, {}).get("content", b"")
    if b"INDEX_FAIL" in content:
        return "failed"
    if b"INDEX_SLOW" in content:
        return "in_progress"
    if b"INDEX_LATE" in content:
        return "in_progress_then_completed"
    if b"INDEX_BGFAIL" in content:
        return "bg_failed"
    if b"INDEX_BG" in content:
        return "bg_completed"
    return "completed"


async def vs_add_file(request: web.Request) -> web.Response:
    body = await request.json()
    await _record(request, body)
    vs = request.match_info["vs"]
    if vs not in VECTOR_STORES:
        return web.json_response({"error": {"message": "no vs"}}, status=404)
    fid = body.get("file_id")
    status = _initial_status(fid)
    entry = {"status": status, "polls": 0, "attributes": body.get("attributes"), "added": time.time()}
    VECTOR_STORES[vs]["files"][fid] = entry
    shown = "in_progress" if status.startswith(("in_progress", "bg_")) else status
    return web.json_response({"id": fid, "object": "vector_store.file", "status": shown})


async def vs_get_file(request: web.Request) -> web.Response:
    await _record(request, None)
    vs = request.match_info["vs"]
    fid = request.match_info["fid"]
    entry = VECTOR_STORES.get(vs, {}).get("files", {}).get(fid)
    if entry is None:
        return web.json_response({"error": {"message": "not found"}}, status=404)
    entry["polls"] += 1
    status = entry["status"]
    if status == "in_progress_then_completed":
        status = "completed" if entry["polls"] >= 2 else "in_progress"
    elif status in ("bg_completed", "bg_failed"):
        if time.time() - entry["added"] < 30:
            status = "in_progress"
        else:
            status = "completed" if status == "bg_completed" else "failed"
    return web.json_response({"id": fid, "status": status})


async def vs_delete(request: web.Request) -> web.Response:
    await _record(request, None)
    vs = request.match_info["vs"]
    if VECTOR_STORES.pop(vs, None) is None:
        return web.json_response({"error": {"message": "not found"}}, status=404)
    return web.json_response({"id": vs, "deleted": True})


async def vs_search(request: web.Request) -> web.Response:
    body = await request.json()
    await _record(request, body)
    return web.json_response({"data": [{"content": [{"type": "text", "text": "knowledge chunk"}]}]})


async def mock_requests(request: web.Request) -> web.Response:
    return web.json_response(REQUESTS)


async def mock_reset(request: web.Request) -> web.Response:
    REQUESTS.clear()
    return web.json_response({"ok": True})


async def mock_config(request: web.Request) -> web.Response:
    CONFIG.update(await request.json())
    return web.json_response(CONFIG)


async def mock_state(request: web.Request) -> web.Response:
    return web.json_response(
        {
            "files": list(FILES.keys()),
            "vector_stores": {k: list(v["files"].keys()) for k, v in VECTOR_STORES.items()},
        }
    )


def build_app() -> web.Application:
    app = web.Application(client_max_size=128 * 1024 * 1024)
    for prefix in ("/v1", "/openai", "/openai/v1"):
        app.router.add_post(f"{prefix}/responses", responses)
        app.router.add_post(f"{prefix}/chat/completions", chat_completions)
        app.router.add_post(f"{prefix}/files", files_create)
        app.router.add_delete(f"{prefix}/files/{{fid}}", files_delete)
        app.router.add_post(f"{prefix}/vector_stores", vs_create)
        app.router.add_post(f"{prefix}/vector_stores/{{vs}}/files", vs_add_file)
        app.router.add_get(f"{prefix}/vector_stores/{{vs}}/files/{{fid}}", vs_get_file)
        app.router.add_delete(f"{prefix}/vector_stores/{{vs}}", vs_delete)
        app.router.add_post(f"{prefix}/vector_stores/{{vs}}/search", vs_search)
    app.router.add_get("/__mock/requests", mock_requests)
    app.router.add_post("/__mock/reset", mock_reset)
    app.router.add_post("/__mock/config", mock_config)
    app.router.add_get("/__mock/state", mock_state)
    return app


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    web.run_app(build_app(), host="127.0.0.1", port=port, print=None)
