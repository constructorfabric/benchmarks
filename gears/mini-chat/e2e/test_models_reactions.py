"""Models API and message reactions.

Acceptance criteria covered:
* Models API — "Read-only model list/get reflects only enabled catalog entries, without exposing internal fields"
* Reactions API — "Set/remove reaction on assistant messages only, idempotently"
"""

from __future__ import annotations

import uuid

from helpers import (
    RT_CHAT,
    RT_MESSAGE,
    RT_MODEL,
    assert_not_found,
    assert_problem,
    list_messages,
    new_chat,
    rows,
    send_ok,
    ub,
)

FORBIDDEN_MODEL_KEYS = {
    "provider_id",
    "provider",
    "provider_model_id",
    "provider_display_name",
    "input_tokens_credit_multiplier_micro",
    "output_tokens_credit_multiplier_micro",
    "credits_micro",
    "max_output_tokens",
    "max_output",
    "max_input_tokens",
    "is_default",
    "preference",
    "policy_version",
    "system_prompt",
    "estimation_budgets",
    "general_config",
    "enabled",
}


# ── models ─────────────────────────────────────────────────────────────────
def test_list_models_enabled_only_projection(fresh):
    r = fresh.req("GET", "/models")
    assert r.status_code == 200, r.text
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert set(ids) == {"gpt-4.1", "gpt-4.1-mini", "std-novision", "tiny-ctx"}
    assert "disabled-model" not in ids
    for m in items:
        assert {"model_id", "display_name", "tier", "multiplier_display", "multimodal_capabilities", "context_window"} <= set(m)
        assert not (FORBIDDEN_MODEL_KEYS & set(m)), f"internal fields exposed: {FORBIDDEN_MODEL_KEYS & set(m)}"
        assert m["tier"] in ("standard", "premium")
    by = {m["model_id"]: m for m in items}
    assert by["gpt-4.1"]["tier"] == "premium"
    assert by["gpt-4.1-mini"]["tier"] == "standard"
    assert by["gpt-4.1"]["display_name"] == "GPT-4.1"
    assert by["gpt-4.1"]["multiplier_display"] == "1x"
    assert by["gpt-4.1"]["description"] == "gpt-4.1 description"
    assert by["gpt-4.1"]["context_window"] == 1047576
    assert by["tiny-ctx"]["context_window"] == 4096
    assert by["gpt-4.1"]["multimodal_capabilities"] == ["VISION_INPUT"]
    assert by["std-novision"]["multimodal_capabilities"] == []


def test_get_model(fresh):
    r = fresh.req("GET", "/models/gpt-4.1-mini")
    assert r.status_code == 200, r.text
    m = r.json()
    assert m["model_id"] == "gpt-4.1-mini" and m["tier"] == "standard"
    assert not (FORBIDDEN_MODEL_KEYS & set(m))
    listed = {x["model_id"]: x for x in fresh.req("GET", "/models").json()["items"]}
    assert listed["gpt-4.1-mini"] == m, "get returns the same projection as list"


def test_get_disabled_or_missing_model_404(fresh):
    assert_not_found(fresh.req("GET", "/models/disabled-model"), RT_MODEL)
    assert_not_found(fresh.req("GET", "/models/does-not-exist"), RT_MODEL)


def test_models_are_read_only(fresh):
    for method in ("POST", "PUT", "DELETE", "PATCH"):
        r = fresh.req(method, "/models/gpt-4.1", json={})
        assert r.status_code in (404, 405), (method, r.status_code)


# ── reactions ─────────────────────────────────────────────────────────────
def _turn(srv):
    cid = new_chat(srv, "gpt-4.1-mini")
    started, _, _ = send_ok(srv, cid, "react to me")
    msgs = list_messages(srv, cid)
    return cid, msgs[0]["id"], started["message_id"]


def _reaction_rows(srv, msg_id):
    return rows(srv, "SELECT * FROM message_reactions WHERE message_id = ?", (ub(msg_id),))


def test_put_reaction_upsert(fresh):
    cid, _, asst = _turn(fresh)
    r = fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", json={"reaction": "like"})
    assert r.status_code == 200, r.text
    j = r.json()
    assert j["message_id"] == asst and j["reaction"] == "like" and j["created_at"]
    r = fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", json={"reaction": "dislike"})
    assert r.status_code == 200, r.text
    assert r.json()["reaction"] == "dislike"
    rs = _reaction_rows(fresh, asst)
    assert len(rs) == 1 and rs[0]["reaction"] == "dislike"
    msgs = list_messages(fresh, cid)
    assert msgs[1]["my_reaction"] == "dislike"
    assert msgs[0]["my_reaction"] is None
    # Same value again: still one row.
    assert fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", json={"reaction": "dislike"}).status_code == 200
    assert len(_reaction_rows(fresh, asst)) == 1


def test_reaction_is_per_user_view(fresh):
    """my_reaction reflects the requesting user's reaction only (owner-only chat: other users see 404)."""
    cid, _, asst = _turn(fresh)
    fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", json={"reaction": "like"})
    assert_not_found(fresh.req("GET", f"/chats/{cid}/messages", "a2"), RT_CHAT)
    assert list_messages(fresh, cid)[1]["my_reaction"] == "like"


def test_delete_reaction_idempotent(fresh):
    cid, _, asst = _turn(fresh)
    fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", json={"reaction": "like"})
    r = fresh.req("DELETE", f"/chats/{cid}/messages/{asst}/reaction")
    assert r.status_code == 204, r.text
    assert _reaction_rows(fresh, asst) == []
    assert list_messages(fresh, cid)[1]["my_reaction"] is None
    r = fresh.req("DELETE", f"/chats/{cid}/messages/{asst}/reaction")
    assert r.status_code == 204, "deleting a missing reaction is still 204"


def test_reaction_validation(fresh):
    cid, user_msg, asst = _turn(fresh)
    r = fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", json={"reaction": "love"})
    assert_problem(r, 400, field_reason="INVALID_REACTION")
    r = fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", json={})
    assert r.status_code == 422, r.text
    # Invalid value is checked before authorization / chat lookup.
    r = fresh.req("PUT", f"/chats/{uuid.uuid4()}/messages/{asst}/reaction", json={"reaction": "love"})
    assert_problem(r, 400, field_reason="INVALID_REACTION")


def test_reaction_on_user_message_rejected(fresh):
    cid, user_msg, _ = _turn(fresh)
    for method, body in (("PUT", {"reaction": "like"}), ("DELETE", None)):
        kw = {"json": body} if body else {}
        r = fresh.req(method, f"/chats/{cid}/messages/{user_msg}/reaction", **kw)
        assert_problem(r, 400, subject="reaction_target", vtype="STATE")
    assert _reaction_rows(fresh, user_msg) == []


def test_reaction_unknown_message_and_chat(fresh):
    cid, _, asst = _turn(fresh)
    other_cid, _, other_asst = _turn(fresh)
    # Message of another chat → message not found.
    r = fresh.req("PUT", f"/chats/{cid}/messages/{other_asst}/reaction", json={"reaction": "like"})
    assert_not_found(r, RT_MESSAGE)
    r = fresh.req("PUT", f"/chats/{cid}/messages/{uuid.uuid4()}/reaction", json={"reaction": "like"})
    assert_not_found(r, RT_MESSAGE)
    r = fresh.req("DELETE", f"/chats/{cid}/messages/{uuid.uuid4()}/reaction")
    assert_not_found(r, RT_MESSAGE)
    # Inaccessible chat → chat not found.
    r = fresh.req("PUT", f"/chats/{cid}/messages/{asst}/reaction", "b", json={"reaction": "like"})
    assert_not_found(r, RT_CHAT)
    r = fresh.req("DELETE", f"/chats/{cid}/messages/{asst}/reaction", "a2")
    assert_not_found(r, RT_CHAT)
    assert _reaction_rows(fresh, asst) == []
