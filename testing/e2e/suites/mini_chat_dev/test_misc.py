"""Models, reactions, chat-delete cleanup, thread summary, auth, provider provisioning."""

from __future__ import annotations

import uuid

import httpx
import pytest

from .conftest import (
    PREFIX,
    PROVIDER_AUTH_UNREADABLE,
    RT_CHAT,
    RT_MESSAGE,
    RT_MODEL,
    USERS,
    assert_problem,
    dispose_server,
    field_reasons,
    new_server,
    outbox_payloads,
    ub,
    wait_until,
)
from .mock_llm import SUMMARY_TEXT
from .test_attachments import png_bytes

ENABLED_MODELS = ["gpt-4.1", "gpt-4.1-mini", "gpt-text-only", "gpt-tiny-ctx", "azure-gpt-4.1"]
MODEL_FIELDS = {"model_id", "display_name", "tier", "multiplier_display", "description", "multimodal_capabilities", "context_window"}


# ── Models API ─────────────────────────────────────────────────────────────


@pytest.mark.smoke
def test_models_list_shows_only_enabled_without_internals(api):
    r = api("A").get("/models")
    assert r.status_code == 200
    items = r.json()["items"]
    assert sorted(m["model_id"] for m in items) == sorted(ENABLED_MODELS)
    for m in items:
        assert set(m) <= MODEL_FIELDS, m
        assert {"model_id", "display_name", "tier", "multiplier_display", "multimodal_capabilities", "context_window"} <= set(m)
        assert m["tier"] in ("standard", "premium")
    prem = next(m for m in items if m["model_id"] == "gpt-4.1")
    assert prem == {
        "model_id": "gpt-4.1",
        "display_name": "GPT-4.1",
        "tier": "premium",
        "multiplier_display": "3x",
        "description": "GPT-4.1 (e2e)",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 1047576,
    }
    for leaked in ("provider", "credit_multiplier", "is_default", "policy_version", "max_output", "openai"):
        assert leaked not in r.text


def test_get_model_and_hidden_models(api):
    a = api("A")
    r = a.get("/models/gpt-4.1-mini")
    assert r.status_code == 200
    assert r.json()["model_id"] == "gpt-4.1-mini" and r.json()["tier"] == "standard"
    for missing in ("gpt-disabled", "no-such-model"):
        p = assert_problem(a.get(f"/models/{missing}"), 404)
        assert p["context"]["resource_type"] == RT_MODEL


# ── Reactions API ──────────────────────────────────────────────────────────


def test_reactions_set_replace_remove(api):
    a = api("A")
    chat = a.create_chat()
    a.send(chat["id"], "react to this")
    user_msg, asst = a.messages(chat["id"])
    path = f"/chats/{chat['id']}/messages/{asst['id']}/reaction"

    r = a.put(path, json={"reaction": "like"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["message_id"] == asst["id"] and body["reaction"] == "like" and body["created_at"]
    assert a.messages(chat["id"])[1]["my_reaction"] == "like"
    assert a.put(path, json={"reaction": "like"}).status_code == 200  # idempotent

    r = a.put(path, json={"reaction": "dislike"})
    assert r.status_code == 200 and r.json()["reaction"] == "dislike"
    msgs = a.messages(chat["id"])
    assert msgs[1]["my_reaction"] == "dislike" and msgs[0]["my_reaction"] is None

    # reactions are per user: B cannot even see the chat
    assert_problem(api("B").put(path, json={"reaction": "like"}), 404)

    assert a.delete(path).status_code == 204
    assert a.delete(path).status_code == 204  # idempotent
    assert a.messages(chat["id"])[1]["my_reaction"] is None


def test_reaction_validation(api):
    a = api("A")
    chat = a.create_chat()
    a.send(chat["id"], "x")
    user_msg, asst = a.messages(chat["id"])
    base = f"/chats/{chat['id']}/messages"
    p = assert_problem(a.put(f"{base}/{asst['id']}/reaction", json={"reaction": "love"}), 400)
    assert field_reasons(p) == ["INVALID_REACTION"]
    assert_problem(a.put(f"{base}/{asst['id']}/reaction", json={}), 422)
    for r in (
        a.put(f"{base}/{user_msg['id']}/reaction", json={"reaction": "like"}),
        a.delete(f"{base}/{user_msg['id']}/reaction"),
    ):
        p = assert_problem(r, 400)
        v = p["context"]["violations"][0]
        assert v["subject"] == "reaction_target" and v["type"] == "STATE"
    p = assert_problem(a.put(f"{base}/{uuid.uuid4()}/reaction", json={"reaction": "like"}), 404)
    assert p["context"]["resource_type"] == RT_MESSAGE
    p = assert_problem(a.put(f"/chats/{uuid.uuid4()}/messages/{asst['id']}/reaction", json={"reaction": "like"}), 404)
    assert p["context"]["resource_type"] == RT_CHAT


# ── Chat deletion cleanup ──────────────────────────────────────────────────


def test_chat_delete_enqueues_cleanup_and_deletes_provider_resources(api, mock_llm, db, provider):
    a = api("A")
    chat = a.create_chat(model=provider["model"])
    doc = a.upload(chat["id"], "c.txt", b"cleanup doc", "text/plain").json()
    img = a.upload(chat["id"], "c.png", png_bytes(), "image/png").json()
    a.send(chat["id"], "with attachments", attachment_ids=[doc["id"]])
    fids = {
        a_id: db.execute("SELECT provider_file_id FROM attachments WHERE id = ?", (ub(a_id),)).fetchone()[0]
        for a_id in (doc["id"], img["id"])
    }
    vs = db.execute("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", (ub(chat["id"]),)).fetchone()[0]

    assert a.delete(f"/chats/{chat['id']}").status_code == 204

    msgs = [m["payload"] for m in outbox_payloads(db, "mini-chat.chat_cleanup") if m["payload"].get("chat_id") == chat["id"]]
    assert len(msgs) == 1
    assert msgs[0]["reason"] == "chat_soft_delete"
    assert msgs[0]["tenant_id"] == USERS["A"]["tenant"]

    for fid in fids.values():
        assert wait_until(lambda fid=fid: mock_llm.find("DELETE", f"/v1/files/{fid}"), timeout=20), fid
    assert wait_until(lambda: mock_llm.find("DELETE", f"/v1/vector_stores/{vs}"), timeout=20)
    deletes = [r for r in mock_llm.find("DELETE") if r.path == f"/v1/vector_stores/{vs}"]
    assert deletes[0].listener == provider["name"] and deletes[0].query == provider["query"]
    # files are deleted before the vector store
    order = [r.path for r in mock_llm.find("DELETE") if r.path in {f"/v1/files/{f}" for f in fids.values()} | {f"/v1/vector_stores/{vs}"}]
    assert order[-1] == f"/v1/vector_stores/{vs}"
    assert wait_until(
        lambda: all(
            r[0] == "done"
            for r in db.execute("SELECT cleanup_status FROM attachments WHERE chat_id = ?", (ub(chat["id"]),)).fetchall()
        )
    )
    row = db.execute("SELECT deleted_at FROM chats WHERE id = ?", (ub(chat["id"]),)).fetchone()
    assert row["deleted_at"] is not None
    # child endpoints are gone with the chat
    assert_problem(a.get(f"/chats/{chat['id']}/attachments/{doc['id']}"), 404)
    assert_problem(a.get(f"/chats/{chat['id']}/messages"), 404)


def test_usage_and_audit_events_published_once_per_turn(api, db):
    a = api("A")
    chat = a.create_chat()
    res = a.send(chat["id"], "publish once")
    usage = wait_until(
        lambda: [m["payload"] for m in outbox_payloads(db, "mini-chat.usage_snapshot") if m["payload"].get("request_id") == res.request_id]
    )
    assert len(usage) == 1
    ev = usage[0]
    assert ev["chat_id"] == chat["id"] and ev["effective_model"] == "gpt-4.1"
    assert ev["actual_credits_micro"] == 42 * 3 + 7 * 15
    audit = [m["payload"] for m in outbox_payloads(db, "mini-chat.audit") if m["payload"].get("request_id") == res.request_id]
    assert [x["event_type"] for x in audit] == ["turn_completed"]


# ── Thread summary ─────────────────────────────────────────────────────────


def test_thread_summary_generated_and_applied(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat(model="gpt-tiny-ctx")  # budget 3072 tokens, threshold 80%
    big = "lorem ipsum dolor sit amet " * 300  # ~8100 bytes ~ 2000 tokens
    a.send(chat["id"], "first: " + big)
    a.send(chat["id"], "second: " + big)
    summary_req = wait_until(lambda: mock_llm.chat_requests(chat["id"], "summary"), timeout=30)
    assert summary_req, "no summary request reached the provider"
    body = summary_req[0].json
    assert body.get("stream") in (False, None)
    assert body["metadata"]["request_type"] == "summary"
    assert body["metadata"]["feature"] == "none"
    assert body["metadata"]["user_id"] == "11111111-6a88-4768-9dfc-6bcd5187d9ed"
    assert len(body["user"]) == 64
    row = wait_until(
        lambda: db.execute("SELECT summary_text FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),)).fetchone(),
        timeout=30,
    )
    assert row and SUMMARY_TEXT in row[0]

    res = a.send(chat["id"], "third: short")
    applied = res.started.get("thread_summary_applied")
    assert applied and applied["token_estimate"] > 0
    nxt = mock_llm.chat_requests(chat["id"])[-1].json
    # the summary is sent as one user-role message with the preamble
    summary_msg = nxt["input"][0]
    assert summary_msg["role"] == "user"
    assert summary_msg["content"].startswith("This conversation has earlier messages that have been summarized.")
    assert SUMMARY_TEXT in summary_msg["content"]
    # summarized messages are flagged in the DB
    flags = db.execute(
        "SELECT is_compressed FROM messages WHERE chat_id = ? ORDER BY created_at, id", (ub(chat["id"]),)
    ).fetchall()
    assert [f[0] for f in flags][:2] == [1, 1]


# ── Auth / error envelope ──────────────────────────────────────────────────


@pytest.mark.parametrize(
    "method,path",
    [
        ("GET", "/models"),
        ("GET", "/models/gpt-4.1"),
        ("GET", "/quota/status"),
        ("GET", f"/chats/{uuid.uuid4()}"),
        ("POST", f"/chats/{uuid.uuid4()}/messages:stream"),
        ("GET", f"/chats/{uuid.uuid4()}/turns/{uuid.uuid4()}"),
        ("DELETE", f"/chats/{uuid.uuid4()}/messages/{uuid.uuid4()}/reaction"),
    ],
)
def test_unauthenticated_requests_are_401_problem(server, method, path):
    r = httpx.request(method, f"{server.base_url}{PREFIX}{path}", json={"content": "x"} if method == "POST" else None)
    assert r.status_code == 401, r.text
    assert r.headers["content-type"].startswith("application/problem+json")
    p = r.json()
    assert p["status"] == 401 and {"type", "title", "detail", "context"} <= set(p)


def test_problem_envelope_fields(api):
    r = api("A").get(f"/chats/{uuid.uuid4()}")
    p = assert_problem(r, 404)
    assert p["type"].startswith("gts://") and p["title"] == "Not Found"
    assert p["instance"] == r.request.url.path
    assert p["trace_id"]
    assert p["context"]["resource_type"] == RT_CHAT


# ── Provider provisioning with an unreadable secret ────────────────────────


def test_server_starts_with_unreadable_provider_secret(mock_llm, server):
    """The provider's auth secret is not readable (credstore): provisioning is
    deferred, startup succeeds and the read APIs work."""
    srv = new_server(mock_llm, "deferred", PROVIDER_AUTH_UNREADABLE)
    try:
        headers = {"Authorization": f"Bearer {USERS['B']['token']}"}
        r = httpx.get(f"{srv.base_url}{PREFIX}/models", headers=headers)
        assert r.status_code == 200
        assert len(r.json()["items"]) == len(ENABLED_MODELS)
        r = httpx.post(f"{srv.base_url}{PREFIX}/chats", headers=headers, json={})
        assert r.status_code == 201
        log_path = srv.home / "logs" / "mini-chat.log"
        read_log = lambda: log_path.read_text() if log_path.exists() else ""  # noqa: E731
        assert wait_until(lambda: "OAGW provisioning done" in read_log()), read_log()[-2000:]
        assert "OAGW provisioning deferred" in read_log(), read_log()[-2000:]
    finally:
        dispose_server(srv)


# ── Recovery workers (orphan watchdog, upload reaper) ──────────────────────


def aged_ts(minutes: int = 10) -> str:
    import datetime as dt

    t = dt.datetime.now(dt.timezone.utc) - dt.timedelta(minutes=minutes)
    return t.strftime("%Y-%m-%dT%H:%M:%S.") + f"{t.microsecond * 1000:09d}Z"


def test_orphan_watchdog_finalizes_stale_turn_and_stream_is_interrupted(api, mock_llm, db):
    import threading

    from .mock_llm import held_stream

    a = api("A")
    chat = a.create_chat(model="gpt-4.1-mini")
    release = threading.Event()
    mock_llm.script_chat(chat["id"], held_stream(release))
    live = a.open_stream(chat["id"], "this turn will hang")
    try:
        rid = live.read_until("delta")[0][1]["request_id"]
        # the chat is blocked while the turn runs
        blocked = a.stream(chat["id"], "blocked?")
        assert blocked.status == 409 and blocked.problem["context"]["reason"] == "turn_already_running"
        # the pod "lost" the turn: no progress for 10 minutes (> orphan_watchdog.timeout_secs 300)
        old = aged_ts(10)
        db.execute(
            "UPDATE chat_turns SET started_at = ?, last_progress_at = ? WHERE request_id = ?", (old, old, ub(rid))
        )
        turn = a.wait_turn(chat["id"], rid, ("error",), timeout=20)
        assert turn["error_code"] == "orphan_timeout"
        assert "assistant_message_id" not in turn
    finally:
        release.set()
    events = live.read_all()
    live.close()
    # the provider finished after the watchdog won the CAS: no done, stream_interrupted instead
    names = [n for n, _ in events]
    assert "done" not in names
    assert events[-1] == ("error", events[-1][1]) and events[-1][1]["code"] == "stream_interrupted", events

    row = db.execute(
        "SELECT state, error_code, reserve_tokens, max_output_tokens_applied, minimal_generation_floor_applied "
        "FROM chat_turns WHERE request_id = ?",
        (ub(rid),),
    ).fetchone()
    assert row["state"] == "failed" and row["error_code"] == "orphan_timeout"
    usage = wait_until(
        lambda: [m["payload"] for m in outbox_payloads(db, "mini-chat.usage_snapshot") if m["payload"].get("request_id") == rid]
    )
    assert len(usage) == 1
    est_in = row["reserve_tokens"] - row["max_output_tokens_applied"]
    assert usage[0]["billing_outcome"] == "aborted" and usage[0]["settlement_method"] == "estimated"
    assert usage[0]["actual_credits_micro"] == est_in * 1 + row["minimal_generation_floor_applied"] * 3
    audit = [m["payload"] for m in outbox_payloads(db, "mini-chat.audit") if m["payload"].get("request_id") == rid]
    assert [x["event_type"] for x in audit] == ["turn_failed"]
    # the chat is usable again
    assert a.send(chat["id"], "after the watchdog").names[-1] == "done"


def test_upload_reaper_fails_abandoned_upload_and_deletes_provider_file(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    att = a.upload(chat["id"], "stuck.txt", b"stuck upload", "text/plain").json()
    fid = db.execute("SELECT provider_file_id FROM attachments WHERE id = ?", (ub(att["id"]),)).fetchone()[0]
    # simulate a request that died after the provider upload: row left `uploaded`, stale
    db.execute(
        "UPDATE attachments SET status = 'uploaded', updated_at = ? WHERE id = ?", (aged_ts(10), ub(att["id"]))
    )
    got = wait_until(
        lambda: (r := a.get(f"/chats/{chat['id']}/attachments/{att['id']}").json())["status"] == "failed" and r,
        timeout=20,
    )
    assert got["error_code"] == "upload_abandoned"
    assert wait_until(lambda: mock_llm.find("DELETE", f"/v1/files/{fid}"), timeout=20)
    row = db.execute("SELECT deleted_at, cleanup_status FROM attachments WHERE id = ?", (ub(att["id"]),)).fetchone()
    assert row["deleted_at"] is None
    assert wait_until(
        lambda: db.execute("SELECT cleanup_status FROM attachments WHERE id = ?", (ub(att["id"]),)).fetchone()[0] == "done"
    )


# ── Thread summary: provider failure retried, mutation invalidation ────────


def test_thread_summary_retried_after_provider_failure(api, mock_llm, db):
    from .mock_llm import json_response

    a = api("A")
    chat = a.create_chat(model="gpt-tiny-ctx")
    mock_llm.script(
        json_response(500, {"error": {"message": "summary backend down"}}),
        match=lambda r: r.chat_id == chat["id"] and r.request_type == "summary",
    )
    big = "lorem ipsum dolor sit amet " * 300
    a.send(chat["id"], "first: " + big)
    a.send(chat["id"], "second: " + big)
    row = wait_until(
        lambda: db.execute("SELECT summary_text FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),)).fetchone(),
        timeout=60,
    )
    assert row and SUMMARY_TEXT in row[0]
    assert len(mock_llm.chat_requests(chat["id"], "summary")) >= 2


def test_turn_delete_invalidates_covering_summary(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat(model="gpt-tiny-ctx")
    big = "lorem ipsum dolor sit amet " * 300
    first = a.send(chat["id"], "first: " + big)
    second = a.send(chat["id"], "second: " + big)
    assert wait_until(
        lambda: db.execute("SELECT 1 FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),)).fetchone(), timeout=30
    )
    compressed = lambda: db.execute(  # noqa: E731
        "SELECT COUNT(*) FROM messages WHERE chat_id = ? AND is_compressed = 1", (ub(chat["id"]),)
    ).fetchone()[0]
    assert compressed() > 0
    # deleting the latest turn (not covered by the summary) keeps it
    assert a.delete(f"/chats/{chat['id']}/turns/{second.request_id}").status_code == 204
    assert db.execute("SELECT 1 FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),)).fetchone()
    # deleting the covered turn drops the summary and clears is_compressed
    assert a.delete(f"/chats/{chat['id']}/turns/{first.request_id}").status_code == 204
    assert db.execute("SELECT 1 FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),)).fetchone() is None
    assert compressed() == 0
    res = a.send(chat["id"], "fresh start")
    assert "thread_summary_applied" not in res.started


# ── Concurrency ────────────────────────────────────────────────────────────


def test_concurrent_load_has_no_server_errors(api, server):
    """Several users stream, upload and delete concurrently while the outbox
    workers process the resulting usage/audit/cleanup events: no 5xx."""
    from concurrent.futures import ThreadPoolExecutor

    from .conftest import Api

    def worker(i: int) -> list:
        c = Api(server.base_url, USERS["AB"[i % 2]]["token"])
        out = []
        chat = c.create_chat(model="gpt-4.1-mini")
        for j in range(3):
            r = c.stream(chat["id"], f"load {i}-{j}")
            out.append(("stream", r.status, r.names[-1] if r.events else r.body[:200]))
        up = c.upload(chat["id"], f"load{i}.txt", f"load doc {i}".encode(), "text/plain")
        out.append(("upload", up.status_code, up.text[:200]))
        r = c.stream(chat["id"], "after upload")
        out.append(("stream", r.status, r.names[-1] if r.events else r.body[:200]))
        out.append(("delete", c.delete(f"/chats/{chat['id']}").status_code, ""))
        c.client.close()
        return out

    with ThreadPoolExecutor(8) as ex:
        results = [x for xs in ex.map(worker, range(16)) for x in xs]
    bad = [r for r in results if r[1] >= 500 or (r[0] == "stream" and (r[1] != 200 or r[2] != "done"))]
    assert not bad, bad


# ── Config: every documented key at its DESIGN default ─────────────────────

DESIGN_DEFAULTS = {
    "url_prefix": "/mini-chat",
    "vendor": "constructorfabric",
    "metrics": {"prefix": ""},
    "streaming": {"sse_ping_interval_seconds": 15, "sse_channel_capacity": 32, "max_output_tokens": 32768},
    "estimation_budgets": {
        "minimal_generation_floor": 50,
        "bytes_per_token_conservative": 4,
        "fixed_overhead_tokens": 100,
        "safety_margin_pct": 10,
        "image_token_budget": 1000,
        "tool_surcharge_tokens": 500,
        "web_search_surcharge_tokens": 500,
        "code_interpreter_surcharge_tokens": 1000,
    },
    "quota": {
        "overshoot_tolerance_factor": 1.10,
        "warning_threshold_pct": 80,
        "web_search_max_calls_per_message": 2,
        "web_search_daily_quota": 75,
        "code_interpreter_max_calls_per_message": 10,
        "code_interpreter_daily_quota": 50,
    },
    "outbox": {
        "queue_name": "mini-chat.usage_snapshot",
        "cleanup_queue_name": "mini-chat.attachment_cleanup",
        "chat_cleanup_queue_name": "mini-chat.chat_cleanup",
        "thread_summary_queue_name": "mini-chat.thread_summary",
        "audit_queue_name": "mini-chat.audit",
        "num_partitions": 4,
    },
    "context": {"recent_messages_limit": 10},
    "rag": {
        "uploaded_file_max_size_kb": 25600,
        "uploaded_image_max_size_kb": 5120,
        "max_images_per_message": 4,
        "max_documents_per_chat": 50,
        "max_total_upload_mb_per_chat": 100,
        "allow_csv_upload": True,
        "max_concurrent_uploads": 10,
    },
    "thumbnail": {"width": 128, "height": 128, "max_bytes": 131072, "max_pixels": 100000000, "max_decode_bytes": 33554432},
    "orphan_watchdog": {"enabled": True, "timeout_secs": 300, "scan_interval_secs": 60},
    "upload_reaper": {"enabled": True, "scan_interval_secs": 60, "stale_after_secs": 300},
    "thread_summary_worker": {
        "enabled": True,
        "claim_timeout_secs": 300,
        "max_attempts": 3,
        "compression_threshold_pct": 80,
        "summary_model_id": "",
        "message_content_limit": 4000,
        "reconcile_interval_secs": 60,
    },
    "cleanup_worker": {
        "max_attempts": 5,
        "enabled": True,
        "poll_interval_secs": 60,
        "reconcile_interval_secs": 300,
        "stale_in_progress_timeout_secs": 900,
        "batch_size": 32,
    },
    "knowledge_search": {"enabled": False, "max_calls_per_message": 3, "top_k": 5, "max_chunk_chars": 2000},
    "providers": {"openai": {"storage_backend": "openai"}},
}


def test_server_starts_with_every_design_key_at_default(mock_llm, server):
    overrides = {
        "gears": {
            "mini-chat": {"config": DESIGN_DEFAULTS},
            "static-mini-chat-model-policy-plugin": {
                "config": {
                    "vendor": "constructorfabric",
                    "priority": 100,
                    "kill_switches": {
                        "disable_premium_tier": False,
                        "force_standard_tier": False,
                        "disable_web_search": False,
                        "disable_file_search": False,
                        "disable_images": False,
                        "disable_code_interpreter": False,
                    },
                    "default_standard_limits": {"limit_daily_credits_micro": 100000000, "limit_monthly_credits_micro": 1000000000},
                    "default_premium_limits": {"limit_daily_credits_micro": 50000000, "limit_monthly_credits_micro": 500000000},
                }
            },
            "static-mini-chat-audit-plugin": {"config": {"vendor": "constructorfabric", "priority": 100, "enabled": True}},
        }
    }
    srv = new_server(mock_llm, "defaults", overrides=overrides)
    try:
        from .conftest import Api

        a = Api(srv.base_url, USERS["B"]["token"])
        chat = a.create_chat()
        assert a.send(chat["id"], "defaults").names[-1] == "done"
        a.client.close()
    finally:
        dispose_server(srv)


def test_unknown_mini_chat_config_key_fails_startup(mock_llm, server):
    with pytest.raises(RuntimeError, match="exited early|not ready"):
        new_server(
            mock_llm,
            "badkey",
            overrides={"gears": {"mini-chat": {"config": {"streaming": {"web_search_context_size": "low"}}}}},
        )


def test_kill_switches(mock_llm, server):
    """A second server with every kill switch on (static policy plugin)."""
    from .conftest import Api
    from .test_attachments import XLSX_MIME, png_bytes, xlsx_bytes

    switches = {
        "force_standard_tier": True,
        "disable_web_search": True,
        "disable_images": True,
        "disable_code_interpreter": True,
        "disable_file_search": True,
    }
    overrides = {"gears": {"static-mini-chat-model-policy-plugin": {"config": {"kill_switches": switches}}}}
    srv = new_server(mock_llm, "kill", overrides=overrides)
    try:
        a = Api(srv.base_url, USERS["B"]["token"])
        chat = a.create_chat()
        r = a.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x", "web_search": {"enabled": True}})
        p = assert_problem(r, 400)
        assert p["context"]["violations"][0]["subject"] == "web_search"
        assert p["context"]["violations"][0]["type"] == "FEATURE_DISABLED"

        p = assert_problem(a.upload(chat["id"], "k.png", png_bytes(), "image/png"), 400)
        assert p["context"]["violations"][0]["subject"] == "images"

        p = assert_problem(a.upload(chat["id"], "k.xlsx", xlsx_bytes(), XLSX_MIME), 400)
        assert "CODE_INTERPRETER_UNAVAILABLE" in field_reasons(p)

        doc = a.upload(chat["id"], "k.txt", b"kill switch doc", "text/plain")
        assert doc.status_code == 201, doc.text

        res = a.send(chat["id"], "premium is forced down")
        d = res.done
        assert d["quota_decision"] == "downgrade" and d["downgrade_reason"] == "force_standard_tier"
        assert d["effective_model"] == "gpt-4.1-mini" and d["downgrade_from"] == "gpt-4.1"
        body = mock_llm.chat_requests(chat["id"])[-1].json
        assert not [t for t in body.get("tools") or [] if t["type"] in ("file_search", "web_search", "code_interpreter")]
        a.client.close()
    finally:
        dispose_server(srv)
