"""Helpers of the S5 black-box tests (turn mutations and the orphan watchdog).

Kept apart from ``support.py`` (S1 - S4) so the earlier suites stay untouched:

* ``open_mutation`` / ``mutate_stream`` / ``delete_turn`` call ``POST .../retry``,
  ``PATCH .../turns/{request_id}`` and ``DELETE .../turns/{request_id}``;
* ``MutationSeeder`` extends ``GearSeeder`` with the rows only these tests need: whole turns (user
  message, assistant message, ``chat_turns`` row), thread summaries, the ``is_compressed`` flag,
  soft-deleted attachments and the liveness timestamps of a running turn;
* ``audit_events`` / ``usage_events`` read the static plugins' log lines of one server.

Mutation audit events are logged by the bundled static audit plugin with their identities
(``S5-test-interface.md`` section 5): ``turn_retry`` / ``turn_edit`` log ``chat_id``,
``actor_user_id``, ``original_request_id`` and ``new_request_id``; ``turn_delete`` logs ``chat_id``,
``actor_user_id`` and ``request_id``.
"""

from __future__ import annotations

import uuid
from typing import Any, Dict, List, Optional

import httpx

from .support import (
    GearSeeder,
    LiveStream,
    MiniChatApi,
    Sent,
    audit_log,
    bearer,
    read_all,
    stored_ts,
    uuid_blob,
    uuid_text,
    usage_log,
)

USAGE_QUEUE = "mini-chat.usage_snapshot"
AUDIT_QUEUE = "mini-chat.audit"
OPS = ("retry", "edit", "delete")
EDIT_TEXT = "replacement text"


# --------------------------------------------------------------------------- #
# the three endpoints
# --------------------------------------------------------------------------- #


def turn_path(api: MiniChatApi, chat_id: str, request_id: str, suffix: str = "") -> str:
    return api.url(f"/v1/chats/{chat_id}/turns/{request_id}{suffix}")


def open_mutation(api: MiniChatApi, op: str, chat_id: str, request_id: str, *,
                  content: Optional[str] = None, user: Optional[str] = "a",
                  raw_body: Optional[str] = None, path: Optional[str] = None,
                  headers: Optional[Dict[str, str]] = None, timeout: float = 60.0) -> LiveStream:
    """``retry`` / ``edit`` (an SSE answer or a JSON problem) read incrementally; the response
    head is available as soon as the setup finished. ``edit`` sends ``{"content": content}``
    unless ``raw_body`` (sent as is, with a JSON content type unless ``headers`` says otherwise)
    is given."""
    assert op in ("retry", "edit"), op
    merged: Dict[str, str] = {}
    if user is not None:
        merged.update(bearer(user))
    kwargs: Dict[str, Any] = {}
    if op == "retry":
        method, target = "POST", path or turn_path(api, chat_id, request_id, "/retry")
    else:
        method, target = "PATCH", path or turn_path(api, chat_id, request_id)
        if raw_body is not None:
            kwargs["content"] = raw_body
            merged["Content-Type"] = "application/json"
        else:
            kwargs["json"] = {"content": content if content is not None else EDIT_TEXT}
    merged.update(headers or {})
    cm = api._client.stream(  # noqa: SLF001 - the suite's own client
        method, target, headers=merged, timeout=httpx.Timeout(timeout, connect=5.0), **kwargs)
    response = cm.__enter__()
    return LiveStream(cm, response)


def mutate_stream(api: MiniChatApi, op: str, chat_id: str, request_id: str, **kwargs: Any) -> Sent:
    """``retry`` / ``edit`` and the whole SSE answer."""
    return read_all(open_mutation(api, op, chat_id, request_id, **kwargs))


def delete_turn(api: MiniChatApi, chat_id: str, request_id: str, *,
                user: Optional[str] = "a", path: Optional[str] = None) -> httpx.Response:
    if path is not None:
        return api.request("DELETE", path, user=user, raw_path=True)
    return api.delete(f"/v1/chats/{chat_id}/turns/{request_id}", user=user)


def reject_mutation(api: MiniChatApi, op: str, chat_id: str, request_id: str, **kwargs: Any
                    ) -> httpx.Response:
    """Run ``op`` and expect a JSON answer (a problem, or ``204`` for a delete)."""
    if op == "delete":
        return delete_turn(api, chat_id, request_id, **kwargs)
    live = open_mutation(api, op, chat_id, request_id, **kwargs)
    assert not live.is_sse, f"{op}: expected a JSON rejection, got an SSE stream"
    return live.read()


# --------------------------------------------------------------------------- #
# plugin evidence
# --------------------------------------------------------------------------- #


def audit_events(server: Any, event_type: Optional[str] = None) -> List[Dict[str, str]]:
    """Audit events the static audit plugin logged (optionally of one ``event_type``)."""
    return [e for e in audit_log(server) if event_type is None or e.get("event_type") == event_type]


def usage_events(server: Any, request_id: Optional[str] = None) -> List[Dict[str, str]]:
    """Usage events the static model-policy plugin logged (optionally of one ``request_id``)."""
    return [e for e in usage_log(server) if request_id is None or e.get("request_id") == request_id]


# --------------------------------------------------------------------------- #
# seeding
# --------------------------------------------------------------------------- #


class MutationSeeder(GearSeeder):
    """``GearSeeder`` plus whole-turn seeding and the S5 readers."""

    def insert_pair(self, *, tenant: str, chat: str, user: str, question: str = "hello",
                    answer: Optional[str] = "an answer", state: str = "completed",
                    error_code: Optional[str] = None, at: int = 10,
                    request_id: Optional[str] = None, effective_model: str = "mock-premium",
                    web_search: bool = False, requester: Optional[str] = None,
                    deleted: bool = False) -> Dict[str, Optional[str]]:
        """One turn the way the API leaves it: the user message at ``at`` s, the assistant message
        (when ``answer`` is not ``None``) one second later, and the ``chat_turns`` row of
        ``state`` (``completed`` / ``failed`` / ``cancelled``), all in 2025 so any turn created by
        the API is newer."""
        request_id = request_id or str(uuid.uuid4())
        user_message = self.insert_message(
            tenant=tenant, chat=chat, role="user", content=question, request_id=request_id,
            created=at, deleted=deleted)
        assistant_message = None
        if answer is not None:
            assistant_message = self.insert_message(
                tenant=tenant, chat=chat, role="assistant", content=answer,
                request_id=request_id, created=at + 1, model=effective_model, deleted=deleted)
        turn_id = self.insert_turn(
            tenant=tenant, chat=chat, request_id=request_id, state=state, user=requester or user,
            error_code=error_code, assistant_message=assistant_message, deleted=deleted,
            effective_model=effective_model, started=at, updated=at + 2, updated_micros=0)
        if web_search:
            self._write("UPDATE chat_turns SET web_search_enabled = 1 WHERE id = ?",
                        (uuid_blob(turn_id),))
        return {"request_id": request_id, "turn_id": turn_id, "user_message": user_message,
                "assistant_message": assistant_message}

    def insert_running(self, *, tenant: str, chat: str, user: str,
                       started_secs: int = 10, bare: bool = True,
                       request_id: Optional[str] = None) -> str:
        """A ``running`` turn dating from 2025 (stale for any timeout); ``bare`` clears the
        preflight columns like a retry/edit turn left running before its reserve was taken."""
        request_id = request_id or str(uuid.uuid4())
        self.insert_turn(tenant=tenant, chat=chat, request_id=request_id, state="running",
                         user=user, started=started_secs)
        if bare:
            self._write("UPDATE chat_turns SET effective_model = NULL WHERE request_id = ?",
                        (uuid_blob(request_id),))
        return request_id

    def set_liveness(self, chat: str, request_id: str, *, started: str,
                     last_progress: Optional[str]) -> None:
        """Rewrite ``started_at`` / ``last_progress_at`` of a turn (stored timestamp texts)."""
        self._write(
            "UPDATE chat_turns SET started_at = ?, last_progress_at = ? "
            "WHERE chat_id = ? AND request_id = ?",
            (started, last_progress, uuid_blob(chat), uuid_blob(request_id)))

    def set_progress(self, chat: str, request_id: str, last_progress: Optional[str]) -> None:
        """Rewrite only ``last_progress_at`` of a turn (what an hour without progress looks like;
        ``started_at`` stays, so the quota periods the watchdog derives from it stay too)."""
        self._write(
            "UPDATE chat_turns SET last_progress_at = ? WHERE chat_id = ? AND request_id = ?",
            (last_progress, uuid_blob(chat), uuid_blob(request_id)))

    def soft_delete_attachment(self, attachment: str) -> None:
        self._write("UPDATE attachments SET deleted_at = ? WHERE id = ?",
                    (stored_ts(1), uuid_blob(attachment)))

    def insert_thread_summary(self, *, tenant: str, chat: str, frontier_message: str,
                              frontier_secs: int, frontier_micros: int = 0) -> str:
        summary_id = str(uuid.uuid4())
        self._write(
            "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, "
            "summarized_up_to_created_at, summarized_up_to_message_id, token_estimate, "
            "created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 12, ?, ?)",
            (uuid_blob(summary_id), uuid_blob(tenant), uuid_blob(chat),
             "a summary of the earlier conversation", stored_ts(frontier_secs, frontier_micros),
             uuid_blob(frontier_message), stored_ts(0), stored_ts(0)))
        return summary_id

    def compress(self, message_ids: List[str]) -> None:
        for message_id in message_ids:
            self._write("UPDATE messages SET is_compressed = 1 WHERE id = ?",
                        (uuid_blob(message_id),))

    # ---- readers -------------------------------------------------------------- #

    def thread_summaries(self, chat: str) -> List[Any]:
        return self.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (uuid_blob(chat),))

    def compressed_flags(self, chat: str) -> Dict[str, int]:
        return {uuid_text(r["id"]): r["is_compressed"] for r in self.messages_of(chat)}

    def links_of(self, message: str) -> List[str]:
        rows = self.query("SELECT attachment_id FROM message_attachments WHERE message_id = ?",
                          (uuid_blob(message),))
        return sorted(uuid_text(r["attachment_id"]) for r in rows)

    def live_texts(self, chat: str) -> List["tuple[str, str]"]:
        """``(role, content)`` of the live messages of ``chat``, chronologically."""
        return [(r["role"], r["content"]) for r in self.ordered_messages(chat)
                if r["deleted_at"] is None]

    def user_message_of(self, chat: str, request_id: str) -> Any:
        rows = [r for r in self.ordered_messages(chat)
                if r["role"] == "user" and r["request_id"] is not None
                and uuid_text(r["request_id"]) == request_id]
        assert len(rows) == 1, f"one user message of {request_id}: {rows}"
        return rows[0]

    def turn_ids(self, chat: str) -> List[str]:
        return [uuid_text(r["request_id"]) for r in self.turns_of(chat)]

    def snapshot(self, chat: str) -> Dict[str, Any]:
        """Everything a rejected mutation must leave unchanged (for equality checks)."""
        return {
            "turns": [(uuid_text(r["request_id"]), r["state"], r["error_code"], r["deleted_at"],
                       uuid_text(r["replaced_by_request_id"])) for r in self.turns_of(chat)],
            "messages": [(uuid_text(r["id"]), r["deleted_at"], r["is_compressed"])
                         for r in self.ordered_messages(chat)],
            "links": self.counts(chat)["links"],
            "summaries": len(self.thread_summaries(chat)),
            "quota": [(r["bucket"], r["period_type"], r["spent_credits_micro"],
                       r["reserved_credits_micro"], r["calls"]) for r in self.quota_rows()],
        }
