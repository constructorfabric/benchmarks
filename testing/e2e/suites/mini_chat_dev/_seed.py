"""Direct DB seeding for rows the API cannot create yet (messages, turns).

Timestamps use the gear's storage format (UTC RFC 3339, nine fraction digits
ending in 1, see ``domain::time::db_ts``) so they sort like gear-written rows.
"""

from __future__ import annotations

import uuid
from contextlib import closing

from .helpers import db, uuid_bytes


def ts(second: int = 0, micros: int = 0) -> str:
    return f"2026-10-04T12:00:{second:02d}.{micros:06d}001Z"


def chat_tenant(chat_id: str) -> bytes:
    with closing(db()) as conn:
        row = conn.execute("SELECT tenant_id FROM chats WHERE id = ?", (uuid_bytes(chat_id),)).fetchone()
    assert row is not None, f"chat {chat_id} not in DB"
    return row["tenant_id"]


def insert_message(chat_id: str, role: str, created_at: str, content: str | None = None,
                   request_id: str | None = None, input_tokens: int = 0, output_tokens: int = 0,
                   model: str | None = None) -> str:
    msg_id = str(uuid.uuid4())
    with closing(db()) as conn:
        conn.execute(
            "INSERT INTO messages (id, tenant_id, chat_id, request_id, role, content, input_tokens,"
            " output_tokens, model, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (uuid_bytes(msg_id), chat_tenant(chat_id), uuid_bytes(chat_id),
             uuid_bytes(request_id or str(uuid.uuid4())), role, content or f"{role} text",
             input_tokens, output_tokens, model, created_at),
        )
        conn.commit()
    return msg_id


def insert_turn(chat_id: str, request_id: str, state: str, *, error_code: str | None = None,
                assistant_message_id: str | None = None, deleted: bool = False) -> None:
    with closing(db()) as conn:
        conn.execute(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state,"
            " error_code, assistant_message_id, deleted_at, started_at, updated_at)"
            " VALUES (?, ?, ?, ?, 'user', ?, ?, ?, ?, ?, ?)",
            (uuid_bytes(str(uuid.uuid4())), chat_tenant(chat_id), uuid_bytes(chat_id),
             uuid_bytes(request_id), state, error_code,
             uuid_bytes(assistant_message_id) if assistant_message_id else None,
             ts(30) if deleted else None, ts(1), ts(2)),
        )
        conn.commit()
