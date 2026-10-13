"""Config variant `exhausted`: total daily limit (5000 credits) below one reserve, so every turn is
rejected at preflight (DESIGN §5.4.2 Downgrade Decision Flow step 4, ADR-0004 resource_exhausted)."""

import uuid

import pytest

from conftest import assert_problem

VARIANT = "exhausted"


@pytest.mark.parametrize("model", ["gpt-4.1", "gpt-4.1-mini"])
def test_turn_rejected_before_stream(api, fresh_mock, db, model):
    chat = api.create_chat(model=model)
    rid = str(uuid.uuid4())
    res = api.stream(chat["id"], "hello", request_id=rid)
    assert res.events == []  # JSON error, no SSE stream opened
    assert_problem(res.problem, 429, "resource_exhausted",
                   violation={"subject": "tokens", "description": "quota_exceeded"})
    assert fresh_mock.responses_calls() == []
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 0
    assert db.query("SELECT id FROM chat_turns WHERE chat_id = ?", (uuid.UUID(chat["id"]).bytes,)) == []
    assert all(r["reserved_credits_micro"] == 0 for r in db.quota_rows())
    # The request id was not consumed.
    assert api.get(f"/chats/{chat['id']}/turns/{rid}").status_code == 404


def test_quota_status_reflects_limits(api):
    q = api.get("/quota/status").json()
    total = next(t for t in q["tiers"] if t["tier"] == "total")
    daily = next(p for p in total["periods"] if p["period"] == "daily")
    assert daily["limit_credits_micro"] == 5000
    assert daily["used_credits_micro"] == 0 and daily["remaining_percentage"] == 100
