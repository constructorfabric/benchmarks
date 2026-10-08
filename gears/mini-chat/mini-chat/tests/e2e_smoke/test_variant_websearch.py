"""Config variant `websearch`: quota.web_search_daily_quota=1 (DESIGN §4 Web Search Quota Enforcement)."""

from conftest import assert_problem

VARIANT = "websearch"


def test_web_search_daily_quota(api, chat, fresh_mock, db):
    api.turn(chat["id"], "one search [[web_search]]", web_search={"enabled": True})
    daily = [r for r in db.quota_rows() if r["period_type"] == "daily" and r["bucket"] == "total"]
    assert daily and daily[0]["web_search_calls"] == 1, daily
    calls = len(fresh_mock.responses_calls())
    res = api.stream(chat["id"], "second search", web_search={"enabled": True})
    assert res.events == []
    assert_problem(res.problem, 429, "resource_exhausted",
                   violation={"subject": "web_search", "description": "quota_exceeded"})
    assert len(fresh_mock.responses_calls()) == calls
    # A request that does not use the tool is never rejected by this quota.
    api.turn(chat["id"], "no search")
    api.turn(chat["id"], "flag off", web_search={"enabled": False})
