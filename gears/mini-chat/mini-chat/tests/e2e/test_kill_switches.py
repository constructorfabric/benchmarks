"""Kill switches from the policy snapshot (dedicated server with all switches on)."""

from __future__ import annotations

import pytest

from conftest import _fresh, assert_problem, ok_stream
from test_attachments import XLSX, png


@pytest.fixture
def ks(servers):
    srv = servers("kill")
    return srv, _fresh(srv)


def test_web_search_disabled_rejected_before_stream(ks, mock):
    srv, api = ks
    chat = api.create_chat(model="gpt-standard")
    r = api.send(chat["id"], "news", web_search={"enabled": True})
    body = assert_problem(r, 400, category="failed_precondition")
    v = body["context"]["violations"][0]
    assert v["subject"] == "web_search" and v["type"] == "FEATURE_DISABLED"
    assert mock.chat_requests(chat["id"]) == []
    ok_stream(api.send(chat["id"], "news", web_search={"enabled": False}))


def test_image_upload_disabled(ks):
    srv, api = ks
    chat = api.create_chat(model="gpt-standard")
    r = api.upload(chat["id"], png(8, 8), "i.png", "image/png")
    body = assert_problem(r, 400, category="failed_precondition")
    v = body["context"]["violations"][0]
    assert v["subject"] == "images" and v["type"] == "FEATURE_DISABLED"


def test_code_interpreter_disabled(ks):
    srv, api = ks
    chat = api.create_chat(model="gpt-standard")
    r = api.upload(chat["id"], b"PK", "s.xlsx", XLSX)
    assert_problem(r, 400, category="invalid_argument", reason="CODE_INTERPRETER_UNAVAILABLE")


def test_file_search_disabled_skips_tool(ks, mock):
    srv, api = ks
    chat = api.create_chat(model="gpt-standard")
    assert api.upload(chat["id"], b"doc", "d.txt", "text/plain").status_code == 201
    ok_stream(api.send(chat["id"], "q"))
    body = mock.chat_requests(chat["id"])[-1]["json"]
    assert "tools" not in body


def test_force_standard_tier(ks, mock):
    srv, api = ks
    chat = api.create_chat()
    assert chat["model"] == "gpt-premium"
    done = ok_stream(api.send(chat["id"], "q")).first("done")
    assert done["effective_model"] == "gpt-standard"
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_reason"] == "force_standard_tier"
    assert done["downgrade_from"] == "gpt-premium"
