"""Black-box tests of mini-chat: REST + SSE through the real server, provider calls answered by
``mock_provider.py``, persisted state read from ``mini_chat.db`` with ``sqlite3``."""

import json
import sqlite3
import struct
import threading
import time
import uuid
import zlib
from contextlib import closing

import httpx
import pytest

from conftest import PREMIUM_MODEL, STANDARD_MODEL

ANSWER = "Hello from mock"


# ── helpers ────────────────────────────────────────────────────────────────────────────────


def create_chat(api, model=STANDARD_MODEL, title="black-box"):
    res = api.post("/chats", json={"title": title, "model": model})
    assert res.status_code == 201, res.text
    return res.json()


def parse_sse(lines):
    """``[(event, data)]`` of an SSE line iterator."""
    events, name, data = [], None, []
    for line in lines:
        if line == "":
            if name is not None or data:
                payload = "\n".join(data)
                events.append((name, json.loads(payload) if payload else None))
            name, data = None, []
        elif line.startswith(":"):
            continue
        elif line.startswith("event:"):
            name = line[len("event:"):].strip()
        elif line.startswith("data:"):
            data.append(line[len("data:"):].lstrip())
    if name is not None or data:
        payload = "\n".join(data)
        events.append((name, json.loads(payload) if payload else None))
    return events


def stream(api, chat_id, content, request_id=None):
    """Sends a message; returns ``(status, events)`` (``events`` empty on a non-SSE answer)."""
    body = {"content": content}
    if request_id is not None:
        body["request_id"] = str(request_id)
    with api.stream("POST", f"/chats/{chat_id}/messages:stream", json=body) as res:
        if not res.headers.get("content-type", "").startswith("text/event-stream"):
            res.read()
            return res.status_code, res.text
        return res.status_code, parse_sse(res.iter_lines())


def names(events):
    return [name for name, _ in events if name != "ping"]


def db(server):
    """A read-only connection; use as ``with closing(db(server)) as conn``."""
    conn = sqlite3.connect(f"file:{server.db}?mode=ro", uri=True, timeout=10)
    conn.row_factory = sqlite3.Row
    return conn


def uid(value):
    """A UUID as stored by SQLite (16-byte BLOB)."""
    return uuid.UUID(str(value)).bytes


def wait_until(desc, check, timeout=20.0):
    deadline = time.monotonic() + timeout
    while True:
        result = check()
        if result:
            return result
        if time.monotonic() > deadline:
            pytest.fail(f"timed out waiting for {desc}")
        time.sleep(0.2)


def png_bytes(width=8, height=8):
    """A minimal valid RGB PNG."""

    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    raw = b"".join(b"\x00" + b"\xff\x00\x00" * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def pdf_bytes():
    return (
        b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n"
        b"2 0 obj << /Type /Pages /Kids [] /Count 0 >> endobj\n"
        b"trailer << /Root 1 0 R >>\n%%EOF\n"
    )


def upload(api, chat_id, filename, content_type, data):
    res = api.post(
        f"/chats/{chat_id}/attachments",
        files={"file": (filename, data, content_type)},
    )
    assert res.status_code == 201, res.text
    att = res.json()
    return wait_until(
        f"attachment {filename} ready",
        lambda: (
            lambda a: a if a["status"] in ("ready", "failed") else None
        )(api.get(f"/chats/{chat_id}/attachments/{att['id']}").json()),
    )


def provider_calls(server, method, path_prefix):
    return [
        r
        for r in server.mock_requests()
        if r["method"] == method and r["path"].startswith(path_prefix)
    ]


# ── tests ──────────────────────────────────────────────────────────────────────────────────


def test_models_list_and_get(api):
    res = api.get("/models")
    assert res.status_code == 200, res.text
    ids = [m["model_id"] for m in res.json()["items"]]
    assert ids == [PREMIUM_MODEL, STANDARD_MODEL]
    tiers = {m["model_id"]: m["tier"] for m in res.json()["items"]}
    assert tiers == {PREMIUM_MODEL: "premium", STANDARD_MODEL: "standard"}

    res = api.get(f"/models/{STANDARD_MODEL}")
    assert res.status_code == 200, res.text
    assert res.json()["model_id"] == STANDARD_MODEL
    assert api.get("/models/no-such-model").status_code == 404


def test_chat_create_list_get_rename_delete(api):
    chat = create_chat(api, title="first title")
    assert chat["model"] == STANDARD_MODEL
    assert chat["title"] == "first title"
    assert chat["message_count"] == 0

    default = create_chat(api, model=None, title="default model")
    assert default["model"] == PREMIUM_MODEL, "the catalog default is the premium model"

    listed = api.get("/chats")
    assert listed.status_code == 200, listed.text
    listed_ids = [c["id"] for c in listed.json()["items"]]
    assert chat["id"] in listed_ids and default["id"] in listed_ids

    got = api.get(f"/chats/{chat['id']}")
    assert got.status_code == 200 and got.json()["id"] == chat["id"]

    renamed = api.patch(f"/chats/{chat['id']}", json={"title": "renamed"})
    assert renamed.status_code == 200, renamed.text
    assert renamed.json()["title"] == "renamed"
    assert api.get(f"/chats/{chat['id']}").json()["title"] == "renamed"

    assert api.delete(f"/chats/{chat['id']}").status_code == 204
    assert api.get(f"/chats/{chat['id']}").status_code == 404
    assert chat["id"] not in [c["id"] for c in api.get("/chats").json()["items"]]


def test_stream_message_events_and_persisted_rows(api, server):
    chat = create_chat(api)
    request_id = uuid.uuid4()
    calls_before = len(provider_calls(server, "POST", "/v1/responses"))

    status, events = stream(api, chat["id"], "Say hello", request_id)

    assert status == 200, events
    order = names(events)
    assert order[0] == "stream_started"
    assert order[-1] == "done", order
    assert set(order[1:-1]) == {"delta"}, order
    started = events[0][1]
    assert started["request_id"] == str(request_id)
    assert started["is_new_turn"] is True
    text = "".join(d["content"] for n, d in events if n == "delta")
    assert text == ANSWER
    done = events[-1][1]
    assert done["usage"] == {"input_tokens": 12, "output_tokens": 3}
    assert done["effective_model"] == STANDARD_MODEL
    assert done["selected_model"] == STANDARD_MODEL
    assert done["quota_decision"] == "allow"

    # One provider call, addressed by the provider model id, carrying the user's text.
    calls = provider_calls(server, "POST", "/v1/responses")
    assert len(calls) == calls_before + 1
    assert calls[-1]["json"]["model"] == f"{STANDARD_MODEL}-provider"
    assert "Say hello" in json.dumps(calls[-1]["json"]["input"])

    with closing(db(server)) as conn:
        turn = conn.execute(
            "SELECT state, assistant_message_id, error_code, reserved_credits_micro "
            "FROM chat_turns WHERE chat_id = ? AND request_id = ?",
            (uid(chat["id"]), uid(request_id)),
        ).fetchone()
        assert turn is not None
        assert turn["state"] == "completed"
        assert turn["error_code"] is None
        assert uuid.UUID(bytes=turn["assistant_message_id"]) == uuid.UUID(started["message_id"])

        rows = conn.execute(
            "SELECT role, content, input_tokens, output_tokens FROM messages "
            "WHERE chat_id = ? AND deleted_at IS NULL ORDER BY created_at, role DESC",
            (uid(chat["id"]),),
        ).fetchall()
        by_role = {r["role"]: r for r in rows}
        assert set(by_role) == {"user", "assistant"}
        assert by_role["user"]["content"] == "Say hello"
        assert by_role["assistant"]["content"] == ANSWER
        assert (by_role["assistant"]["input_tokens"], by_role["assistant"]["output_tokens"]) == (12, 3)

        quota = conn.execute(
            "SELECT period_type, bucket, spent_credits_micro, reserved_credits_micro, calls, "
            "input_tokens, output_tokens FROM quota_usage"
        ).fetchall()
        assert quota, "settlement wrote quota_usage rows"
        assert all(q["reserved_credits_micro"] == 0 for q in quota), [dict(q) for q in quota]
        assert any(q["spent_credits_micro"] > 0 and q["calls"] >= 1 for q in quota)

    msgs = api.get(f"/chats/{chat['id']}/messages")
    assert msgs.status_code == 200, msgs.text
    assert [(m["role"], m["content"]) for m in msgs.json()["items"]] == [
        ("user", "Say hello"),
        ("assistant", ANSWER),
    ]
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 2


def test_replay_of_a_completed_turn_does_not_call_the_provider(api, server):
    chat = create_chat(api)
    request_id = uuid.uuid4()
    status, first = stream(api, chat["id"], "Replay me", request_id)
    assert status == 200 and names(first)[-1] == "done"
    calls = len(provider_calls(server, "POST", "/v1/responses"))

    status, replay = stream(api, chat["id"], "Replay me", request_id)

    assert status == 200, replay
    order = names(replay)
    assert order[0] == "stream_started" and order[-1] == "done", order
    assert replay[0][1]["is_new_turn"] is False
    assert replay[0][1]["message_id"] == first[0][1]["message_id"]
    assert "".join(d["content"] for n, d in replay if n == "delta") == ANSWER
    assert len(provider_calls(server, "POST", "/v1/responses")) == calls


def test_turn_status(api):
    chat = create_chat(api)
    request_id = uuid.uuid4()
    status, events = stream(api, chat["id"], "Status please", request_id)
    assert status == 200, events

    res = api.get(f"/chats/{chat['id']}/turns/{request_id}")
    assert res.status_code == 200, res.text
    body = res.json()
    assert body["request_id"] == str(request_id)
    assert body["state"] == "done"
    assert body["assistant_message_id"] == events[0][1]["message_id"]

    assert api.get(f"/chats/{chat['id']}/turns/{uuid.uuid4()}").status_code == 404


def test_quota_status_reports_usage(api):
    chat = create_chat(api)
    status, _ = stream(api, chat["id"], "Spend some credits")
    assert status == 200

    res = api.get("/quota/status")
    assert res.status_code == 200, res.text
    body = res.json()
    tiers = {t["tier"]: t for t in body["tiers"]}
    assert set(tiers) == {"total", "premium"}
    total = tiers["total"]["periods"]
    assert {p["period"] for p in total} == {"daily", "monthly"}
    # A standard-model turn counts toward the total tier only.
    assert all(p["used_credits_micro"] > 0 for p in total)
    for p in total:
        assert p["remaining_credits_micro"] == p["limit_credits_micro"] - p["used_credits_micro"]
    assert 0 < body["warning_threshold_pct"] <= 100


def test_upload_pdf_and_image(api, server):
    chat = create_chat(api, model=PREMIUM_MODEL)

    pdf = upload(api, chat["id"], "report.pdf", "application/pdf", pdf_bytes())
    assert pdf["status"] == "ready", pdf
    assert pdf["kind"] == "document"
    assert pdf["filename"] == "report.pdf"
    assert pdf["size_bytes"] == len(pdf_bytes())

    image = upload(api, chat["id"], "pixel.png", "image/png", png_bytes())
    assert image["status"] == "ready", image
    assert image["kind"] == "image"
    assert image["img_thumbnail"]["content_type"].startswith("image/")

    files = provider_calls(server, "POST", "/v1/files")
    uploaded = [f["file"]["filename"] for f in files if f.get("file")]
    assert f"{chat['id']}_{pdf['id']}.pdf" in uploaded
    assert f"{chat['id']}_{image['id']}.png" in uploaded
    assert provider_calls(server, "POST", "/v1/vector_stores")

    # No provider identifier leaks into the API.
    for body in (pdf, image):
        text = json.dumps(body)
        assert "file-" not in text and "vs_" not in text

    with closing(db(server)) as conn:
        row = conn.execute(
            "SELECT vector_store_id, file_count FROM chat_vector_stores WHERE chat_id = ?",
            (uid(chat["id"]),),
        ).fetchone()
    assert row is not None and row["vector_store_id"].startswith("vs_")
    assert provider_calls(server, "POST", f"/v1/vector_stores/{row['vector_store_id']}/files")


def test_delete_chat_deletes_the_vector_store(api, server):
    chat = create_chat(api, model=PREMIUM_MODEL)
    att = upload(api, chat["id"], "notes.pdf", "application/pdf", pdf_bytes())
    assert att["status"] == "ready", att
    with closing(db(server)) as conn:
        vs_id = conn.execute(
            "SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?",
            (uid(chat["id"]),),
        ).fetchone()["vector_store_id"]
        file_id = conn.execute(
            "SELECT provider_file_id FROM attachments WHERE id = ?", (uid(att["id"]),)
        ).fetchone()["provider_file_id"]
    assert file_id.startswith("file-")

    assert api.delete(f"/chats/{chat['id']}").status_code == 204

    wait_until(
        f"DELETE /v1/vector_stores/{vs_id}",
        lambda: any(
            r["path"] == f"/v1/vector_stores/{vs_id}"
            for r in provider_calls(server, "DELETE", "/v1/vector_stores/")
        ),
        timeout=30,
    )
    wait_until(
        f"DELETE /v1/files/{file_id}",
        lambda: any(
            r["path"] == f"/v1/files/{file_id}"
            for r in provider_calls(server, "DELETE", "/v1/files/")
        ),
        timeout=30,
    )


def test_parallel_turn_in_one_chat_is_rejected_with_409(api, server):
    chat = create_chat(api)
    started = threading.Event()
    result = {}

    def slow_turn():
        body = {"content": "[slow] take your time"}
        with httpx.Client(base_url=str(api.base_url), timeout=60) as client:
            with client.stream(
                "POST", f"/chats/{chat['id']}/messages:stream", json=body
            ) as res:
                result["status"] = res.status_code
                lines = []
                for line in res.iter_lines():
                    lines.append(line)
                    if line.startswith("event: stream_started"):
                        started.set()
                result["events"] = parse_sse(lines)
        started.set()

    worker = threading.Thread(target=slow_turn, daemon=True)
    worker.start()
    assert started.wait(20), "the first turn never started"

    try:
        status, body = stream(api, chat["id"], "me too")
        assert status == 409, body
        assert "turn_already_running" in body
    finally:
        # The first turn's provider stream is held until released.
        httpx.post(f"{server.mock_url}/_mock/release", timeout=5)

    worker.join(30)
    assert not worker.is_alive()
    assert result["status"] == 200
    assert names(result["events"])[-1] == "done"

    with closing(db(server)) as conn:
        states = [
            r["state"]
            for r in conn.execute(
                "SELECT state FROM chat_turns WHERE chat_id = ?", (uid(chat["id"]),)
            )
        ]
        reserved = conn.execute(
            "SELECT COALESCE(SUM(reserved_credits_micro), 0) FROM quota_usage"
        ).fetchone()[0]
    assert states == ["completed"], "the rejected request left no turn"
    assert reserved == 0, "no reserve left behind"


def test_reaction_lifecycle(api, server):
    chat = create_chat(api)
    status, events = stream(api, chat["id"], "React to this")
    assert status == 200, events
    message_id = events[0][1]["message_id"]
    base = f"/chats/{chat['id']}/messages/{message_id}/reaction"

    res = api.put(base, json={"reaction": "like"})
    assert res.status_code == 200, res.text
    assert res.json()["message_id"] == message_id
    assert res.json()["reaction"] == "like"

    res = api.put(base, json={"reaction": "dislike"})
    assert res.status_code == 200 and res.json()["reaction"] == "dislike"

    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assistant = next(m for m in msgs if m["id"] == message_id)
    assert assistant["my_reaction"] == "dislike"

    assert api.put(base, json={"reaction": "love"}).status_code == 400

    with closing(db(server)) as conn:
        count = conn.execute(
            "SELECT COUNT(*) FROM message_reactions WHERE message_id = ?", (uid(message_id),)
        ).fetchone()[0]
    assert count == 1

    assert api.delete(base).status_code == 204
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assert next(m for m in msgs if m["id"] == message_id)["my_reaction"] is None
