"""OpenAI-compatible mock provider (Responses, Files, Vector Stores) for the
mini-chat black-box suite. Records every request; behavior is scriptable via
``POST /_mock/script`` (a queue of response specs consumed in order)."""

import asyncio
import json
import threading
import uuid

from aiohttp import web


class MockProvider:
    def __init__(self):
        self.requests = []
        self.script = []  # queue of dicts for streaming /responses (chat turns)
        self.summary_script = []  # queue of dicts for non-streaming /responses (thread summary)
        self.files = []  # provider file ids created through POST /files
        self.vector_stores = []  # vector store ids created through POST /vector_stores
        self.vs_status = "completed"
        self.file_upload_status = 200
        self.file_delete_status = 200
        self.vs_delete_status = 200
        self.app = web.Application(client_max_size=64 * 1024 * 1024)
        self.app.router.add_route("*", "/{tail:.*}", self.handle)
        self.runner = None
        self.port = None
        self.loop = None
        self.thread = None

    # ---------------------------------------------------------------- control
    def reset(self):
        self.requests.clear()
        self.script.clear()
        self.summary_script.clear()
        self.files.clear()
        self.vector_stores.clear()
        self.vs_status = "completed"
        self.file_upload_status = 200
        self.file_delete_status = 200
        self.vs_delete_status = 200

    def push(self, spec):
        self.script.append(spec)

    def push_summary(self, spec):
        self.summary_script.append(spec)

    def responses_requests(self):
        return [r for r in self.requests if r["path"].endswith("/responses")]

    def chat_requests(self):
        """Streaming chat-turn calls (``metadata.request_type == "chat"``)."""
        return [r for r in self.responses_requests() if (r["json"] or {}).get("stream")]

    def summary_requests(self):
        return [r for r in self.responses_requests() if not (r["json"] or {}).get("stream")]

    def calls(self, method, fragment):
        return [r for r in self.requests if r["method"] == method and fragment in r["path"]]

    # ---------------------------------------------------------------- server
    async def handle(self, request: web.Request):
        path = request.path
        if path.startswith("/_mock"):
            return web.json_response({"ok": True})
        body = await request.read()
        rec = {
            "method": request.method,
            "path": path,
            "query": dict(request.query),
            "headers": {k.lower(): v for k, v in request.headers.items()},
            "body": body,
        }
        try:
            rec["json"] = json.loads(body) if body and request.content_type == "application/json" else None
        except Exception:  # noqa: BLE001
            rec["json"] = None
        self.requests.append(rec)
        if path.endswith("/responses"):
            return await self.responses(request, rec)
        if request.method == "POST" and path.endswith("/files") and "vector_stores" not in path:
            if self.file_upload_status != 200:
                return web.json_response({"error": {"message": "upload rejected"}}, status=self.file_upload_status)
            fid = "file-" + uuid.uuid4().hex[:24]
            self.files.append(fid)
            return web.json_response({"id": fid, "object": "file"})
        if request.method == "DELETE" and "/files/" in path and "vector_stores" not in path:
            return web.json_response({"deleted": True}, status=self.file_delete_status)
        if request.method == "POST" and path.endswith("/vector_stores"):
            vid = "vs_" + uuid.uuid4().hex[:24]
            self.vector_stores.append(vid)
            return web.json_response({"id": vid})
        if request.method == "POST" and "/vector_stores/" in path and path.endswith("/files"):
            return web.json_response({"id": "x", "status": self.vs_status})
        if request.method == "GET" and "/vector_stores/" in path:
            return web.json_response({"id": "x", "status": self.vs_status})
        if request.method == "DELETE" and "/vector_stores/" in path:
            return web.json_response({"deleted": True}, status=self.vs_delete_status)
        return web.json_response({"error": {"message": "not found"}}, status=404)

    async def responses(self, request, rec):
        data = rec["json"] or {}
        queue = self.script if data.get("stream", False) else self.summary_script
        spec = queue.pop(0) if queue else {}
        if spec.get("status", 200) != 200:
            headers = {}
            if "retry_after" in spec:
                headers["Retry-After"] = str(spec["retry_after"])
            return web.json_response(
                {"error": {"message": spec.get("message", "boom"), "code": "x"}},
                status=spec["status"],
                headers=headers,
            )
        if not data.get("stream", False):
            text = spec.get("text", "<analysis>thinking</analysis><summary>Summary of the conversation</summary>")
            return web.json_response({
                "id": "resp_" + uuid.uuid4().hex,
                "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
                "usage": spec.get("usage", {"input_tokens": 100, "output_tokens": 30}),
            })
        resp = web.StreamResponse(headers={"Content-Type": "text/event-stream"})
        await resp.prepare(request)
        try:
            await self._stream(resp, spec)
        except ConnectionError:
            # the gear dropped the upstream connection (client disconnect / cancellation)
            rec["aborted"] = True
        return resp

    async def _stream(self, resp, spec):
        async def ev(name, payload):
            await resp.write(f"event: {name}\ndata: {json.dumps(payload)}\n\n".encode())

        for e in spec.get("pre_events", []):
            await ev(e[0], e[1])
        if spec.get("delay_before"):
            await asyncio.sleep(spec["delay_before"])
        text = spec.get("text", "Hello from mock")
        chunks = spec.get("chunks") or [text[i:i + 5] for i in range(0, len(text), 5)]
        for c in chunks:
            await ev("response.output_text.delta", {"type": "response.output_text.delta", "item_id": "msg_1", "content_index": 0, "delta": c})
            if spec.get("delay"):
                await asyncio.sleep(spec["delay"])
        for e in spec.get("post_events", []):
            await ev(e[0], e[1])
        if spec.get("hang"):
            await asyncio.sleep(spec["hang"])
        end = spec.get("end", "completed")
        usage = spec.get("usage", {"input_tokens": 20, "output_tokens": 10})
        if end == "completed":
            await ev("response.completed", {"type": "response.completed", "response": {"id": "resp_" + uuid.uuid4().hex, "usage": usage, "output": spec.get("output", [])}})
        elif end == "incomplete":
            await ev("response.incomplete", {"type": "response.incomplete", "response": {"usage": usage, "incomplete_details": {"reason": "max_output_tokens"}}})
        elif end == "failed":
            await ev("response.failed", {"type": "response.failed", "response": {"error": {"code": "server_error", "message": spec.get("message", "upstream failed")}}})
        elif end == "error":
            await ev("error", {"type": "error", "code": "x", "message": spec.get("message", "bad")})
        # end == "none": close without terminal event
        await resp.write_eof()

    def start(self):
        ready = threading.Event()

        def run():
            self.loop = asyncio.new_event_loop()
            asyncio.set_event_loop(self.loop)
            self.runner = web.AppRunner(self.app, shutdown_timeout=1.0)
            self.loop.run_until_complete(self.runner.setup())
            site = web.TCPSite(self.runner, "127.0.0.1", 0)
            self.loop.run_until_complete(site.start())
            self.port = site._server.sockets[0].getsockname()[1]
            ready.set()
            self.loop.run_forever()

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()
        ready.wait(10)
        return self.port

    def stop(self):
        if not self.loop:
            return
        fut = asyncio.run_coroutine_threadsafe(self.runner.cleanup(), self.loop)
        try:
            fut.result(15)
        except Exception:  # noqa: BLE001
            pass
        self.loop.call_soon_threadsafe(self.loop.stop)
        self.thread.join(5)
