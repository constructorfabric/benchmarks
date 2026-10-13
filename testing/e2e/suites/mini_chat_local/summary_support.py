"""Helpers of the S7 black-box tests (thread summary).

Kept apart from ``support.py`` / ``mutation_support.py`` / ``catalog.py`` so the earlier suites stay
untouched:

* the catalog of the summary stacks: the shared catalog plus ``sum-chat`` (a standard-tier chat
  model whose input budget is 2000 tokens: proactive threshold 1600, truncation budget 1900) and
  ``mock-summary`` (the summary model, ``max_output_tokens`` 2048) -- ``summary_overrides``;
* ``SummarySeeder`` (``MutationSeeder`` plus sized turns and the summary readers);
* prompt / request views (``entries``, ``labels``, ``summary_item``) and evidence readers
  (``system_usage``, ``summary_requests_of``).

Token arithmetic (DESIGN 5.5.4, ``catalog.estimate_tokens``): the system prompt of the e2e catalog
is 25 bytes (118 tokens), ``ping`` is 112 tokens and a 1000 byte message is 385 tokens, so the
history of two 1000-byte turns plus ``ping`` assembles ``1540 + 118 + 112 = 1770`` tokens (above
80 % of 2000, within 1900); three turns exceed the budget and are truncated.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from typing import Any, Dict, List, Optional

from . import catalog
from .mock_openai_provider import MockOpenAIProvider, RecordedRequest
from .mutation_support import MutationSeeder
from .support import (
    TENANT_A_ID,
    USER_A_ID,
    MiniChatApi,
    uuid_text,
    usage_log,
    wait_for,
)

SUMMARY_QUEUE = "mini-chat.thread_summary"
CHAT_MODEL_ID = "sum-chat"
SUMMARY_MODEL_ID = "mock-summary"
DEFAULT_SUMMARY_MODEL_ID = "gpt-4.1-mini"
SUMMARY_MAX_OUTPUT = 2048
SYSTEM_SUBJECT_ID = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
TASK_TYPE = "thread_summary_update"
CHAT_ANSWER = "Hello from the mock provider."

PREAMBLE = ("This conversation has earlier messages that have been summarized. The summary below "
            "covers the earlier portion of the conversation. Recent messages follow after.")
OPENING_FRESH = "Summarize the following conversation:"
OPENING_MERGE = ("The existing summary below covers the earlier conversation. Incorporate it with "
                 "the new messages into a single updated summary.")
CLOSING = "Respond with an <analysis> block followed by a <summary> block."
DEFAULT_PROMPT_START = "You are a conversation summarizer."


# --------------------------------------------------------------------------- #
# stacks
# --------------------------------------------------------------------------- #


def chat_entry() -> Dict[str, Any]:
    return catalog.variant(CHAT_MODEL_ID, max_input_tokens=2000)


def summary_entry(model_id: str = SUMMARY_MODEL_ID, **patch: Any) -> Dict[str, Any]:
    return catalog.variant(model_id, max_output_tokens=SUMMARY_MAX_OUTPUT, **patch)


def provider_model(model_id: str) -> str:
    """The provider-side name of a catalog model of this suite."""
    return catalog.SECRET_PROVIDER_MODEL_PREFIX + model_id


def summary_overrides(*, worker: Optional[Dict[str, Any]] = None,
                      summary_model: Optional[str] = SUMMARY_MODEL_ID,
                      extra_models: Optional[List[Dict[str, Any]]] = None) -> Dict[str, Any]:
    """Server config overrides of a summary stack: the summary catalog and the
    ``thread_summary_worker`` keys (``summary_model_id`` = ``summary_model``; ``None`` leaves the key
    out, ``""`` means the built-in default model)."""
    keys: Dict[str, Any] = dict(worker or {})
    if summary_model is not None:
        keys["summary_model_id"] = summary_model
    return {"gears": {
        "static-mini-chat-model-policy-plugin": {"config": {
            "model_catalog": catalog.catalog_entries() + [chat_entry(), summary_entry()]
            + list(extra_models or [])}},
        "mini-chat": {"config": {"thread_summary_worker": keys}},
    }}


# --------------------------------------------------------------------------- #
# seeding and readers
# --------------------------------------------------------------------------- #


def filler(label: str, size: int) -> str:
    assert len(label) <= size
    return label + "x" * (size - len(label))


class SummarySeeder(MutationSeeder):
    """``MutationSeeder`` plus sized history and the summary readers."""

    def new_chat(self, model: str = CHAT_MODEL_ID, **kwargs: Any) -> str:
        return self.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model, **kwargs)

    def history(self, chat: str, turns: int, size: int = 1000) -> List[Dict[str, Optional[str]]]:
        """``turns`` completed turns ``Q{i}xxx`` / ``A{i}xxx`` of ``size`` bytes per message at
        10 s, 20 s, ... (2025: every turn created by the API is newer)."""
        return [self.insert_pair(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 question=filler(f"Q{i}", size), answer=filler(f"A{i}", size),
                                 at=10 * i, effective_model=CHAT_MODEL_ID)
                for i in range(1, turns + 1)]

    def summary_rows(self, chat: str) -> List[Any]:
        return self.thread_summaries(chat)

    def summary_text(self, chat: str) -> Optional[str]:
        rows = self.thread_summaries(chat)
        return rows[0]["summary_text"].strip() if rows else None

    def compressed_ids(self, chat: str) -> List[str]:
        return sorted(i for i, flag in self.compressed_flags(chat).items() if flag)

    def live_ids(self, chat: str) -> List[str]:
        return [uuid_text(r["id"]) for r in self.ordered_messages(chat) if r["deleted_at"] is None]


def ids_of(turns: List[Dict[str, Optional[str]]]) -> List[str]:
    """Sorted message ids of the seeded turns."""
    return sorted(i for t in turns for i in (t["user_message"], t["assistant_message"]) if i)


def wait_summary(seed: SummarySeeder, chat: str, text: Optional[str] = None,
                 timeout: float = 20.0) -> Any:
    """Wait until ``chat`` has a summary row (with ``text`` when given); returns the row."""
    def find() -> Any:
        rows = seed.thread_summaries(chat)
        if rows and (text is None or rows[0]["summary_text"].strip() == text):
            return rows[0]
        return None
    return wait_for(find, f"the summary of {chat}" + (f" to say {text!r}" if text else ""),
                    timeout=timeout)


# --------------------------------------------------------------------------- #
# request views
# --------------------------------------------------------------------------- #


def entries(prompt: str) -> List[str]:
    """The ``User: ...`` / ``Assistant: ...`` entries of a summary prompt."""
    return [p.strip() for p in prompt.split("\n\n")
            if p.strip().startswith(("User: ", "Assistant: "))]


def labels(prompt: str) -> List[str]:
    """First two characters of the content of every entry (``Q1`` / ``A2`` ...)."""
    return [e.split(": ", 1)[1][:2] for e in entries(prompt)]


def is_summary_item(item: str) -> bool:
    return item.startswith("user:" + PREAMBLE)


def summary_item_text(item: str) -> str:
    assert is_summary_item(item), item
    return item[len("user:" + PREAMBLE):].strip()


def summary_requests_of(mock: MockOpenAIProvider, chat: str) -> List[RecordedRequest]:
    return [r for r in mock.summary_requests() if r.metadata.get("chat_id") == chat]


def last_chat_request(mock: MockOpenAIProvider) -> RecordedRequest:
    requests = mock.chat_requests()
    assert requests, "the chat provider was never called"
    return requests[-1]


def system_usage(server: Any, chat: Optional[str] = None) -> List[Dict[str, str]]:
    """The ``system_task`` usage events the static model-policy plugin logged."""
    return [e for e in usage_log(server) if e.get("billing_outcome") == "system_task"
            and (chat is None or e.get("chat_id") == chat)]


@dataclass
class Summ:
    """One summary stack as a test uses it."""

    stack: Any
    api: MiniChatApi
    seed: SummarySeeder

    @property
    def mock(self) -> MockOpenAIProvider:
        return self.stack.mock

    @property
    def server(self) -> Any:
        return self.stack.server

    def db(self) -> Any:
        return self.stack.db()

    def queued(self) -> int:
        """Thread-summary messages ever enqueued."""
        return self.db().outbox_sequenced(SUMMARY_QUEUE)

    def dead_letters(self) -> int:
        return self.db().dead_letter_count()

    def new_chat(self, **kwargs: Any) -> str:
        return self.seed.new_chat(**kwargs)

    def uuid(self) -> str:
        return str(uuid.uuid4())
