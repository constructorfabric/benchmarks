"""Crash recovery: orphan watchdog and upload reaper after a server crash and restart
(DESIGN section 4 "Turn Lifecycle, Crash Recovery and Orphan Handling", B.9.1, B.9.5).

The suite config runs both workers every second (orphan timeout 90 s, upload stale
after 60 s); rows are made stale by rewriting their timestamps in the gear DB.
"""

import re
import threading
import uuid
from contextlib import closing
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
import requests

from . import mock_provider as mp
from .helpers import PREFIX, api, create_chat, db, stream, uuid_bytes, wait_until

pytestmark = pytest.mark.usefixtures("server")

PNG = Path(__file__).resolve().parents[2] / "testdata" / "images" / "tiny.png"
#: Gear timestamp text (``domain::time::db_ts``): UTC, nine fraction digits ending in 001.
GEAR_TS = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6}001Z$")


def _ago(**delta) -> str:
    """Gear timestamp text for ``now - delta`` (sorts like gear-written values)."""
    t = datetime.now(timezone.utc) - timedelta(**delta)
    return t.strftime("%Y-%m-%dT%H:%M:%S.%f") + "001Z"


def _turn(s, chat_id: str, rid: str) -> dict:
    r = s.get(f"{PREFIX}/chats/{chat_id}/turns/{rid}")
    assert r.status_code == 200, r.text
    return r.json()


def _upload_png(s, chat_id: str) -> dict:
    r = s.post(f"{PREFIX}/chats/{chat_id}/attachments", files={"file": (PNG.name, PNG.read_bytes(), "image/png")})
    assert r.status_code == 201, r.text
    assert r.json()["status"] == "ready"
    return r.json()


def _attachment_row(attachment_id: str):
    with closing(db()) as conn:
        return conn.execute("SELECT status, error_code, provider_file_id, cleanup_status, updated_at"
                            " FROM attachments WHERE id = ?", (uuid_bytes(attachment_id),)).fetchone()


def _exec(sql: str, params: tuple) -> None:
    with closing(db()) as conn:
        assert conn.execute(sql, params).rowcount == 1
        conn.commit()


def test_crash_restart_recovers_orphan_turn_and_abandoned_uploads(server_ctl, reset_mock):
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    # The provider sends some text and then neither a terminal event nor EOF.
    reset_mock.enqueue("responses", {"events": [mp.ev_created(), mp.ev_delta("partial")], "hang": True})

    def send():
        try:
            stream(api(), chat["id"], {"content": "hi", "request_id": rid})
        except requests.RequestException:
            pass  # the crash drops the SSE connection

    sender = threading.Thread(target=send)
    sender.start()
    wait_until(lambda: s.get(f"{PREFIX}/chats/{chat['id']}/turns/{rid}").status_code == 200
               and reset_mock.requests(route="responses"), message="turn running at the provider")
    assert _turn(s, chat["id"], rid)["state"] == "running"

    server_ctl.crash_and_restart()
    sender.join(timeout=30)
    assert not sender.is_alive()

    # Restart over the existing DB: models still listed, the chat is still there.
    models = s.get(f"{PREFIX}/models")
    assert models.status_code == 200, models.text
    assert "gpt-premium" in {m["model_id"] for m in models.json()["items"]}
    assert s.get(f"{PREFIX}/chats/{chat['id']}").status_code == 200

    # Nothing finalizes the crashed turn until its progress is older than the 90 s timeout.
    assert _turn(s, chat["id"], rid)["state"] == "running"
    with closing(db()) as conn:
        row = conn.execute("SELECT last_progress_at FROM chat_turns WHERE request_id = ?",
                           (uuid_bytes(rid),)).fetchone()
    assert GEAR_TS.match(row["last_progress_at"]), row["last_progress_at"]
    _exec("UPDATE chat_turns SET last_progress_at = ? WHERE request_id = ?", (_ago(minutes=2), uuid_bytes(rid)))

    status = wait_until(lambda: (t := _turn(s, chat["id"], rid))["state"] == "error" and t, timeout=5,
                        message="orphan turn finalized")
    assert status["error_code"] == "orphan_timeout"
    with closing(db()) as conn:
        turn = conn.execute("SELECT state, error_code, completed_at FROM chat_turns WHERE request_id = ?",
                            (uuid_bytes(rid),)).fetchone()
    assert (turn["state"], turn["error_code"]) == ("failed", "orphan_timeout")
    assert turn["completed_at"] is not None


def test_reaper_fails_abandoned_uploads_after_restart(server_ctl, reset_mock):
    s = api()
    chat = create_chat(s)
    pending = _upload_png(s, chat["id"])
    uploaded = _upload_png(s, chat["id"])
    file_id = _attachment_row(uploaded["id"])["provider_file_id"]
    assert file_id
    assert GEAR_TS.match(_attachment_row(pending["id"])["updated_at"])

    # The request died mid-upload: one row before the provider stored the file
    # (`pending`, no file id), one after (`uploaded`, file id recorded).
    _exec("UPDATE attachments SET status = 'pending', provider_file_id = NULL, updated_at = ? WHERE id = ?",
          (_ago(minutes=10), uuid_bytes(pending["id"])))
    _exec("UPDATE attachments SET status = 'uploaded', updated_at = ? WHERE id = ?",
          (_ago(minutes=10), uuid_bytes(uploaded["id"])))
    server_ctl.crash_and_restart()

    for a in (pending, uploaded):
        body = wait_until(lambda a=a: (b := s.get(f"{PREFIX}/chats/{chat['id']}/attachments/{a['id']}").json())
                          ["status"] == "failed" and b, timeout=5, message=f"attachment {a['id']} reaped")
        assert body["error_code"] == "upload_abandoned"
    assert _attachment_row(pending["id"])["cleanup_status"] is None

    # The uploaded row's provider file is deleted by the attachment cleanup handler.
    wait_until(lambda: [r for r in reset_mock.requests(method="DELETE", route="delete_file")
                        if r["path"].endswith(f"/files/{file_id}")], timeout=15, message="provider file delete")
    wait_until(lambda: _attachment_row(uploaded["id"])["cleanup_status"] == "done", timeout=15,
               message="cleanup done")
    # The rows are not soft-deleted: still listed with status failed.
    assert s.get(f"{PREFIX}/chats/{chat['id']}/attachments/{uploaded['id']}").status_code == 200
