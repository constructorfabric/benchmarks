"""OpenAI-compatible mock LLM/RAG provider for mini-chat black-box tests.

Serves (both under `/v1/...` for OpenAI and `/openai/...` for Azure):
  POST   responses                      streaming (SSE) and non-streaming
  POST   files                          multipart upload
  DELETE files/{id}
  POST   vector_stores
  POST   vector_stores/{id}/files
  GET    vector_stores/{id}/files/{fid}
  DELETE vector_stores/{id}

Control API (not proxied by OAGW; tests call it directly):
  POST /__mock/reset                    clear recorded requests and scripts
  POST /__mock/script                   {"path_suffix": "responses", "items": [...]} queue scripted replies
  POST /__mock/config                   {"vector_store_file_status": "completed", ...}
  GET  /__mock/requests                 recorded requests

Scripted reply item kinds:
  {"kind": "sse", "events": [{"event": "...", "data": {...}} | {"raw": "..."}], "delay_ms": 0,
   "hang_after": false}
  {"kind": "json", "status": 200, "body": {...}, "headers": {...}}
  {"kind": "error", "status": 500, "body": {...}, "headers": {...}}
  {"kind": "hang"}                      never answers (until the client disconnects)

Unscripted `responses` requests get a default answer: streaming
`Echo: <last user text>` split in a few deltas, then `response.completed`
with usage; non-streaming returns a thread-summary style answer.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import secrets
import string
import sys
from typing import Any

from aiohttp import web

ALNUM = string.ascii_letters + string.digits


def rid(prefix: str, n: int = 24) -> str:
    return prefix + "".join(secrets.choice(ALNUM) for _ in range(n))


class MockState:
    def __init__(self) -> None:
        self.reset()

    def reset(self) -> None:
        self.requests: list[dict[str, Any]] = []
        self.scripts: dict[str, list[dict[str, Any]]] = {}
        self.config: dict[str, Any] = {
            "vector_store_file_status": "completed",
            "vector_store_file_poll_statuses": [],
            "file_delete_status": 200,
            "vector_store_delete_status": 200,
            "file_upload_status": 200,
            "vector_store_create_status": 200,
            "vector_store_add_status": 200,
        }
        self.files: dict[str, dict[str, Any]] = {}
        self.vector_stores: dict[str, dict[str, Any]] = {}
        self.vs_poll_counts: dict[str, int] = {}

    def pop_script(self, path: str) -> dict[str, Any] | None:
        for suffix, items in self.scripts.items():
            if not items:
                continue
            if suffix.startswith("~"):
                if suffix[1:] in path:
                    return items.pop(0)
            elif path.endswith(suffix):
                return items.pop(0)
        return None


STATE = MockState()


def last_user_text(body: dict[str, Any]) -> str:
    items = body.get("input")
    if isinstance(items, str):
        return items
    text = ""
    if isinstance(items, list):
        for item in items:
            if not isinstance(item, dict) or item.get("role") != "user":
                continue
            content = item.get("content")
            if isinstance(content, str):
                text = content
            elif isinstance(content, list):
                parts = [c.get("text", "") for c in content if isinstance(c, dict) and c.get("type") in ("input_text", "text")]
                text = "".join(parts)
    return text


def sse_frame(event: str | None, data: Any) -> bytes:
    payload = data if isinstance(data, str) else json.dumps(data)
    out = ""
    if event:
        out += f"event: {event}\n"
    for line in payload.split("\n"):
        out += f"data: {line}\n"
    return (out + "\n").encode()


def default_stream_events(body: dict[str, Any]) -> list[dict[str, Any]]:
    text = f"Echo: {last_user_text(body)}"
    resp_id = rid("resp_")
    chunks = [text[i : i + 8] for i in range(0, len(text), 8)] or [""]
    events: list[dict[str, Any]] = [
        {"event": "response.created", "data": {"type": "response.created", "response": {"id": resp_id, "status": "in_progress"}}}
    ]
    for c in chunks:
        events.append({"event": "response.output_text.delta", "data": {"type": "response.output_text.delta", "delta": c}})
    events.append(
        {
            "event": "response.completed",
            "data": {
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "status": "completed",
                    "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}],
                    "usage": {"input_tokens": 100, "output_tokens": 20, "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}},
                },
            },
        }
    )
    return events


async def record(request: web.Request) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "method": request.method,
        "path": request.path,
        "query": dict(request.query),
        "headers": {k: v for k, v in request.headers.items()},
    }
    ctype = request.headers.get("Content-Type", "")
    if ctype.startswith("multipart/form-data"):
        reader = await request.multipart()
        fields: dict[str, Any] = {}
        while True:
            part = await reader.next()
            if part is None:
                break
            data = await part.read(decode=False)
            if part.filename is not None:
                fields[part.name] = {"filename": part.filename, "content_type": part.headers.get("Content-Type"), "size": len(data)}
            else:
                fields[part.name] = data.decode(errors="replace")
        entry["multipart"] = fields
    else:
        raw = await request.read()
        if raw:
            try:
                entry["json"] = json.loads(raw)
            except Exception:  # noqa: BLE001
                entry["body"] = raw.decode(errors="replace")
    STATE.requests.append(entry)
    return entry


async def run_script(request: web.Request, item: dict[str, Any]) -> web.StreamResponse:
    kind = item.get("kind", "sse")
    if kind in ("json", "error"):
        status = int(item.get("status", 200 if kind == "json" else 500))
        return web.json_response(item.get("body", {}), status=status, headers=item.get("headers"))
    if kind == "hang":
        try:
            await asyncio.sleep(3600)
        except asyncio.CancelledError:
            raise
        return web.Response(status=500)
    resp = web.StreamResponse(status=200, headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"})
    await resp.prepare(request)
    delay = float(item.get("delay_ms", 0)) / 1000.0
    for ev in item.get("events", []):
        if "sleep_ms" in ev:
            await asyncio.sleep(float(ev["sleep_ms"]) / 1000.0)
            continue
        if "raw" in ev:
            await resp.write(ev["raw"].encode())
        else:
            await resp.write(sse_frame(ev.get("event"), ev.get("data", {})))
        if delay:
            await asyncio.sleep(delay)
    if item.get("hang_after"):
        await asyncio.sleep(3600)
    await resp.write_eof()
    return resp


async def handle_responses(request: web.Request) -> web.StreamResponse:
    entry = await record(request)
    body = entry.get("json") or {}
    item = STATE.pop_script(request.path)
    if item is not None:
        return await run_script(request, item)
    if body.get("stream"):
        return await run_script(request, {"kind": "sse", "events": default_stream_events(body)})
    summary = "<analysis>reviewed</analysis>\n<summary>Mock summary of the conversation.</summary>"
    return web.json_response(
        {
            "id": rid("resp_"),
            "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": summary}]}],
            "usage": {"input_tokens": 500, "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 0}},
        }
    )


async def handle_files_post(request: web.Request) -> web.Response:
    entry = await record(request)
    item = STATE.pop_script(request.path)
    if item is not None:
        return await run_script(request, item)  # type: ignore[return-value]
    status = int(STATE.config["file_upload_status"])
    if status != 200:
        return web.json_response({"error": {"message": "upload failed", "type": "server_error"}}, status=status)
    fid = rid("file-")
    meta = (entry.get("multipart") or {}).get("file") or {}
    STATE.files[fid] = {"id": fid, "filename": meta.get("filename"), "bytes": meta.get("size", 0)}
    return web.json_response({"id": fid, "object": "file", "bytes": meta.get("size", 0), "filename": meta.get("filename"), "purpose": (entry.get("multipart") or {}).get("purpose")})


async def handle_files_delete(request: web.Request) -> web.Response:
    await record(request)
    status = int(STATE.config["file_delete_status"])
    fid = request.match_info["fid"]
    if status != 200:
        return web.json_response({"error": {"message": "delete failed"}}, status=status)
    if fid not in STATE.files:
        return web.json_response({"error": {"message": "No such File object"}}, status=404)
    STATE.files.pop(fid, None)
    return web.json_response({"id": fid, "object": "file", "deleted": True})


async def handle_vs_create(request: web.Request) -> web.Response:
    await record(request)
    item = STATE.pop_script(request.path)
    if item is not None:
        return await run_script(request, item)  # type: ignore[return-value]
    status = int(STATE.config["vector_store_create_status"])
    if status != 200:
        return web.json_response({"error": {"message": "vector store create failed"}}, status=status)
    vid = rid("vs_")
    STATE.vector_stores[vid] = {"id": vid, "files": {}}
    return web.json_response({"id": vid, "object": "vector_store", "status": "completed"})


async def handle_vs_add_file(request: web.Request) -> web.Response:
    entry = await record(request)
    item = STATE.pop_script(request.path)
    if item is not None:
        return await run_script(request, item)  # type: ignore[return-value]
    status = int(STATE.config["vector_store_add_status"])
    if status != 200:
        return web.json_response({"error": {"message": "add failed"}}, status=status)
    vid = request.match_info["vid"]
    fid = (entry.get("json") or {}).get("file_id", "")
    st = STATE.config["vector_store_file_status"]
    STATE.vector_stores.setdefault(vid, {"id": vid, "files": {}})["files"][fid] = st
    return web.json_response({"id": fid, "object": "vector_store.file", "status": st, "vector_store_id": vid})


async def handle_vs_get_file(request: web.Request) -> web.Response:
    await record(request)
    vid = request.match_info["vid"]
    fid = request.match_info["fid"]
    seq = STATE.config.get("vector_store_file_poll_statuses") or []
    key = f"{vid}/{fid}"
    n = STATE.vs_poll_counts.get(key, 0)
    STATE.vs_poll_counts[key] = n + 1
    if seq:
        st = seq[min(n, len(seq) - 1)]
    else:
        st = STATE.vector_stores.get(vid, {}).get("files", {}).get(fid, STATE.config["vector_store_file_status"])
    if st == "__500__":
        return web.json_response({"error": {"message": "temporary"}}, status=500)
    if st == "__404__":
        return web.json_response({"error": {"message": "not found"}}, status=404)
    return web.json_response({"id": fid, "object": "vector_store.file", "status": st, "vector_store_id": vid})


async def handle_vs_delete(request: web.Request) -> web.Response:
    await record(request)
    status = int(STATE.config["vector_store_delete_status"])
    vid = request.match_info["vid"]
    if status != 200:
        return web.json_response({"error": {"message": "delete failed"}}, status=status)
    if vid not in STATE.vector_stores:
        return web.json_response({"error": {"message": "not found"}}, status=404)
    STATE.vector_stores.pop(vid, None)
    return web.json_response({"id": vid, "object": "vector_store.deleted", "deleted": True})


async def ctl_reset(request: web.Request) -> web.Response:
    STATE.reset()
    return web.json_response({"ok": True})


async def ctl_script(request: web.Request) -> web.Response:
    body = await request.json()
    STATE.scripts.setdefault(body.get("path_suffix", "responses"), []).extend(body.get("items", []))
    return web.json_response({"ok": True})


async def ctl_config(request: web.Request) -> web.Response:
    body = await request.json()
    STATE.config.update(body)
    return web.json_response(STATE.config)


async def ctl_requests(request: web.Request) -> web.Response:
    return web.json_response(STATE.requests)


async def ctl_state(request: web.Request) -> web.Response:
    return web.json_response({"files": STATE.files, "vector_stores": STATE.vector_stores})


def build_app() -> web.Application:
    app = web.Application(client_max_size=64 * 1024 * 1024)
    for prefix in ("/v1", "/openai", "/openai/v1"):
        app.router.add_post(prefix + "/responses", handle_responses)
        app.router.add_post(prefix + "/files", handle_files_post)
        app.router.add_delete(prefix + "/files/{fid}", handle_files_delete)
        app.router.add_post(prefix + "/vector_stores", handle_vs_create)
        app.router.add_post(prefix + "/vector_stores/{vid}/files", handle_vs_add_file)
        app.router.add_get(prefix + "/vector_stores/{vid}/files/{fid}", handle_vs_get_file)
        app.router.add_delete(prefix + "/vector_stores/{vid}", handle_vs_delete)
    app.router.add_post("/__mock/reset", ctl_reset)
    app.router.add_post("/__mock/script", ctl_script)
    app.router.add_post("/__mock/config", ctl_config)
    app.router.add_get("/__mock/requests", ctl_requests)
    app.router.add_get("/__mock/state", ctl_state)
    return app


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=18123)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()
    web.run_app(build_app(), host=args.host, port=args.port, print=lambda *_: sys.stdout.flush())


if __name__ == "__main__":
    main()
