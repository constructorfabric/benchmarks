"""Turn mutations black-box: retry / edit / delete of the latest turn.

``POST /v1/chats/{id}/turns/{request_id}/retry``, ``PATCH /v1/chats/{id}/turns/{request_id}``
(``{"content": ...}``) and ``DELETE /v1/chats/{id}/turns/{request_id}`` (204): the effect on the
turn, its messages, the provider request and the quota, the preconditions and the error table, the
request-id invariants, concurrency, attachment / tool carry-forward, the summary rule, the audit
events and the failures after the mutation commit.

Source: DESIGN 3.9 "Turn Mutation Rules" (rules 1 - 9, API contracts, "Summary Interaction on Turn
Mutation", "Audit Events for Turn Mutations"), 3.6 "Retry / edit variant", 3.3 error table
(``NOT_LATEST_TURN``, ``GENERATION_IN_PROGRESS``, ``turn_state`` / ``STATE``), 3.8, 4 "Idempotency
Rules", 5.7 "Unstarted retry/edit turn", 3.7 ``chat_turns`` / ``message_attachments``, the OpenAPI
operations ``mini_chat.retry_turn`` / ``mini_chat.edit_turn`` / ``mini_chat.delete_turn``.
The Rust tests (``turn_mutations.rs``, ``turn_mutation_*.rs``) pin the same rules in-process.
"""

from __future__ import annotations

import uuid
from concurrent.futures import ThreadPoolExecutor

import pytest

from . import catalog
from .mock_openai_provider import error_response, sse_text, web_search_events
from .mutation_support import (
    AUDIT_QUEUE,
    EDIT_TEXT,
    OPS,
    USAGE_QUEUE,
    MutationSeeder,
    audit_events,
    delete_turn,
    mutate_stream,
    open_mutation,
    reject_mutation,
    usage_events,
)
from .support import (
    RESOURCE_TYPES,
    TENANT_A_ID,
    TENANT_B_ID,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    USER_B_ID,
    assert_conflict,
    assert_field_violation,
    assert_precondition,
    assert_problem,
    field_violation_reasons,
    get_turn,
    open_stream,
    read_all,
    reject_message,
    send_message,
    uuid_text,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
RESPONSES = ("POST", "/v1/responses")
Q = "what is the capital of France"


@pytest.fixture()
def mseed(mini_chat_db_path):
    return MutationSeeder(mini_chat_db_path)


def new_chat(seeder, model=PREMIUM, user=USER_A_ID, tenant=TENANT_A_ID, **kwargs):
    return seeder.insert_chat(tenant=tenant, user=user, model=model, **kwargs)


def pair(seeder, chat, **kwargs):
    kwargs.setdefault("question", Q)
    kwargs.setdefault("answer", "A")
    return seeder.insert_pair(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, **kwargs)


def provider_calls(mock):
    return mock.requests_to(*RESPONSES)


def history(seeder, chat, n):
    return [pair(seeder, chat, question=f"question {i}", answer=f"answer {i}", at=i * 10)
            for i in range(1, n + 1)]


def hold_running(api, mock, chat, request_id=None):
    """A running turn: the provider pauses after its first delta."""
    request_id = request_id or str(uuid.uuid4())
    mock.script(*RESPONSES, sse_text(["held", " back"], pause_after_deltas=1))
    live = open_stream(api, chat, {"content": "hold", "request_id": request_id})
    live.expect("stream_started")
    live.next_non_ping()
    mock.wait_paused()
    return live, request_id


def release_and_finish(mock, live):
    mock.release()
    try:
        return live.drain()
    finally:
        live.close()


# --------------------------------------------------------------------------- #
# retry
# --------------------------------------------------------------------------- #


def test_a_retry_replaces_the_latest_turn_and_streams_a_new_answer(api, mseed, mock_provider):
    chat = new_chat(mseed)
    mock_provider.script(*RESPONSES, sse_text(["Paris."]))
    first = send_message(api, chat, Q)
    old = first.request_id
    mock_provider.script(*RESPONSES, sse_text(["It is ", "Paris."]))

    sent = mutate_stream(api, "retry", chat, old)

    sent.assert_well_formed()
    assert sent.status_code == 200
    assert sent.headers["content-type"].startswith("text/event-stream")
    assert sent.names_no_ping()[0] == "stream_started" and sent.terminal.event == "done"
    assert sent.started["is_new_turn"] is True
    assert sent.text() == "It is Paris."
    new = sent.request_id
    assert new != old and uuid.UUID(new).version == 4

    old_turn, new_turn = mseed.turn(chat, old), mseed.turn(chat, new)
    assert old_turn["state"] == "completed", "the old turn keeps its state"
    assert old_turn["deleted_at"] is not None
    assert uuid_text(old_turn["replaced_by_request_id"]) == new
    assert new_turn["state"] == "completed" and new_turn["deleted_at"] is None
    assert new_turn["replaced_by_request_id"] is None
    assert uuid_text(new_turn["assistant_message_id"]) == sent.message_id

    rows = mseed.ordered_messages(chat)
    assert len(rows) == 4, "soft delete: the old pair stays in storage"
    for row in rows:
        assert (row["deleted_at"] is not None) == (uuid_text(row["request_id"]) == old)
    assert mseed.live_texts(chat) == [("user", Q), ("assistant", "It is Paris.")]


def test_a_retry_resubmits_the_original_message_and_the_history_before_it(api, mseed, mock_provider):
    chat = new_chat(mseed)
    history(mseed, chat, 2)
    third = send_message(api, chat, "question 3")

    mutate_stream(api, "retry", chat, third.request_id)

    assert provider_calls(mock_provider)[-1].conversation_texts() == [
        "user:question 1", "assistant:answer 1", "user:question 2", "assistant:answer 2",
        "user:question 3"]


def test_a_failed_or_cancelled_turn_can_be_retried(api, mseed, mock_provider):
    for kwargs in ({"state": "failed", "answer": None, "error_code": "provider_error"},
                   {"state": "cancelled", "answer": "half"}):
        chat = new_chat(mseed)
        seeded = pair(mseed, chat, **kwargs)
        sent = mutate_stream(api, "retry", chat, seeded["request_id"])
        assert sent.terminal.event == "done", kwargs
        assert mseed.turn(chat, seeded["request_id"])["deleted_at"] is not None
        assert mseed.live_texts(chat)[0] == ("user", Q)
        assert all(text != "half" for _, text in mseed.live_texts(chat))


def test_a_retry_bumps_the_chat_activity_time(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    before = mseed.chat(chat)["updated_at"]
    mutate_stream(api, "retry", chat, seeded["request_id"])
    assert mseed.chat(chat)["updated_at"] > before


# --------------------------------------------------------------------------- #
# edit
# --------------------------------------------------------------------------- #


def test_an_edit_replaces_the_user_message_and_regenerates_the_answer(api, mseed, mock_provider):
    chat = new_chat(mseed)
    first = send_message(api, chat, Q)
    old = first.request_id
    mock_provider.script(*RESPONSES, sse_text(["Rome."]))

    sent = mutate_stream(api, "edit", chat, old, content="what is the capital of Italy")

    sent.assert_well_formed()
    assert sent.terminal.event == "done" and sent.text() == "Rome."
    assert provider_calls(mock_provider)[-1].conversation_texts() == [
        "user:what is the capital of Italy"]
    assert mseed.live_texts(chat) == [("user", "what is the capital of Italy"),
                                      ("assistant", "Rome.")]
    old_user = mseed.user_message_of(chat, old)
    assert old_user["content"] == Q and old_user["deleted_at"] is not None
    assert uuid_text(mseed.turn(chat, old)["replaced_by_request_id"]) == sent.request_id


def test_an_edit_stores_the_content_verbatim(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    content = "  padded\n  text \n"
    sent = mutate_stream(api, "edit", chat, seeded["request_id"], content=content)
    assert mseed.user_message_of(chat, sent.request_id)["content"] == content


@pytest.mark.parametrize("content", ["", " ", "   \n\t  \r\n"])
def test_empty_edit_content_is_400_empty_content_and_changes_nothing(api, mseed, mock_provider,
                                                                      content):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    before = mseed.snapshot(chat)
    resp = reject_mutation(api, "edit", chat, seeded["request_id"], content=content)
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content", "EMPTY_CONTENT")
    assert mseed.snapshot(chat) == before
    assert not provider_calls(mock_provider)


@pytest.mark.parametrize("raw_body", ["{}", '{"content": 5}', '{"content": null}', "[]"])
def test_an_edit_body_that_does_not_match_the_schema_is_422(api, mseed, mock_provider, raw_body):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    before = mseed.snapshot(chat)
    resp = reject_mutation(api, "edit", chat, seeded["request_id"], raw_body=raw_body)
    problem = assert_problem(resp, 422, "invalid_argument")
    assert_field_violation(problem, "body", "invalid_json_body")
    assert mseed.snapshot(chat) == before


def test_malformed_edit_json_is_400_and_a_missing_content_type_is_415(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    resp = reject_mutation(api, "edit", chat, seeded["request_id"], raw_body='{"content": ')
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "body", "json_syntax_error")
    resp = reject_mutation(api, "edit", chat, seeded["request_id"], raw_body='{"content": "x"}',
                           headers={"Content-Type": "text/plain"})
    problem = assert_problem(resp, 415, "invalid_argument")
    assert_field_violation(problem, "body", "missing_json_content_type")
    assert not provider_calls(mock_provider)


# --------------------------------------------------------------------------- #
# delete
# --------------------------------------------------------------------------- #


def test_a_delete_is_204_without_a_body_and_hides_the_turn_everywhere(api, mseed, mock_provider):
    chat = new_chat(mseed)
    first = send_message(api, chat, Q)
    rid = first.request_id
    calls = len(provider_calls(mock_provider))

    resp = delete_turn(api, chat, rid)

    assert resp.status_code == 204 and resp.content == b""
    turn = mseed.turn(chat, rid)
    assert turn["deleted_at"] is not None and turn["replaced_by_request_id"] is None
    rows = mseed.ordered_messages(chat)
    assert len(rows) == 2 and all(r["deleted_at"] is not None for r in rows)
    assert len(provider_calls(mock_provider)) == calls, "a delete never calls the provider"

    problem = assert_problem(get_turn(api, chat, rid), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["turn"]
    listed = api.get(f"/v1/chats/{chat}/messages")
    assert listed.status_code == 200 and listed.json()["items"] == []
    assert api.get(f"/v1/chats/{chat}").json()["message_count"] == 0


def test_a_deleted_turn_is_not_part_of_the_next_context(api, mseed, mock_provider):
    chat = new_chat(mseed)
    history(mseed, chat, 2)
    third = send_message(api, chat, "question 3")
    assert delete_turn(api, chat, third.request_id).status_code == 204

    send_message(api, chat, "question 4")

    assert provider_calls(mock_provider)[-1].conversation_texts() == [
        "user:question 1", "assistant:answer 1", "user:question 2", "assistant:answer 2",
        "user:question 4"]


def test_deleting_the_latest_turn_makes_the_previous_one_the_latest(api, mseed, mock_provider):
    chat = new_chat(mseed)
    turns = history(mseed, chat, 2)
    assert delete_turn(api, chat, turns[1]["request_id"]).status_code == 204
    assert_conflict(delete_turn(api, chat, turns[1]["request_id"]), "NOT_LATEST_TURN")

    sent = mutate_stream(api, "retry", chat, turns[0]["request_id"])

    assert sent.terminal.event == "done"
    assert uuid_text(mseed.turn(chat, turns[0]["request_id"])["replaced_by_request_id"]) \
        == sent.request_id


def test_a_delete_settles_nothing(quota):
    api = quota.api()
    try:
        chat = new_chat(quota.seeder())
        first = send_message(api, chat, Q)
        rows_before = [dict(r) for r in quota.db().quota_rows()]
        usage_before = quota.db().outbox_sequenced(USAGE_QUEUE)

        assert delete_turn(api, chat, first.request_id).status_code == 204

        assert [dict(r) for r in quota.db().quota_rows()] == rows_before
        assert quota.db().outbox_sequenced(USAGE_QUEUE) == usage_before
    finally:
        api.close()


# --------------------------------------------------------------------------- #
# preconditions and the error table (every operation)
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("op", OPS)
def test_a_turn_that_is_not_the_latest_is_409_not_latest_turn(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    turns = history(mseed, chat, 2)
    before = mseed.snapshot(chat)
    resp = reject_mutation(api, op, chat, turns[0]["request_id"])
    problem = assert_conflict(resp, "NOT_LATEST_TURN")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["turn"]
    assert mseed.snapshot(chat) == before
    assert not provider_calls(mock_provider)


@pytest.mark.parametrize("op", OPS)
def test_a_soft_deleted_or_replaced_turn_is_409_not_latest_turn(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    deleted = pair(mseed, chat, deleted=True)
    assert_conflict(reject_mutation(api, op, chat, deleted["request_id"]), "NOT_LATEST_TURN")

    other = new_chat(mseed)
    first = send_message(api, other, Q)
    mutate_stream(api, "retry", other, first.request_id)
    before = mseed.snapshot(other)
    assert_conflict(reject_mutation(api, op, other, first.request_id), "NOT_LATEST_TURN")
    assert mseed.snapshot(other) == before


@pytest.mark.parametrize("op", OPS)
def test_a_running_target_is_400_failed_precondition_turn_state(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    live, rid = hold_running(api, mock_provider, chat)
    before = mseed.snapshot(chat)

    resp = reject_mutation(api, op, chat, rid)

    problem = assert_problem(resp, 400, "failed_precondition")
    assert_precondition(problem, "turn_state", "STATE")
    assert mseed.snapshot(chat) == before
    events = release_and_finish(mock_provider, live)
    assert events[-1].event == "done", "the running turn goes on"


@pytest.mark.parametrize("op", OPS)
def test_a_newer_running_turn_makes_the_target_not_the_latest(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    older = pair(mseed, chat)
    live, _ = hold_running(api, mock_provider, chat)
    try:
        assert_conflict(reject_mutation(api, op, chat, older["request_id"]), "NOT_LATEST_TURN")
    finally:
        release_and_finish(mock_provider, live)


@pytest.mark.parametrize("op", OPS)
def test_the_running_check_comes_before_the_latest_turn_check(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    stuck = mseed.insert_running(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, started_secs=5,
                                 bare=False)
    pair(mseed, chat, at=20)
    problem = assert_problem(reject_mutation(api, op, chat, stuck), 400, "failed_precondition")
    assert_precondition(problem, "turn_state", "STATE")


@pytest.mark.parametrize("op", OPS)
def test_unknown_foreign_and_deleted_targets_are_404(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    other_user_chat = new_chat(mseed, user=USER_A_REVIEWER_ID)
    other_tenant_chat = new_chat(mseed, user=USER_B_ID, tenant=TENANT_B_ID)
    deleted_chat = new_chat(mseed, deleted=True)
    deleted_pair = pair(mseed, deleted_chat)
    before = mseed.snapshot(chat)

    resp = reject_mutation(api, op, chat, str(uuid.uuid4()))
    problem = assert_problem(resp, 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["turn"]
    for target in (str(uuid.uuid4()), other_user_chat, other_tenant_chat, deleted_chat):
        resp = reject_mutation(api, op, target, str(uuid.uuid4()))
        problem = assert_problem(resp, 404, "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]
    resp = reject_mutation(api, op, deleted_chat, deleted_pair["request_id"])
    assert_problem(resp, 404, "not_found")
    # somebody else's chat is invisible even with a valid turn id of one's own chat
    for user in ("a_reviewer", "b"):
        resp = reject_mutation(api, op, chat, seeded["request_id"], user=user)
        problem = assert_problem(resp, 404, "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]
    assert mseed.snapshot(chat) == before
    assert not provider_calls(mock_provider)


@pytest.mark.parametrize("op", OPS)
def test_the_turn_of_another_chat_is_404(api, mseed, mock_provider, op):
    chat, other = new_chat(mseed), new_chat(mseed)
    elsewhere = pair(mseed, other)
    pair(mseed, chat, question="here")
    problem = assert_problem(reject_mutation(api, op, chat, elsewhere["request_id"]), 404,
                             "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["turn"]


@pytest.mark.parametrize("op", OPS)
def test_a_turn_of_another_requester_is_403_permission_denied(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat, requester=USER_A_REVIEWER_ID)
    before = mseed.snapshot(chat)
    assert_problem(reject_mutation(api, op, chat, seeded["request_id"]), 403, "permission_denied")
    assert mseed.snapshot(chat) == before


@pytest.mark.parametrize("op", OPS)
def test_a_missing_token_is_401_and_a_bad_path_is_400(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    before = mseed.snapshot(chat)
    resp = reject_mutation(api, op, chat, seeded["request_id"], user=None)
    assert resp.status_code == 401
    for path_chat, path_rid in (("not-a-uuid", seeded["request_id"]), (chat, "not-a-uuid")):
        resp = reject_mutation(api, op, path_chat, path_rid)
        problem = assert_problem(resp, 400, "invalid_argument")
        assert "invalid_path_params" in field_violation_reasons(problem)
    assert mseed.snapshot(chat) == before
    assert not provider_calls(mock_provider)


# --------------------------------------------------------------------------- #
# request ids
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("op", OPS)
def test_the_old_request_id_is_a_conflict_on_messages_stream(api, mseed, mock_provider, op):
    chat = new_chat(mseed)
    old = send_message(api, chat, Q).request_id
    if op == "delete":
        assert delete_turn(api, chat, old).status_code == 204
    else:
        mutate_stream(api, op, chat, old)
    calls = len(provider_calls(mock_provider))
    before = mseed.snapshot(chat)

    resp = reject_message(api, chat, {"content": Q, "request_id": old})

    assert_conflict(resp, "request_id_conflict")
    assert len(provider_calls(mock_provider)) == calls
    assert mseed.snapshot(chat) == before


def test_the_new_request_id_is_replayed_like_any_completed_turn(api, mseed, mock_provider):
    chat = new_chat(mseed)
    first = send_message(api, chat, Q)
    mock_provider.script(*RESPONSES, sse_text(["second answer"]))
    retried = mutate_stream(api, "retry", chat, first.request_id)
    calls = len(provider_calls(mock_provider))

    replay = send_message(api, chat, "ignored", request_id=retried.request_id)

    assert replay.started["is_new_turn"] is False
    assert replay.started["message_id"] == retried.message_id
    assert replay.text() == "second answer"
    assert len(provider_calls(mock_provider)) == calls


def test_the_turn_status_and_the_message_list_follow_the_mutation(api, mseed, mock_provider):
    chat = new_chat(mseed)
    first = send_message(api, chat, Q)
    retried = mutate_stream(api, "retry", chat, first.request_id)

    assert get_turn(api, chat, first.request_id).status_code == 404
    status = get_turn(api, chat, retried.request_id)
    assert status.status_code == 200 and status.json()["state"] == "done"
    assert status.json()["assistant_message_id"] == retried.message_id
    items = api.get(f"/v1/chats/{chat}/messages").json()["items"]
    assert {m["request_id"] for m in items} == {retried.request_id}
    assert {m["role"] for m in items} == {"user", "assistant"}
    assert api.get(f"/v1/chats/{chat}").json()["message_count"] == 2


# --------------------------------------------------------------------------- #
# concurrency
# --------------------------------------------------------------------------- #

LOSS_REASONS = {"NOT_LATEST_TURN", "GENERATION_IN_PROGRESS"}


def race(api, chat, makers):
    with ThreadPoolExecutor(max_workers=len(makers)) as pool:
        futures = [pool.submit(make) for make in makers]
        lives = [f.result() for f in futures]
    streams = [live for live in lives if live.is_sse]
    losers = [live.read() for live in lives if not live.is_sse]
    return streams, losers


def test_two_concurrent_retries_yield_one_stream_and_one_409(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    rid = seeded["request_id"]
    mock_provider.script(*RESPONSES, sse_text(["slow"], pause_after_deltas=0))

    streams, losers = race(api, chat, [lambda: open_mutation(api, "retry", chat, rid)] * 2)

    assert len(streams) == 1 and len(losers) == 1, "exactly one stream"
    assert losers[0].status_code == 409, losers[0].text
    assert losers[0].json()["context"]["reason"] in LOSS_REASONS
    mock_provider.release()
    try:
        events = streams[0].drain()
    finally:
        streams[0].close()
    assert events[-1].event == "done"
    assert len(provider_calls(mock_provider)) == 1
    assert len(mseed.turns_of(chat)) == 2 and len(mseed.ordered_messages(chat)) == 4
    running = [t for t in mseed.turns_of(chat) if t["state"] == "running" and t["deleted_at"] is None]
    assert not running


def test_concurrent_edits_store_only_the_winners_content(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    rid = seeded["request_id"]
    mock_provider.script(*RESPONSES, sse_text(["slow"], pause_after_deltas=0))
    texts = ["content of the first editor", "content of the second editor"]

    streams, losers = race(api, chat, [
        (lambda t=t: open_mutation(api, "edit", chat, rid, content=t)) for t in texts])

    assert len(streams) == 1 and len(losers) == 1
    assert losers[0].json()["context"]["reason"] in LOSS_REASONS
    mock_provider.release()
    try:
        streams[0].drain()
    finally:
        streams[0].close()
    live = mseed.live_texts(chat)
    assert live[0][0] == "user" and live[0][1] in texts and len(live) == 2


def test_a_retry_racing_a_send_leaves_at_most_one_running_turn(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    mock_provider.script(*RESPONSES, sse_text(["slow"], pause_after_deltas=0))
    body = {"content": "a brand new question", "request_id": str(uuid.uuid4())}

    streams, losers = race(api, chat, [
        lambda: open_mutation(api, "retry", chat, seeded["request_id"]),
        lambda: open_stream(api, chat, body)])

    assert len(streams) == 1 and len(losers) == 1
    assert losers[0].status_code == 409
    assert losers[0].json()["context"]["reason"] in LOSS_REASONS | {"turn_already_running"}
    running = [t for t in mseed.turns_of(chat) if t["state"] == "running" and t["deleted_at"] is None]
    assert len(running) == 1
    mock_provider.release()
    try:
        assert streams[0].drain()[-1].event == "done"
    finally:
        streams[0].close()


def test_a_delete_racing_a_retry_has_one_winner(api, mseed, mock_provider):
    for _ in range(3):
        chat = new_chat(mseed)
        seeded = pair(mseed, chat)
        rid = seeded["request_id"]
        with ThreadPoolExecutor(max_workers=2) as pool:
            retry = pool.submit(open_mutation, api, "retry", chat, rid)
            delete = pool.submit(delete_turn, api, chat, rid)
            live, deleted = retry.result(), delete.result()
        if live.is_sse:
            assert live.drain()[-1].event == "done"
            live.close()
            assert_conflict(deleted, "NOT_LATEST_TURN")
        else:
            assert deleted.status_code == 204
            assert_conflict(live.read(), "NOT_LATEST_TURN")


# --------------------------------------------------------------------------- #
# the full send pipeline: carry-forward, tools, context
# --------------------------------------------------------------------------- #


def test_a_retry_copies_the_attachment_links_and_skips_deleted_attachments(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    kept = mseed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, filename="a.pdf")
    gone = mseed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, filename="b.pdf",
                                   deleted=True)
    for attachment in (kept, gone):
        mseed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=seeded["user_message"],
                              attachment=attachment)

    sent = mutate_stream(api, "retry", chat, seeded["request_id"])

    new_user = uuid_text(mseed.user_message_of(chat, sent.request_id)["id"])
    assert mseed.links_of(new_user) == [kept], "deleted attachments are silently excluded"
    assert mseed.links_of(seeded["user_message"]) == sorted([kept, gone]), "old rows stay"
    listed = api.get(f"/v1/chats/{chat}/messages").json()["items"]
    mine = next(m for m in listed if m["id"] == new_user)
    assert [a["attachment_id"] for a in mine["attachments"]] == [kept]


def test_an_edit_re_links_the_original_attachments(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    attachment = mseed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                         filename="a.pdf")
    mseed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=seeded["user_message"],
                          attachment=attachment)

    sent = mutate_stream(api, "edit", chat, seeded["request_id"], content="reworded")

    new_user = uuid_text(mseed.user_message_of(chat, sent.request_id)["id"])
    assert mseed.links_of(new_user) == [attachment]


def test_a_retry_of_a_web_search_turn_keeps_web_search_and_counts_its_own_calls(
        api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat, web_search=True)
    mock_provider.script(*RESPONSES, sse_text(
        ["found", " it"], inserts=[(1, web_search_events())]))

    sent = mutate_stream(api, "retry", chat, seeded["request_id"])

    assert provider_calls(mock_provider)[-1].tool("web_search") is not None
    assert "tool" in sent.names()
    turn = mseed.turn(chat, sent.request_id)
    assert turn["web_search_enabled"] == 1 and turn["web_search_completed_count"] == 1
    assert mseed.turn(chat, seeded["request_id"])["web_search_completed_count"] == 0


def test_a_plain_turn_stays_plain_on_retry(api, mseed, mock_provider):
    chat = new_chat(mseed)
    seeded = pair(mseed, chat)
    sent = mutate_stream(api, "retry", chat, seeded["request_id"])
    assert provider_calls(mock_provider)[-1].tool("web_search") is None
    assert mseed.turn(chat, sent.request_id)["web_search_enabled"] == 0


def test_an_edit_sends_the_history_and_the_edited_message(api, mseed, mock_provider):
    chat = new_chat(mseed)
    turns = history(mseed, chat, 3)

    mutate_stream(api, "edit", chat, turns[2]["request_id"], content="question 3, reworded")

    conversation = provider_calls(mock_provider)[-1].conversation_texts()
    assert conversation == ["user:question 1", "assistant:answer 1", "user:question 2",
                            "assistant:answer 2", "user:question 3, reworded"]


# --------------------------------------------------------------------------- #
# summary invalidation (the rows are seeded; generating a summary belongs to a later slice)
# --------------------------------------------------------------------------- #


def summarized_chat(mseed):
    chat = new_chat(mseed)
    turns = history(mseed, chat, 3)
    mseed.compress([t[k] for t in turns[:2] for k in ("user_message", "assistant_message")])
    return chat, turns


@pytest.mark.parametrize("op", OPS)
def test_a_summary_that_covers_the_mutated_turn_is_deleted(api, mseed, mock_provider, op):
    chat, turns = summarized_chat(mseed)
    mseed.insert_thread_summary(tenant=TENANT_A_ID, chat=chat,
                                frontier_message=turns[2]["assistant_message"], frontier_secs=31)

    if op == "delete":
        assert delete_turn(api, chat, turns[2]["request_id"]).status_code == 204
    else:
        assert mutate_stream(api, op, chat, turns[2]["request_id"]).terminal.event == "done"

    assert mseed.thread_summaries(chat) == []
    assert not any(mseed.compressed_flags(chat).values()), "is_compressed is cleared"


@pytest.mark.parametrize("op", OPS)
def test_a_summary_that_stops_before_the_mutated_turn_is_kept(api, mseed, mock_provider, op):
    chat, turns = summarized_chat(mseed)
    summary = mseed.insert_thread_summary(tenant=TENANT_A_ID, chat=chat,
                                          frontier_message=turns[1]["assistant_message"],
                                          frontier_secs=21)
    flags = mseed.compressed_flags(chat)

    if op == "delete":
        assert delete_turn(api, chat, turns[2]["request_id"]).status_code == 204
    else:
        assert mutate_stream(api, op, chat, turns[2]["request_id"]).terminal.event == "done"

    rows = mseed.thread_summaries(chat)
    assert [uuid_text(r["id"]) for r in rows] == [summary]
    after = mseed.compressed_flags(chat)
    assert all(after[m] == flag for m, flag in flags.items())


def test_after_the_summary_is_dropped_the_context_reads_the_uncompressed_history(
        api, mseed, mock_provider):
    chat, turns = summarized_chat(mseed)
    mseed.insert_thread_summary(tenant=TENANT_A_ID, chat=chat,
                                frontier_message=turns[2]["assistant_message"], frontier_secs=31)

    mutate_stream(api, "retry", chat, turns[2]["request_id"])

    assert provider_calls(mock_provider)[-1].conversation_texts() == [
        "user:question 1", "assistant:answer 1", "user:question 2", "assistant:answer 2",
        "user:question 3"]


def test_a_rejected_mutation_keeps_the_summary(api, mseed, mock_provider):
    chat, turns = summarized_chat(mseed)
    mseed.insert_thread_summary(tenant=TENANT_A_ID, chat=chat,
                                frontier_message=turns[2]["assistant_message"], frontier_secs=31)
    before = mseed.snapshot(chat)
    assert_conflict(reject_mutation(api, "retry", chat, turns[0]["request_id"]), "NOT_LATEST_TURN")
    assert mseed.snapshot(chat) == before and len(mseed.thread_summaries(chat)) == 1


# --------------------------------------------------------------------------- #
# quota, kill switches and the failures after the mutation commit (extra stacks)
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("op", ["retry", "edit"])
def test_an_exhausted_quota_is_429_and_the_previous_turn_stays(quota, op):
    api = quota.api()
    try:
        seeder = MutationSeeder(quota.server.db_path)
        chat = new_chat(seeder)
        seeded = pair(seeder, chat)
        seeder.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                            spent=catalog.QUOTA_LIMITS["total"]["daily"])
        before = seeder.snapshot(chat)

        resp = reject_mutation(api, op, chat, seeded["request_id"])

        problem = assert_problem(resp, 429, "resource_exhausted")
        assert problem["context"]["violations"][0]["subject"] == "tokens"
        assert seeder.snapshot(chat) == before, "the old turn is intact"
        assert not provider_calls(quota.mock)
        assert audit_events(quota.server, "turn_retry") == []
        assert audit_events(quota.server, "turn_edit") == []
    finally:
        api.close()


def test_a_delete_needs_no_quota(quota):
    api = quota.api()
    try:
        seeder = MutationSeeder(quota.server.db_path)
        chat = new_chat(seeder)
        seeded = pair(seeder, chat)
        seeder.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                            spent=catalog.QUOTA_LIMITS["total"]["daily"])
        assert delete_turn(api, chat, seeded["request_id"]).status_code == 204
    finally:
        api.close()


def test_a_mutation_settles_once_and_publishes_one_usage_event(quota):
    api = quota.api()
    try:
        seeder = MutationSeeder(quota.server.db_path)
        chat = new_chat(seeder)
        quota.mock.script(*RESPONSES, sse_text(["Hel", "lo"], input_tokens=12, output_tokens=5))
        first = send_message(api, chat, Q)
        quota.mock.script(*RESPONSES, sse_text(["Hel", "lo"], input_tokens=100, output_tokens=40))

        sent = mutate_stream(api, "retry", chat, first.request_id)

        event = wait_for(lambda: usage_events(quota.server, sent.request_id) or None,
                         "the usage event of the retried turn")
        assert len(event) == 1
        assert event[0]["terminal_state"] == "completed"
        assert event[0]["billing_outcome"] == "completed"
        assert event[0]["settlement_method"] == "actual"
        assert event[0]["actual_credits_micro"] == str(catalog.model_credits(PREMIUM, 100, 40))
        assert len(usage_events(quota.server, first.request_id)) == 1, "the old event stays single"
        db = quota.db()
        for period in ("daily", "monthly"):
            spent, reserved = db.quota_amounts(USER_A_ID, "total", period)
            assert reserved == 0
            assert spent == catalog.model_credits(PREMIUM, 12, 5) + catalog.model_credits(
                PREMIUM, 100, 40)
            assert db.quota_row(USER_A_ID, "total", period)["calls"] == 2
        assert db.outbox_sequenced(USAGE_QUEUE) >= 2
    finally:
        api.close()


def test_the_reserve_of_a_mutation_is_written_before_the_provider_is_called(quota):
    api = quota.api()
    try:
        seeder = MutationSeeder(quota.server.db_path)
        chat = new_chat(seeder)
        seeded = pair(seeder, chat)
        quota.mock.script(*RESPONSES, sse_text(["held"], pause_after_deltas=0))
        live = open_mutation(api, "retry", chat, seeded["request_id"])
        try:
            live.expect("stream_started")
            quota.mock.wait_paused()
            reserve = catalog.reserve_for(PREMIUM, Q)
            running = [t for t in seeder.turns_of(chat) if t["state"] == "running"]
            assert len(running) == 1
            assert running[0]["reserve_tokens"] == reserve["reserve_tokens"]
            assert running[0]["reserved_credits_micro"] == reserve["reserved_credits_micro"]
            assert running[0]["effective_model"] == PREMIUM
            assert seeder.turn(chat, seeded["request_id"])["deleted_at"] is not None
            for period in ("daily", "monthly"):
                assert quota.db().quota_amounts(USER_A_ID, "total", period) == (
                    0, reserve["reserved_credits_micro"])
        finally:
            quota.mock.release()
            live.drain()
            live.close()
        assert_nothing_reserved(quota)
    finally:
        api.close()


def assert_nothing_reserved(stack):
    for row in stack.db().quota_rows():
        assert row["reserved_credits_micro"] == 0, dict(row)


def test_the_web_search_kill_switch_rejects_the_retry_of_a_web_search_turn(restricted):
    api = restricted.api()
    try:
        seeder = MutationSeeder(restricted.server.db_path)
        chat = new_chat(seeder)
        seeded = pair(seeder, chat, web_search=True)
        before = seeder.snapshot(chat)

        resp = reject_mutation(api, "retry", chat, seeded["request_id"])

        problem = assert_problem(resp, 400, "failed_precondition")
        assert_precondition(problem, "web_search", "FEATURE_DISABLED")
        assert seeder.snapshot(chat) == before
        assert not provider_calls(restricted.mock)

        plain = new_chat(seeder)
        plain_pair = pair(seeder, plain)
        assert mutate_stream(api, "retry", plain, plain_pair["request_id"]).terminal.event == "done"
    finally:
        api.close()


@pytest.mark.parametrize("op", ["retry", "edit"])
def test_a_context_that_does_not_fit_fails_the_new_turn_after_the_commit(restricted, op):
    api = restricted.api()
    try:
        seeder = MutationSeeder(restricted.server.db_path)
        chat = new_chat(seeder, model=catalog.TINY_WINDOW_ID)
        seeded = pair(seeder, chat, effective_model=catalog.TINY_WINDOW_ID)

        resp = reject_mutation(api, op, chat, seeded["request_id"])

        problem = assert_problem(resp, 400, "out_of_range")
        assert "CONTEXT_BUDGET_EXCEEDED" in field_violation_reasons(problem)
        old = seeder.turn(chat, seeded["request_id"])
        assert old["deleted_at"] is not None, "the mutation committed"
        new_rid = uuid_text(old["replaced_by_request_id"])
        new = seeder.turn(chat, new_rid)
        assert new["state"] == "failed" and new["error_code"] == "context_length_exceeded"
        assert new["deleted_at"] is None
        assert not provider_calls(restricted.mock)
        for row in restricted.db().quota_rows():
            assert row["reserved_credits_micro"] == 0
        # the chat is not blocked by a running turn
        assert not [t for t in seeder.turns_of(chat) if t["state"] == "running"]
        # the mutation event was enqueued by the commit; the failed setup emits no turn_failed
        event_type = "turn_retry" if op == "retry" else "turn_edit"
        wait_for(lambda: [e for e in audit_events(restricted.server, event_type)
                          if e.get("new_request_id") == new_rid] or None,
                 f"the {event_type} audit event")
        assert [e for e in audit_events(restricted.server, "turn_failed")
                if e.get("request_id") == new_rid] == []
        assert usage_events(restricted.server, new_rid) == []
    finally:
        api.close()


# --------------------------------------------------------------------------- #
# audit events
# --------------------------------------------------------------------------- #


def test_every_mutation_emits_its_audit_event(quota):
    api = quota.api()
    try:
        seeder = MutationSeeder(quota.server.db_path)
        db = quota.db()
        chat = new_chat(seeder)
        before = db.outbox_sequenced(AUDIT_QUEUE)
        first = send_message(api, chat, Q)
        retried = mutate_stream(api, "retry", chat, first.request_id)
        edited = mutate_stream(api, "edit", chat, retried.request_id, content=EDIT_TEXT)
        assert delete_turn(api, chat, edited.request_id).status_code == 204

        def logged(event_type, key, value):
            return [e for e in audit_events(quota.server, event_type) if e.get(key) == value]

        retry = wait_for(lambda: logged("turn_retry", "new_request_id", retried.request_id) or None,
                         "the turn_retry audit event")
        assert len(retry) == 1
        assert retry[0]["original_request_id"] == first.request_id
        assert retry[0]["chat_id"] == chat and retry[0]["actor_user_id"] == USER_A_ID
        edit = wait_for(lambda: logged("turn_edit", "new_request_id", edited.request_id) or None,
                        "the turn_edit audit event")
        assert len(edit) == 1 and edit[0]["original_request_id"] == retried.request_id
        delete = wait_for(lambda: logged("turn_delete", "request_id", edited.request_id) or None,
                          "the turn_delete audit event")
        assert len(delete) == 1
        assert delete[0]["chat_id"] == chat and delete[0]["actor_user_id"] == USER_A_ID
        # 3 finalizations + retry + edit + delete
        wait_for(lambda: db.outbox_sequenced(AUDIT_QUEUE) - before == 6 or None,
                 "six audit messages on the audit queue")
        assert db.dead_letter_count() == 0
    finally:
        api.close()


def test_rejected_mutations_emit_no_audit_event(quota):
    api = quota.api()
    try:
        seeder = MutationSeeder(quota.server.db_path)
        db = quota.db()
        chat = new_chat(seeder)
        turns = history(seeder, chat, 2)
        before = db.outbox_sequenced(AUDIT_QUEUE)
        for op in OPS:
            assert_conflict(reject_mutation(api, op, chat, turns[0]["request_id"]),
                            "NOT_LATEST_TURN")
            assert reject_mutation(api, op, chat, str(uuid.uuid4())).status_code == 404
        assert db.outbox_sequenced(AUDIT_QUEUE) == before
    finally:
        api.close()
