"""Light self-tests of the mock provider (no gear server needed): later tasks rely on its contract."""

import json
import re

import pytest
import requests

from . import mock_provider as mp
from .helpers import parse_sse


@pytest.fixture
def m(mock):
    mock.reset()
    yield mock
    mock.reset()


def sse(resp):
    assert resp.status_code == 200
    assert resp.headers["Content-Type"].startswith("text/event-stream")
    return parse_sse(resp.content.decode())


def post_responses(m, path="/v1/responses", **body):
    body.setdefault("model", "gpt-premium")
    body.setdefault("stream", True)
    return requests.post(m.base_url + path, json=body, stream=body["stream"], timeout=15)


def test_default_stream_script_shape(m):
    for path in ("/v1/responses", "/openai/v1/responses"):
        events = sse(post_responses(m, path))
        assert [n for n, _ in events] == [
            "response.created", "response.output_text.delta", "response.output_text.delta",
            "response.completed"]
        for name, data in events:
            assert data["type"] == name
        assert [d["delta"] for n, d in events if n.endswith("delta")] == ["Hello", " world"]
        done = events[-1][1]["response"]
        assert done["usage"] == {"input_tokens": 12, "output_tokens": 5}
        assert re.fullmatch(r"resp_[A-Za-z0-9]{24}", done["id"])


def test_raw_frame_format(m):
    raw = post_responses(m).content.decode()
    assert raw.startswith("event: response.created\ndata: {")
    assert raw.endswith("\n\n")


def test_non_stream_json(m):
    r = post_responses(m, stream=False)
    assert r.status_code == 200
    j = r.json()
    assert j["output_text"] == "Hello world" and j["usage"]["input_tokens"] == 12


def test_requests_recorded_and_control_endpoints(m):
    post_responses(m, input="hi", metadata={"k": "v"}).content
    r = requests.get(m.base_url + "/__control/requests", timeout=5).json()["requests"]
    assert len(r) == 1
    rec = r[0]
    assert rec["method"] == "POST" and rec["path"] == "/v1/responses" and rec["route"] == "responses"
    assert rec["json"]["input"] == "hi" and rec["json"]["stream"] is True
    assert "content-type" in rec["headers"] and rec["query"] == {}
    requests.post(m.base_url + "/__control/reset", timeout=5)
    assert requests.get(m.base_url + "/__control/requests", timeout=5).json() == {"requests": []}


def test_enqueue_custom_events_fifo_and_repeat(m):
    requests.post(m.base_url + "/__control/enqueue", timeout=5, json={
        "route": "responses",
        "script": {"events": mp.text_events("A", usage={"input_tokens": 1, "output_tokens": 2}),
                   "delay_ms": 30}})
    first = sse(post_responses(m))
    assert [d["delta"] for n, d in first if n.endswith("delta")] == ["A"]
    assert first[-1][1]["response"]["usage"] == {"input_tokens": 1, "output_tokens": 2}
    second = sse(post_responses(m))  # queue drained -> default again
    assert len([n for n, _ in second if n.endswith("delta")]) == 2
    m.enqueue("responses", {"events": mp.text_events("R")}, repeat=0)  # forever
    for _ in range(3):
        assert [d["delta"] for n, d in sse(post_responses(m)) if n.endswith("delta")] == ["R"]


def test_event_delay_is_applied(m):
    import time
    m.enqueue("responses", {"events": mp.text_events("x"), "delay_ms": 150})
    t = time.monotonic()
    sse(post_responses(m))
    assert time.monotonic() - t >= 0.28  # 2 gaps (events 2..3) of 150 ms


def test_tool_and_annotation_events(m):
    m.enqueue("responses", {"events": [
        mp.ev_created(), mp.ev_file_search(), mp.ev_file_search(done=True),
        mp.ev_web_search(), mp.ev_web_search(done=True),
        mp.ev_code_interpreter_start(), mp.ev_code_interpreter_done("42"),
        mp.ev_delta("t"), mp.ev_url_citation("https://example.com"), mp.ev_file_citation("file-abc"),
        mp.ev_completed()]})
    names = [n for n, _ in sse(post_responses(m))]
    assert names == [
        "response.created", "response.file_search_call.searching", "response.file_search_call.completed",
        "response.web_search_call.searching", "response.web_search_call.completed",
        "response.code_interpreter_call.in_progress", "response.output_item.done",
        "response.output_text.delta", "response.output_text.annotation.added",
        "response.output_text.annotation.added", "response.completed"]


def test_error_status_script(m):
    m.enqueue("responses", {"status": 429, "body": {"error": {"message": "slow down", "code": "rate_limit"}},
                            "headers": {"Retry-After": "7"}})
    r = post_responses(m)
    assert r.status_code == 429 and r.headers["Retry-After"] == "7"
    assert r.json()["error"]["message"] == "slow down"
    m.enqueue("responses", {"status": 500})
    r = post_responses(m)
    assert r.status_code == 500 and r.json()["error"]


def test_response_failed_script(m):
    m.enqueue("responses", {"failed": {"code": "server_error", "message": "boom"}})
    events = sse(post_responses(m))
    assert [n for n, _ in events] == ["response.created", "response.failed"]
    assert events[-1][1]["response"]["error"] == {"code": "server_error", "message": "boom"}
    assert events[-1][1]["response"]["status"] == "failed"


def test_hang_has_no_terminal_event_and_reset_releases(m):
    m.enqueue("responses", {"events": [mp.ev_created(), mp.ev_delta("partial")], "hang": True})
    r = requests.post(m.base_url + "/v1/responses", json={"model": "x", "stream": True}, stream=True,
                      timeout=(5, 0.7))
    it = r.iter_lines()
    seen = []
    with pytest.raises(requests.exceptions.RequestException):
        for line in it:
            if line.startswith(b"event:"):
                seen.append(line.decode())
    assert seen == ["event: response.created", "event: response.output_text.delta"]
    r.close()
    m.reset()


def test_client_disconnect_is_recorded(m):
    from .helpers import wait_until
    m.enqueue("responses", {"events": [mp.ev_created()], "hang": True})
    r = requests.post(m.base_url + "/v1/responses", json={"model": "x", "stream": True}, stream=True, timeout=5)
    next(r.iter_lines())
    r.close()
    wait_until(lambda: m.requests(route="responses")[0]["client_disconnected"], 5, message="disconnect seen")


def test_summary_route_by_model(m):
    m.enqueue("summary", {"events": [mp.ev_created(), mp.ev_delta("<summary>S</summary>"), mp.ev_completed()]})
    r = post_responses(m, model="gpt-4.1-mini", stream=True)
    assert [d["delta"] for n, d in sse(r) if n.endswith("delta")] == ["<summary>S</summary>"]
    r = post_responses(m, model="gpt-4.1-mini", stream=False)
    assert "<summary>" in r.json()["output_text"]
    assert [x["route"] for x in m.requests()] == ["summary", "summary"]


@pytest.mark.parametrize("prefix", ["/v1", "/openai"])
def test_files_and_vector_stores_lifecycle(m, prefix):
    files = m.base_url + prefix + "/files"
    r = requests.post(files, files={"file": ("a.txt", b"hello", "text/plain")}, data={"purpose": "assistants"},
                      timeout=5)
    assert r.status_code == 200
    fid = r.json()["id"]
    assert re.fullmatch(r"file-[A-Za-z0-9]{24}", fid)
    rec = m.requests(method="POST", path="/files")[0]
    assert sorted(rec["multipart_fields"]) == ["file", "purpose"] and [p["size"] for p in rec["multipart"] if p["filename"]] == [5]

    r = requests.post(m.base_url + prefix + "/vector_stores", json={"name": "chat"}, timeout=5)
    vs = r.json()["id"]
    assert re.fullmatch(r"vs_[A-Za-z0-9]{24}", vs)
    q = {"params": {"api-version": "2025-03-01-preview"}} if prefix == "/openai" else {}
    r = requests.post(f"{m.base_url}{prefix}/vector_stores/{vs}/files", json={"file_id": fid}, timeout=5, **q)
    assert r.json()["status"] == "completed" and r.json()["id"] == fid
    if q:
        assert m.requests(path="/files")[-1]["query"] == {"api-version": ["2025-03-01-preview"]}
    r = requests.get(f"{m.base_url}{prefix}/vector_stores/{vs}/files/{fid}", timeout=5)
    assert r.json()["status"] == "completed"
    assert requests.delete(f"{files}/{fid}", timeout=5).json()["deleted"] is True
    assert requests.delete(f"{m.base_url}{prefix}/vector_stores/{vs}", timeout=5).json()["deleted"] is True


def test_vector_store_file_status_script(m):
    m.enqueue("vector_store_file_status", {"file_status": "in_progress"})
    m.enqueue("vector_store_file_status", {"file_status": "failed"})
    base = m.base_url + "/v1/vector_stores/vs_x/files"
    assert requests.post(base, json={"file_id": "file-1"}, timeout=5).json()["status"] == "in_progress"
    r = requests.get(base + "/file-1", timeout=5).json()
    assert r["status"] == "failed" and r["last_error"]["message"]
    assert requests.get(base + "/file-1", timeout=5).json()["status"] == "failed"  # sticks once drained


def test_error_scripts_for_files_and_deletes(m):
    m.enqueue("files", {"status": 500, "body": {"error": {"message": "upload broke"}}})
    r = requests.post(m.base_url + "/v1/files", files={"file": ("a.txt", b"x", "text/plain")}, timeout=5)
    assert r.status_code == 500 and r.json()["error"]["message"] == "upload broke"
    m.enqueue("delete_file", {"status": 404})
    assert requests.delete(m.base_url + "/v1/files/file-zzz", timeout=5).status_code == 404
    m.enqueue("delete_vector_store", {"status": 503})
    assert requests.delete(m.base_url + "/v1/vector_stores/vs_zzz", timeout=5).status_code == 503
    m.enqueue("files", {"id": "file-custom"})
    r = requests.post(m.base_url + "/v1/files", files={"file": ("a.txt", b"x", "text/plain")}, timeout=5)
    assert r.json()["id"] == "file-custom"


def test_unknown_route_404_and_bad_enqueue(m):
    assert requests.get(m.base_url + "/v1/nope", timeout=5).status_code == 404
    r = requests.post(m.base_url + "/__control/enqueue", json={"route": "bogus"}, timeout=5)
    assert r.status_code == 400


def test_disconnect_script_drops_stream_without_terminal(m):
    m.enqueue("responses", {"events": [mp.ev_created()], "disconnect": True})
    r = requests.post(m.base_url + "/v1/responses", json={"model": "x", "stream": True}, stream=True, timeout=5)
    with pytest.raises(requests.exceptions.RequestException):
        r.content
