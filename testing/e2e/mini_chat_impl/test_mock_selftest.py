"""Self-tests of the mock provider (no gear server needed).

Run alone with:  python -m pytest testing/e2e/mini_chat_impl/test_mock_selftest.py -q
"""

import json
import threading
import time

import httpx
import pytest

from mchelpers import parse_sse_text
from mock_llm import FAKE_FILE_ID_IN_ERROR, SUMMARY_MARKER, MockLLM, last_user_text, parse_directives

pytestmark = pytest.mark.noserver


@pytest.fixture(scope="module")
def mk():
    m = MockLLM().start()
    yield m
    m.stop()


@pytest.fixture(autouse=True)
def _reset(mk):
    mk.reset()
    yield


def _stream(mk, text, **extra):
    body = {"model": "mock-x", "stream": True, "input": [{"role": "user", "content": [{"type": "input_text", "text": text}]}]}
    body.update(extra)
    r = httpx.post(f"{mk.base_url}/v1/responses", json=body, headers={"authorization": "Bearer sk-x"}, timeout=30)
    return r, parse_sse_text(r.text) if r.headers.get("content-type", "").startswith("text/event-stream") else []


def test_directive_parsing():
    assert parse_directives("hi [[usage:3:4]] [[websearch:2]] [[hang]]") == {"usage": "3:4", "websearch": "2", "hang": ""}
    body = {
        "input": [
            {"role": "user", "content": "first [[fail]]"},
            {"role": "assistant", "content": [{"type": "output_text", "text": "a"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second"}, {"type": "input_image", "file_id": "f"}]},
        ]
    }
    assert last_user_text(body) == "second"


def test_default_stream_and_recording(mk):
    r, ev = _stream(mk, "plain")
    assert r.status_code == 200
    names = [e for e, _ in ev]
    assert names[0] == "response.created"
    deltas = [d["delta"] for e, d in ev if e == "response.output_text.delta"]
    assert deltas == ["Hello", " world"]
    e, d = ev[-1]
    assert e == "response.completed" and d["type"] == "response.completed"
    assert d["response"]["usage"]["input_tokens"] == 10 and d["response"]["usage"]["output_tokens"] == 5
    assert d["response"]["id"].startswith("resp_") and len(d["response"]["id"]) == 5 + 24
    reqs = mk.chat_requests()
    assert len(reqs) == 1
    assert reqs[0]["headers"]["authorization"] == "Bearer sk-x"
    assert reqs[0]["json"]["model"] == "mock-x"
    assert reqs[0]["response_id"] == d["response"]["id"]


def test_usage_long_incomplete_empty(mk):
    _, ev = _stream(mk, "x [[usage:100:50]] [[long:7]]")
    assert len([1 for e, _ in ev if e == "response.output_text.delta"]) == 7
    assert ev[-1][1]["response"]["usage"] == {**ev[-1][1]["response"]["usage"], "input_tokens": 100, "output_tokens": 50}
    _, ev = _stream(mk, "x [[incomplete]]")
    assert ev[-1][0] == "response.incomplete"
    _, ev = _stream(mk, "x [[empty]]")
    assert not [1 for e, _ in ev if e == "response.output_text.delta"]
    assert ev[-1][0] == "response.completed"


def test_failures(mk):
    _, ev = _stream(mk, "x [[fail]]")
    assert ev[-1][0] == "response.failed"
    assert FAKE_FILE_ID_IN_ERROR in ev[-1][1]["response"]["error"]["message"]
    _, ev = _stream(mk, "x [[error_event]]")
    assert ev[-1][0] == "error"
    r, _ = _stream(mk, "x [[http500]]")
    assert r.status_code == 500 and r.json()["error"]["message"]
    r, _ = _stream(mk, "x [[http429]]")
    assert r.status_code == 429 and r.headers["retry-after"] == "7"


def test_tools_and_citations(mk):
    _, ev = _stream(mk, "x [[websearch:3]]")
    names = [e for e, _ in ev]
    assert names.count("response.web_search_call.searching") == 3
    assert names.count("response.web_search_call.completed") == 3
    ann = [d["annotation"] for e, d in ev if e == "response.output_text.annotation.added"]
    assert ann and ann[0]["type"] == "url_citation" and ann[0]["url"].startswith("https://")
    out = ev[-1][1]["response"]["output"]
    assert any(o["type"] == "message" and o["content"][0]["annotations"] for o in out)

    _, ev = _stream(mk, "x [[codeint:2]]")
    done_items = [d["item"] for e, d in ev if e == "response.output_item.done"]
    assert [i["outputs"][0]["logs"] for i in done_items] == ["ci-output-0", "ci-output-1"]
    assert [e for e, _ in ev].count("response.code_interpreter_call.in_progress") == 2


def test_files_vector_stores_and_filecite(mk):
    files = {"file": ("doc.pdf", b"%PDF-1.4 test", "application/pdf")}
    r = httpx.post(f"{mk.base_url}/v1/files", data={"purpose": "assistants"}, files=files)
    assert r.status_code == 200
    f = r.json()
    assert f["id"].startswith("file-") and len(f["id"]) == 5 + 24
    assert f["bytes"] == len(b"%PDF-1.4 test") and f["filename"] == "doc.pdf" and f["purpose"] == "assistants"
    rec = mk.requests(path_contains="/files", method="POST")[0]
    assert rec["form"]["purpose"] == "assistants"
    assert rec["form"]["_files"]["file"]["content_type"] == "application/pdf"

    vs = httpx.post(f"{mk.base_url}/v1/vector_stores", json={"name": "chat"}).json()["id"]
    assert vs.startswith("vs_")
    added = httpx.post(f"{mk.base_url}/v1/vector_stores/{vs}/files", json={"file_id": f["id"], "attributes": {"attachment_id": "a"}}).json()
    assert added["status"] == "completed"
    st = httpx.get(f"{mk.base_url}/v1/vector_stores/{vs}/files/{f['id']}").json()
    assert st["status"] == "completed"

    _, ev = _stream(mk, "x [[filecite]]", tools=[{"type": "file_search", "vector_store_ids": [vs]}])
    ann = [d["annotation"] for e, d in ev if e == "response.output_text.annotation.added"]
    assert ann[0]["type"] == "file_citation" and ann[0]["file_id"] == f["id"]
    assert "response.file_search_call.searching" in [e for e, _ in ev]

    assert httpx.delete(f"{mk.base_url}/v1/files/{f['id']}").json()["deleted"] is True
    assert httpx.delete(f"{mk.base_url}/v1/files/{f['id']}").status_code == 404
    assert httpx.delete(f"{mk.base_url}/v1/vector_stores/{vs}").status_code == 200
    assert httpx.delete(f"{mk.base_url}/v1/vector_stores/{vs}").status_code == 404


def test_file_failure_controls(mk):
    mk.configure(files_fail=True)
    r = httpx.post(f"{mk.base_url}/v1/files", data={"purpose": "assistants"}, files={"file": ("a.txt", b"a", "text/plain")})
    assert r.status_code == 500
    mk.configure(files_fail=None, index_status="in_progress")
    fid = httpx.post(f"{mk.base_url}/v1/files", data={"purpose": "assistants"}, files={"file": ("a.txt", b"a", "text/plain")}).json()["id"]
    vs = httpx.post(f"{mk.base_url}/v1/vector_stores", json={}).json()["id"]
    assert httpx.post(f"{mk.base_url}/v1/vector_stores/{vs}/files", json={"file_id": fid}).json()["status"] == "in_progress"
    assert httpx.get(f"{mk.base_url}/v1/vector_stores/{vs}/files/{fid}").json()["status"] == "in_progress"
    # control over HTTP
    httpx.post(f"{mk.base_url}/__mock/config", json={"poll_status": "completed"})
    assert httpx.get(f"{mk.base_url}/v1/vector_stores/{vs}/files/{fid}").json()["status"] == "completed"
    mk.configure(file_delete_fail_count=1)
    assert httpx.delete(f"{mk.base_url}/v1/files/{fid}").status_code == 500
    assert httpx.delete(f"{mk.base_url}/v1/files/{fid}").status_code == 200


def test_chunked_upload_is_parsed(mk):
    def gen():
        yield b"--BOUND\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n"
        yield b"--BOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"c.txt\"\r\nContent-Type: text/plain\r\n\r\n"
        yield b"hello chunked\r\n--BOUND--\r\n"

    r = httpx.post(f"{mk.base_url}/v1/files", content=gen(), headers={"Content-Type": "multipart/form-data; boundary=BOUND"})
    assert r.status_code == 200
    assert r.json()["bytes"] == len(b"hello chunked")
    assert r.json()["filename"] == "c.txt"


def test_summary_non_streaming_and_failure_control(mk):
    body = {"model": "mock-std-tiny", "input": [{"role": "user", "content": "Summarize the following conversation: abc-nonce"}]}
    r = httpx.post(f"{mk.base_url}/v1/responses", json=body)
    assert r.status_code == 200
    text = r.json()["output"][0]["content"][0]["text"]
    assert SUMMARY_MARKER in text and "<summary>" in text
    mk.configure(summary_fail={"match": "abc-nonce", "count": 1})
    assert httpx.post(f"{mk.base_url}/v1/responses", json=body).status_code == 500
    assert httpx.post(f"{mk.base_url}/v1/responses", json=body).status_code == 200
    assert len(mk.summary_requests(contains="abc-nonce")) == 3


def test_slow_stream_is_incremental(mk):
    body = {"model": "m", "stream": True, "input": [{"role": "user", "content": "x [[slow:3:1]]"}]}
    times = []
    t0 = time.time()
    with httpx.stream("POST", f"{mk.base_url}/v1/responses", json=body, timeout=30) as r:
        for line in r.iter_lines():
            if line.startswith("event: response.output_text.delta"):
                times.append(time.time() - t0)
    assert len(times) == 3
    assert times[0] < 0.8 and times[2] >= 1.8


def test_hang_detects_client_disconnect(mk):
    body = {"model": "m", "stream": True, "input": [{"role": "user", "content": "x [[hang]]"}]}
    with httpx.stream("POST", f"{mk.base_url}/v1/responses", json=body, timeout=30) as r:
        for line in r.iter_lines():
            if line.startswith("event: response.output_text.delta"):
                break
    # client closed: the mock notices on its next keepalive write
    deadline = time.time() + 10
    while time.time() < deadline:
        recs = mk.chat_requests()
        if recs and recs[0].get("client_disconnected"):
            break
        time.sleep(0.2)
    assert mk.chat_requests()[0].get("client_disconnected") is True


def test_hang_released_by_reset(mk):
    body = {"model": "m", "stream": True, "input": [{"role": "user", "content": "x [[hang]]"}]}
    done = threading.Event()

    def run():
        httpx.post(f"{mk.base_url}/v1/responses", json=body, timeout=30)
        done.set()

    threading.Thread(target=run, daemon=True).start()
    time.sleep(1.0)
    mk.release_hangs()
    assert done.wait(5)


def test_control_endpoints(mk):
    httpx.post(f"{mk.base_url}/__mock/reset")
    _stream(mk, "a")
    reqs = httpx.get(f"{mk.base_url}/__mock/requests").json()
    assert len(reqs) == 1 and reqs[0]["path"] == "/v1/responses"
    assert json.loads(reqs[0]["body_text"])["stream"] is True
    st = httpx.get(f"{mk.base_url}/__mock/state").json()
    assert "files" in st and "vector_stores" in st
