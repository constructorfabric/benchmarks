"""Builders of scripted OpenAI Responses API SSE replies for the mock provider."""

from __future__ import annotations

from typing import Any


def ev(name: str, **data: Any) -> dict[str, Any]:
    return {"event": name, "data": {"type": name, **data}}


def created(resp_id: str = "resp_abc123def456") -> dict[str, Any]:
    return ev("response.created", response={"id": resp_id, "status": "in_progress"})


def delta(text: str) -> dict[str, Any]:
    return ev("response.output_text.delta", delta=text)


def completed(text: str = "", usage: dict[str, Any] | None = None, annotations: list[dict[str, Any]] | None = None,
              resp_id: str = "resp_abc123def456", incomplete: bool = False) -> dict[str, Any]:
    response: dict[str, Any] = {
        "id": resp_id,
        "status": "incomplete" if incomplete else "completed",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": annotations or []}],
            }
        ],
    }
    if usage is not False:
        response["usage"] = usage or {"input_tokens": 100, "output_tokens": 20}
    if incomplete:
        response["incomplete_details"] = {"reason": "max_output_tokens"}
    return ev("response.incomplete" if incomplete else "response.completed", response=response)


def text_reply(text: str, usage: dict[str, Any] | None = None, chunk: int = 5, **kw: Any) -> dict[str, Any]:
    events = [created()]
    for i in range(0, len(text), chunk):
        events.append(delta(text[i : i + chunk]))
    events.append(completed(text, usage))
    return {"kind": "sse", "events": events, **kw}


def sse(*events: dict[str, Any], **kw: Any) -> dict[str, Any]:
    return {"kind": "sse", "events": list(events), **kw}


def sleep(ms: int) -> dict[str, Any]:
    return {"sleep_ms": ms}


def failed(message: str, code: str = "server_error", usage: dict[str, Any] | None = None) -> dict[str, Any]:
    resp: dict[str, Any] = {"id": "resp_failed000000", "status": "failed", "error": {"code": code, "message": message}}
    if usage:
        resp["usage"] = usage
    return ev("response.failed", response=resp)
