"""OpenAI-compatible mock provider for the mini-chat e2e tests.

Serves the Responses API (streaming and non-streaming), Chat Completions,
the Files API and the Vector Stores API. Every request is recorded and can be
read back through ``GET /_admin/requests``. Scripted responses for the next
chat requests are queued with ``POST /_admin/script``.

A script item is a JSON object:

* ``{"events": [...]}``       SSE events to send (``data`` objects; each may
                                carry ``"_event"`` for the ``event:`` line and
                                ``"_delay_ms"``)
* ``{"status": 429, "body": {...}, "headers": {...}}``  an HTTP error
* ``{"hang": true}``           never answer (keeps the connection open)
* ``{"json": {...}}``          a non-streaming JSON body

Run: ``python3 mock_provider.py <port>``
"""

import asyncio
import json
import sys
import uuid

from aiohttp import web

STATE = {
    "requests": [],
    "script": [],
    "index_status": "completed",  # status returned for vector store files
    "index_script": [],  # statuses returned by successive status reads
    "file_upload_status": 200,
    "vs_create_status": 200,
    "delete_status": 200,
}


def default_events(text="Hello from mock", usage=None):
    usage = usage or {"input_tokens": 11, "output_tokens": 7}
    rid = "resp_" + uuid.uuid4().hex
    events = [{"type": "response.created", "response": {"id": rid}}]
    for i, chunk in enumerate(split(text)):
        events.append(
            {"type": "response.output_text.delta", "item_id": "msg_1", "content_index": 0, "delta": chunk}
        )
    events.append({"type": "response.completed", "response": {"id": rid, "usage": usage}})
    return events


def split(text, n=3):
    if not text:
        return []
    size = max(1, len(text) // n)
    return [text[i : i + size] for i in range(0, len(text), size)]


async def record(request):
    body_text = None
    body_json = None
    multipart = None
    ctype = request.headers.get("Content-Type", "")
    if ctype.startswith("multipart/"):
        multipart = {}
        reader = await request.multipart()
        while True:
            part = await reader.next()
            if part is None:
                break
            data = await part.read()
            multipart[part.name] = {
                "filename": part.filename,
                "content_type": part.headers.get("Content-Type"),
                "size": len(data),
                "text": data.decode("utf-8", "replace") if part.filename is None else None,
            }
    else:
        raw = await request.read()
        if raw:
            body_text = raw.decode("utf-8", "replace")
            try:
                body_json = json.loads(body_text)
            except ValueError:
                pass
    entry = {
        "method": request.method,
        "path": request.path,
        "query": dict(request.query),
        "headers": {k.lower(): v for k, v in request.headers.items()},
        "json": body_json,
        "text": body_text,
        "multipart": multipart,
    }
    STATE["requests"].append(entry)
    return entry


async def sse(request, events):
    resp = web.StreamResponse(status=200, headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"})
    await resp.prepare(request)
    try:
        for ev in events:
            ev = dict(ev)
            delay = ev.pop("_delay_ms", 0)
            name = ev.pop("_event", None)
            raw = ev.pop("_raw", None)
            if delay:
                await asyncio.sleep(delay / 1000)
            frame = ""
            if name:
                frame += f"event: {name}\n"
            frame += "data: " + (raw if raw is not None else json.dumps(ev)) + "\n\n"
            await resp.write(frame.encode())
        await resp.write_eof()
    except (ConnectionResetError, asyncio.CancelledError):
        pass
    return resp


async def responses(request):
    entry = await record(request)
    body = entry["json"] or {}
    item = STATE["script"].pop(0) if STATE["script"] else None
    if item and item.get("hang"):
        try:
            resp = web.StreamResponse(status=200, headers={"Content-Type": "text/event-stream"})
            await resp.prepare(request)
            await resp.write(b"data: " + json.dumps({"type": "response.created", "response": {"id": "resp_hang"}}).encode() + b"\n\n")
            await asyncio.sleep(3600)
        except (ConnectionResetError, asyncio.CancelledError):
            pass
        return web.Response(status=200)
    if item and "status" in item:
        return web.json_response(item.get("body", {}), status=item["status"], headers=item.get("headers", {}))
    if item and "json" in item:
        return web.json_response(item["json"])
    if not body.get("stream"):
        text = "<analysis>thinking</analysis><summary>Summary of the conversation.</summary>"
        return web.json_response(
            {
                "id": "resp_" + uuid.uuid4().hex,
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
                "usage": {"input_tokens": 100, "output_tokens": 20},
            }
        )
    events = item["events"] if item and "events" in item else default_events()
    return await sse(request, events)


async def chat_completions(request):
    entry = await record(request)
    item = STATE["script"].pop(0) if STATE["script"] else None
    if item and "status" in item:
        return web.json_response(item.get("body", {}), status=item["status"])
    if item and "events" in item:
        return await sse(request, item["events"])
    chunks = [
        {"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "Hi there"}}]},
        {"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        {"id": "chatcmpl-1", "choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 3}},
        {"_raw": "[DONE]"},
    ]
    return await sse(request, chunks)


async def files_create(request):
    await record(request)
    if STATE["file_upload_status"] != 200:
        return web.json_response({"error": {"message": "upload failed"}}, status=STATE["file_upload_status"])
    return web.json_response({"id": "file-" + uuid.uuid4().hex[:24], "object": "file"})


async def files_delete(request):
    await record(request)
    if STATE["delete_status"] != 200:
        return web.json_response({"error": {"message": "nope"}}, status=STATE["delete_status"])
    return web.json_response({"id": request.match_info["fid"], "deleted": True})


async def vs_create(request):
    await record(request)
    if STATE["vs_create_status"] != 200:
        return web.json_response({"error": {"message": "vs failed"}}, status=STATE["vs_create_status"])
    return web.json_response({"id": "vs_" + uuid.uuid4().hex[:24]})


async def vs_add_file(request):
    entry = await record(request)
    status = STATE["index_status"]
    return web.json_response({"id": (entry["json"] or {}).get("file_id"), "status": status})


async def vs_file_status(request):
    await record(request)
    status = STATE["index_script"].pop(0) if STATE["index_script"] else STATE["index_status"]
    return web.json_response({"id": request.match_info["fid"], "status": status})


async def vs_search(request):
    await record(request)
    return web.json_response(
        {
            "data": [
                {"file_id": "file-kb", "filename": "kb.md", "score": 0.91,
                 "content": [{"type": "text", "text": "The KB says the answer is 42."}]}
            ]
        }
    )


async def vs_delete(request):
    await record(request)
    return web.json_response({"id": request.match_info["vid"], "deleted": True})


async def admin_requests(request):
    return web.json_response(STATE["requests"])


async def admin_reset(request):
    STATE["requests"].clear()
    STATE["script"].clear()
    STATE["index_status"] = "completed"
    STATE["index_script"] = []
    STATE["file_upload_status"] = 200
    STATE["vs_create_status"] = 200
    STATE["delete_status"] = 200
    return web.json_response({"ok": True})


async def admin_script(request):
    items = await request.json()
    STATE["script"].extend(items)
    return web.json_response({"queued": len(STATE["script"])})


async def admin_set(request):
    data = await request.json()
    for k, v in data.items():
        STATE[k] = v
    return web.json_response({"ok": True})


def app():
    a = web.Application(client_max_size=64 * 1024 * 1024)
    for prefix in ("/v1", "/openai", "/openai/v1"):
        a.router.add_post(prefix + "/responses", responses)
        a.router.add_post(prefix + "/chat/completions", chat_completions)
        a.router.add_post(prefix + "/files", files_create)
        a.router.add_delete(prefix + "/files/{fid}", files_delete)
        a.router.add_post(prefix + "/vector_stores", vs_create)
        a.router.add_post(prefix + "/vector_stores/{vid}/files", vs_add_file)
        a.router.add_get(prefix + "/vector_stores/{vid}/files/{fid}", vs_file_status)
        a.router.add_delete(prefix + "/vector_stores/{vid}", vs_delete)
        a.router.add_post(prefix + "/vector_stores/{vid}/search", vs_search)
    a.router.add_get("/_admin/requests", admin_requests)
    a.router.add_post("/_admin/reset", admin_reset)
    a.router.add_post("/_admin/script", admin_script)
    a.router.add_post("/_admin/set", admin_set)
    return a


if __name__ == "__main__":
    web.run_app(app(), host="127.0.0.1", port=int(sys.argv[1]), print=None)
