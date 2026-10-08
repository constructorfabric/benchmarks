"""Web search (acceptance: Web Search)."""

from mchelpers import assert_problem, assert_sse_grammar, find_tool, nonce


# Acceptance: Web search — reported (tool events), cited (citations), accounted (turn counter)
def test_web_search_reported_cited_accounted(api, db, mock_llm):
    c = api.create_chat()
    n = nonce()
    s = api.stream(c["id"], f"news [[websearch:2]] {n}", web_search=True)
    assert_sse_grammar(s)
    tools = [(t["phase"], t["name"]) for t in s.of("tool")]
    assert tools == [("start", "web_search"), ("done", "web_search")] * 2
    cits = s.of("citations")
    assert len(cits) == 1 and any(i["source"] == "web" for i in cits[0]["items"])
    assert s.done
    row = db.turn_row(c["id"], s.request_id)
    assert row["web_search_enabled"] in (1, True)
    assert row["web_search_completed_count"] == 2
    req = mock_llm.chat_requests(contains=n)[-1]
    assert find_tool(req, "web_search") is not None


# Acceptance: Web search — tool only when requested
def test_web_search_not_requested(api, db, mock_llm):
    c = api.create_chat()
    n = nonce()
    s = api.stream(c["id"], f"no search {n}")
    assert s.done
    assert find_tool(mock_llm.chat_requests(contains=n)[-1], "web_search") is None
    assert db.turn_row(c["id"], s.request_id)["web_search_enabled"] in (0, False)


# Acceptance: Web search — per-turn call limit (default 2) enforced mid-turn
def test_web_search_per_turn_limit(api, db):
    c = api.create_chat()
    s = api.stream(c["id"], "search a lot [[websearch:3]] " + nonce(), web_search=True)
    assert s.is_sse
    assert_sse_grammar(s)
    assert s.error["code"] == "web_search_calls_exceeded"
    assert s.of("done") == []
    t = api.wait_turn_state(c["id"], s.request_id, {"error"})
    assert t["error_code"] == "web_search_calls_exceeded"
    assert db.turn_row(c["id"], s.request_id)["state"] == "failed"


# Acceptance: Web search — kill switch rejects before the stream opens
def test_web_search_kill_switch(ks_api_for, mock_llm):
    a = ks_api_for("tok-a")
    c = a.create_chat()
    n = nonce()
    s = a.stream(c["id"], f"search {n}", web_search=True)
    assert not s.is_sse
    assert_problem(a.as_response(s), 400, "failed_precondition", violation_subject="web_search", violation_type="FEATURE_DISABLED")
    assert mock_llm.chat_requests(contains=n) == []
    assert a.stream(c["id"], f"no search {n}").done
