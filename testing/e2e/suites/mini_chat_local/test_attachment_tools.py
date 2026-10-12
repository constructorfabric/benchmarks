"""Attachments reach the model, black-box: the ``file_search`` / ``code_interpreter`` tools and the
``input_image`` items of the Responses request the mock provider recorded, file citations, the
image guards (``VISION_NOT_SUPPORTED``, ``TOO_MANY_IMAGES``, ``disable_images``), the code
interpreter daily quota (429 ``code_interpreter``) and the carry-forward of retry / edit.

Shared server unless a test names a stack: ``attachments_small`` (two images per message, a daily
code interpreter quota of three calls, two calls per message), ``attachments_killed`` (kill
switches on) and the quota stack with ``force_standard_tier`` (``force_standard``).

Source: DESIGN "File Search Tool Availability", "Retrieval Invariants", "Code Interpreter Tool
Availability" / "Configuration" / "Quota Enforcement", "File Search Retrieval Scope", "Citation
File ID and Title Resolution", "Internal: Multimodal Input Format (Responses API)", 3.6 context
assembly step 8, "Model Capability Constraint (Images)", "Images after a downgrade", 2.2
kill switches, 5.5.5 / 5.5.6 (surcharges), 3.6 "Retry / edit variant", REST error table.
"""

from __future__ import annotations

import re
from types import SimpleNamespace

import pytest

from . import catalog
from .attachment_support import (
    CODE_INTERPRETER_DAILY_QUOTA,
    SMALL_MAX_IMAGES,
    AttachmentSeeder,
    all_input_images,
    assert_exhausted,
    input_image_ids,
    last_responses_request,
    responses_requests,
    tool_set,
    upload_document,
    upload_image,
    upload_spreadsheet,
    user_content,
)
from .mock_openai_provider import (
    code_interpreter_events,
    file_annotation,
    file_search_events,
    sse_text,
)
from .mutation_support import mutate_stream, reject_mutation
from .support import (
    TENANT_A_ID,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    MiniChatApi,
    assert_field_violation,
    assert_precondition,
    assert_problem,
    reject_message,
    send_message,
    uuid_text,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
PROVIDER_FILE = re.compile(r"file[-_][A-Za-z0-9]{6,}")


def new_chat(attach, model=PREMIUM):
    return attach.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model)


def seeded_document(attach, chat, *, vector_store="vs_e2e_seeded_tools", status="ready",
                    deleted=False, model_chat_store=True):
    row = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, filename="seeded.pdf",
                            status=status, deleted=deleted)
    if model_chat_store and not attach.stores_of(chat):
        attach.insert_vector_store(tenant=TENANT_A_ID, chat=chat, vector_store_id=vector_store)
    return row


def file_ids_of(tool):
    return set(tool["container"]["file_ids"])


@pytest.fixture()
def sm(small):
    api = MiniChatApi(small.server.base_url)
    try:
        yield SimpleNamespace(api=api, attach=AttachmentSeeder(small.server.db_path),
                              mock=small.mock, server=small.server)
    finally:
        api.close()


@pytest.fixture()
def kl(killed):
    api = MiniChatApi(killed.server.base_url)
    try:
        yield SimpleNamespace(api=api, attach=AttachmentSeeder(killed.server.db_path),
                              mock=killed.mock, server=killed.server)
    finally:
        api.close()


@pytest.fixture()
def fs(force_standard):
    api = MiniChatApi(force_standard.server.base_url)
    try:
        yield SimpleNamespace(api=api, attach=AttachmentSeeder(force_standard.server.db_path),
                              mock=force_standard.mock, server=force_standard.server)
    finally:
        api.close()


# --------------------------------------------------------------------------- #
# file_search
# --------------------------------------------------------------------------- #


def test_a_ready_document_adds_file_search_on_the_chat_vector_store(api, attach, mock_provider):
    chat = new_chat(attach)
    upload_document(api, chat, "contract.pdf")
    store = attach.vector_store_of(chat)

    sent = send_message(api, chat, "what does the contract say?")
    assert sent.terminal.event == "done"
    request = last_responses_request(mock_provider)
    assert request.tool("file_search") == {
        "type": "file_search", "vector_store_ids": [store], "max_num_results": 5}
    assert tool_set(request) == {"file_search"}
    assert "file_search" in request.system_text()
    assert request.json["metadata"]["feature"] == "file_search"
    assert store not in sent.blob(), "the vector store id never reaches the client"


def test_file_search_is_on_every_request_of_the_chat(api, attach, mock_provider):
    chat = new_chat(attach)
    doc = upload_document(api, chat, "a.pdf")
    send_message(api, chat, "first, with the file", attachment_ids=[doc])
    assert last_responses_request(mock_provider).tool("file_search")
    send_message(api, chat, "second, without")
    assert last_responses_request(mock_provider).tool("file_search"), \
        "retrieval covers the whole chat store"


def test_without_a_ready_document_there_is_no_file_search(api, attach, mock_provider):
    chat = new_chat(attach)
    send_message(api, chat, "hello")
    request = last_responses_request(mock_provider)
    assert request.tools() == [] and "include" not in request.json
    assert request.json["metadata"]["feature"] == "none"


@pytest.mark.parametrize("variant", ["pending", "uploaded", "failed", "deleted"])
def test_documents_that_are_not_ready_or_deleted_do_not_count(api, attach, mock_provider, variant):
    chat = new_chat(attach)
    seeded_document(attach, chat, status="ready" if variant == "deleted" else variant,
                    deleted=variant == "deleted")
    send_message(api, chat, "hello")
    assert last_responses_request(mock_provider).tool("file_search") is None


def test_a_document_without_a_vector_store_row_gets_no_file_search(api, attach, mock_provider):
    chat = new_chat(attach)
    attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID)
    send_message(api, chat, "hello")
    assert last_responses_request(mock_provider).tool("file_search") is None


def test_spreadsheets_and_images_are_not_searchable(api, attach, mock_provider):
    chat = new_chat(attach)
    upload_spreadsheet(api, chat)
    upload_image(api, chat)
    send_message(api, chat, "hello")
    assert last_responses_request(mock_provider).tool("file_search") is None
    assert attach.stores_of(chat) == []


def test_a_model_with_file_search_support_but_no_vision_still_searches(api, attach, mock_provider):
    chat = new_chat(attach, STANDARD)
    seeded_document(attach, chat)
    send_message(api, chat, "hello")
    assert last_responses_request(mock_provider).tool("file_search")["vector_store_ids"] == \
        ["vs_e2e_seeded_tools"]


def test_the_file_search_kill_switch_removes_the_tool(kl):
    chat = new_chat(kl.attach)
    seeded_document(kl.attach, chat)
    assert send_message(kl.api, chat, "hello").terminal.event == "done"
    request = last_responses_request(kl.mock)
    assert request.tool("file_search") is None
    assert "file_search" not in request.system_text()


def test_retrieval_is_scoped_to_the_chat_vector_store(api, attach, mock_provider):
    first, second = new_chat(attach), new_chat(attach)
    upload_document(api, first, "a.pdf")
    upload_document(api, second, "b.pdf")
    store_a, store_b = attach.vector_store_of(first), attach.vector_store_of(second)
    send_message(api, first, "q")
    request = last_responses_request(mock_provider)
    assert request.tool("file_search")["vector_store_ids"] == [store_a]
    assert store_b not in str(request.json)
    send_message(api, second, "q")
    assert last_responses_request(mock_provider).tool("file_search")["vector_store_ids"] == [store_b]


def test_a_document_reference_adds_no_input_item_and_never_exposes_its_provider_id(
        api, attach, mock_provider):
    chat = new_chat(attach)
    doc = upload_document(api, chat, "a.pdf")
    provider_file = attach.provider_file_of(doc)
    sent = send_message(api, chat, "summarise", attachment_ids=[doc])
    assert sent.terminal.event == "done"
    request = last_responses_request(mock_provider)
    assert user_content(request) == [{"type": "input_text", "text": "summarise"}]
    assert provider_file not in str(request.json)
    assert not PROVIDER_FILE.search(sent.blob()), sent.blob()


def test_deleting_the_only_document_removes_the_tool_at_once(api, attach, mock_provider):
    from .attachment_support import delete_attachment

    chat = new_chat(attach)
    doc = upload_document(api, chat, "a.pdf")
    send_message(api, chat, "q1")
    assert last_responses_request(mock_provider).tool("file_search")
    assert delete_attachment(api, chat, doc).status_code == 204
    send_message(api, chat, "q2")
    assert last_responses_request(mock_provider).tool("file_search") is None


def test_the_tool_surcharge_is_reserved_only_when_the_tool_is_sent(api, attach, mock_provider):
    plain_chat, doc_chat = new_chat(attach), new_chat(attach)
    plain = send_message(api, plain_chat, "hello")
    upload_document(api, doc_chat, "a.pdf")
    with_doc = send_message(api, doc_chat, "hello")
    plain_turn = attach.turn(plain_chat, plain.request_id)
    turn = attach.turn(doc_chat, with_doc.request_id)
    assert plain_turn["reserve_tokens"] == catalog.reserve_for(PREMIUM, "hello")["reserve_tokens"]
    assert turn["reserve_tokens"] == catalog.reserve_for(
        PREMIUM, "hello", surcharge_tokens=500)["reserve_tokens"]


# --------------------------------------------------------------------------- #
# citations
# --------------------------------------------------------------------------- #


def test_a_file_annotation_of_an_uploaded_document_is_cited_by_attachment_id_and_filename(
        api, attach, mock_provider):
    chat = new_chat(attach)
    doc = upload_document(api, chat, "Q3 Report.pdf")
    provider_file = attach.provider_file_of(doc)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["It says ", "hello."], inserts=[(0, file_search_events())],
        annotations=[file_annotation(provider_file, "provider-side-name.pdf")]))

    sent = send_message(api, chat, "what is in it?")
    sent.assert_well_formed()
    citations = sent.of("citations")
    assert len(citations) == 1
    assert citations[0].data == {"items": [{
        "source": "file", "title": "Q3 Report.pdf", "attachment_id": doc, "snippet": ""}]}
    assert provider_file not in sent.blob()
    names = [e.data["name"] for e in sent.of("tool")]
    assert "file_search" in names


def test_citations_of_deleted_other_chat_and_spreadsheet_files_are_omitted(api, attach,
                                                                           mock_provider):
    from .attachment_support import delete_attachment

    chat, other = new_chat(attach), new_chat(attach)
    kept = upload_document(api, chat, "kept.pdf")
    dropped = upload_document(api, chat, "dropped.pdf")
    foreign = upload_document(api, other, "foreign.pdf")
    sheet = upload_spreadsheet(api, chat)
    files = {name: attach.provider_file_of(i) for name, i in
             (("kept", kept), ("dropped", dropped), ("foreign", foreign), ("sheet", sheet))}
    assert delete_attachment(api, chat, dropped).status_code == 204
    mock_provider.script("POST", "/v1/responses", sse_text(["answer"], annotations=[
        file_annotation(files["dropped"], "dropped.pdf"),
        file_annotation(files["foreign"], "foreign.pdf"),
        file_annotation("file-unknownunknown0000", "x.pdf"),
        file_annotation(files["kept"], "kept.pdf"),
        file_annotation(files["sheet"], "sheet.xlsx")]))
    sent = send_message(api, chat, "q")
    cited = [i["attachment_id"] for i in sent.of("citations")[0].data["items"]]
    assert kept in cited and dropped not in cited and foreign not in cited
    for provider_file in files.values():
        assert provider_file not in sent.blob()


# --------------------------------------------------------------------------- #
# code_interpreter
# --------------------------------------------------------------------------- #


def test_a_ready_spreadsheet_adds_the_code_interpreter_tool_with_its_file_ids(api, attach,
                                                                              mock_provider):
    chat = new_chat(attach)
    sheet = upload_spreadsheet(api, chat)
    provider_file = attach.provider_file_of(sheet)

    assert send_message(api, chat, "sum column B").terminal.event == "done"
    request = last_responses_request(mock_provider)
    assert request.tool("code_interpreter") == {
        "type": "code_interpreter", "container": {"type": "auto", "file_ids": [provider_file]}}
    assert request.json["include"] == ["code_interpreter_call.outputs"]
    assert tool_set(request) == {"code_interpreter"}


def test_the_tool_is_chat_wide_and_lists_every_live_ready_spreadsheet_of_the_chat(
        api, attach, mock_provider):
    chat, other = new_chat(attach), new_chat(attach)
    one, two = upload_spreadsheet(api, chat, "one.xlsx"), upload_spreadsheet(api, chat, "two.xlsx")
    foreign = upload_spreadsheet(api, other, "foreign.xlsx")
    excluded = [
        attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, for_code_interpreter=True,
                          status="pending"),
        attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, for_code_interpreter=True,
                          status="failed"),
        attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, for_code_interpreter=True,
                          deleted=True),
        foreign]
    send_message(api, chat, "analyse")  # the message does not reference them
    tool = last_responses_request(mock_provider).tool("code_interpreter")
    assert file_ids_of(tool) == {attach.provider_file_of(one), attach.provider_file_of(two)}
    for attachment in excluded:
        assert attach.provider_file_of(attachment) not in str(tool)


def test_without_a_ready_spreadsheet_there_is_no_code_interpreter_and_no_include(
        api, attach, mock_provider):
    chat = new_chat(attach)
    attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, for_code_interpreter=True,
                      status="uploaded")
    upload_document(api, chat, "doc.pdf")
    send_message(api, chat, "hello")
    request = last_responses_request(mock_provider)
    assert request.tool("code_interpreter") is None and "include" not in request.json


def test_the_code_interpreter_kill_switch_silently_omits_the_tool(kl):
    chat = new_chat(kl.attach)
    kl.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, for_code_interpreter=True)
    assert send_message(kl.api, chat, "analyse").terminal.event == "done"
    request = last_responses_request(kl.mock)
    assert request.tool("code_interpreter") is None and "include" not in request.json


def test_a_model_without_code_interpreter_gets_no_tool(api, attach, mock_provider):
    chat = new_chat(attach, STANDARD)  # mock-standard: file search only
    attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, for_code_interpreter=True)
    assert send_message(api, chat, "analyse").terminal.event == "done"
    assert last_responses_request(mock_provider).tool("code_interpreter") is None


def test_the_code_interpreter_surcharge_is_reserved_only_when_the_tool_is_sent(
        api, attach, mock_provider):
    plain_chat, sheet_chat = new_chat(attach), new_chat(attach)
    upload_spreadsheet(api, sheet_chat)
    plain = send_message(api, plain_chat, "hello")
    with_tool = send_message(api, sheet_chat, "hello")
    assert attach.turn(plain_chat, plain.request_id)["reserve_tokens"] == \
        catalog.reserve_for(PREMIUM, "hello")["reserve_tokens"]
    assert attach.turn(sheet_chat, with_tool.request_id)["reserve_tokens"] == \
        catalog.reserve_for(PREMIUM, "hello", surcharge_tokens=1000)["reserve_tokens"]


def test_documents_spreadsheets_and_web_search_combine(api, attach, mock_provider):
    chat = new_chat(attach)
    upload_document(api, chat, "doc.pdf")
    upload_spreadsheet(api, chat)
    sent = send_message(api, chat, "everything", web_search={"enabled": True})
    assert sent.terminal.event == "done"
    request = last_responses_request(mock_provider)
    assert tool_set(request) == {"file_search", "code_interpreter", "web_search"}
    assert request.json["include"] == ["code_interpreter_call.outputs"]
    turn = attach.turn(chat, sent.request_id)
    assert turn["reserve_tokens"] == catalog.reserve_for(
        PREMIUM, "everything", surcharge_tokens=2000)["reserve_tokens"]  # 500 + 500 + 1000


# --------------------------------------------------------------------------- #
# the code interpreter daily quota (deferred S4 F-002) and the per-message limit
# --------------------------------------------------------------------------- #


def calls_today(attach, user=USER_A_ID):
    row = attach.quota_row(user, "total", "daily")
    return 0 if row is None else row["code_interpreter_calls"]


def test_an_exhausted_daily_quota_is_429_code_interpreter_before_anything_happens(sm):
    chat = new_chat(sm.attach)
    upload_spreadsheet(sm.api, chat)
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                           code_interpreter_calls=CODE_INTERPRETER_DAILY_QUOTA)

    before = sm.attach.counts(chat)
    quota_before = [dict(r) for r in sm.attach.quota_rows()]
    resp = reject_message(sm.api, chat, {"content": "analyse the sheet"})
    problem = assert_exhausted(resp, "code_interpreter")
    assert problem["context"]["violations"][0]["description"] == "quota_exceeded"
    assert sm.attach.counts(chat) == before
    assert [dict(r) for r in sm.attach.quota_rows()] == quota_before
    assert not responses_requests(sm.mock), "no provider call"


def test_the_limit_is_reached_at_the_quota_not_above_it(sm):
    chat = new_chat(sm.attach)
    upload_spreadsheet(sm.api, chat)
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                           code_interpreter_calls=CODE_INTERPRETER_DAILY_QUOTA - 1)
    assert send_message(sm.api, chat, "one call below the quota").terminal.event == "done"
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                           code_interpreter_calls=1)
    assert calls_today(sm.attach) == CODE_INTERPRETER_DAILY_QUOTA
    assert_exhausted(reject_message(sm.api, chat, {"content": "at the quota"}), "code_interpreter")


def test_the_check_only_applies_when_the_tool_is_included(sm):
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                           code_interpreter_calls=100)
    chat = new_chat(sm.attach)
    for status, deleted in (("uploaded", False), ("failed", False), ("ready", True)):
        # no provider file: the upload reaper of this stack takes the stale `uploaded` row quietly
        sm.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                             for_code_interpreter=True, status=status, deleted=deleted,
                             provider_file_id="file-seed-code-interpreter" if status == "ready"
                             else None)
    upload_document(sm.api, chat, "d.pdf")
    other = new_chat(sm.attach)
    upload_spreadsheet(sm.api, other)  # a spreadsheet of another chat does not include the tool here
    sent = send_message(sm.api, chat, "no tool, no quota check")
    assert sent.terminal.event == "done"
    assert last_responses_request(sm.mock).tool("code_interpreter") is None


def test_only_the_users_daily_total_bucket_of_today_counts(sm):
    chat = new_chat(sm.attach)
    upload_spreadsheet(sm.api, chat)
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, bucket="total",
                           period="daily", code_interpreter_calls=50)
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                           code_interpreter_calls=50, stale=True)
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="monthly",
                           code_interpreter_calls=50)
    assert send_message(sm.api, chat, "analyse").terminal.event == "done"


def test_the_kill_switch_skips_the_quota_check(kl):
    chat = new_chat(kl.attach)
    kl.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, for_code_interpreter=True)
    kl.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                           code_interpreter_calls=100)
    assert send_message(kl.api, chat, "analyse").terminal.event == "done"


def test_completed_calls_are_counted_until_the_next_message_is_refused(sm):
    chat = new_chat(sm.attach)
    upload_spreadsheet(sm.api, chat)
    for turn in range(1, CODE_INTERPRETER_DAILY_QUOTA + 1):
        sm.mock.script("POST", "/v1/responses", sse_text(
            ["a", "b"], inserts=[(1, code_interpreter_events(f"run {turn}"))]))
        sent = send_message(sm.api, chat, "analyse")
        assert sent.terminal.event == "done", sent.names()
        assert "code_interpreter" in [e.data["name"] for e in sent.of("tool")]
        wait_for(lambda t=turn: calls_today(sm.attach) == t, f"{turn} settled call(s)")
    assert_exhausted(reject_message(sm.api, chat, {"content": "a fourth"}), "code_interpreter")
    monthly = sm.attach.quota_row(USER_A_ID, "total", "monthly")
    assert monthly["code_interpreter_calls"] == CODE_INTERPRETER_DAILY_QUOTA
    premium = sm.attach.quota_row(USER_A_ID, "tier:premium", "daily")
    assert premium is None or premium["code_interpreter_calls"] == 0, "never in the tier bucket"


def test_a_turn_over_the_per_message_limit_fails_with_code_interpreter_calls_exceeded(sm):
    chat = new_chat(sm.attach)
    upload_spreadsheet(sm.api, chat)
    events = []
    for n in range(3):
        events += code_interpreter_events(f"run {n}")
    sm.mock.script("POST", "/v1/responses", sse_text(["a", "b"], inserts=[(1, events)]))
    sent = send_message(sm.api, chat, "run the analysis")
    assert sent.error["code"] == "code_interpreter_calls_exceeded"
    turn = sm.attach.turn(chat, sent.request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "code_interpreter_calls_exceeded")
    assert turn["code_interpreter_completed_count"] == 2


def test_retry_and_edit_are_429_before_the_commit_when_the_daily_quota_is_exhausted(sm):
    chat = new_chat(sm.attach)
    upload_spreadsheet(sm.api, chat)
    first = send_message(sm.api, chat, "analyse")
    assert first.terminal.event == "done"
    sm.attach.insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="daily",
                           code_interpreter_calls=CODE_INTERPRETER_DAILY_QUOTA)

    before = sm.attach.snapshot(chat)
    for op in ("retry", "edit"):
        assert_exhausted(reject_mutation(sm.api, op, chat, first.request_id), "code_interpreter")
    assert sm.attach.snapshot(chat) == before, "nothing was committed"
    assert len(responses_requests(sm.mock)) == 1


# --------------------------------------------------------------------------- #
# images as model input
# --------------------------------------------------------------------------- #


def test_an_image_follows_the_text_as_an_input_image_with_its_provider_file_id(api, attach,
                                                                               mock_provider):
    chat = new_chat(attach)
    image = upload_image(api, chat)
    provider_file = attach.provider_file_of(image)

    sent = send_message(api, chat, "What is in this image?", attachment_ids=[image])
    assert sent.terminal.event == "done", sent.names()
    request = last_responses_request(mock_provider)
    assert user_content(request) == [
        {"type": "input_text", "text": "What is in this image?"},
        {"type": "input_image", "file_id": provider_file}]
    assert request.tools() == [], "an image is not a retrieval document"
    assert provider_file not in sent.blob()
    assert attach.stores_of(chat) == []


def test_several_images_are_appended_as_further_input_images(api, attach, mock_provider):
    chat = new_chat(attach)
    images = [upload_image(api, chat, f"photo{i}.png") for i in range(3)]
    sent = send_message(api, chat, "compare them", attachment_ids=images)
    assert sent.terminal.event == "done"
    request = last_responses_request(mock_provider)
    parts = user_content(request)
    assert len(parts) == 4 and parts[0] == {"type": "input_text", "text": "compare them"}
    assert set(input_image_ids(request)) == {attach.provider_file_of(i) for i in images}


def test_only_the_image_of_a_mixed_attachment_list_becomes_model_input(api, attach, mock_provider):
    chat = new_chat(attach)
    doc, image = upload_document(api, chat, "doc.pdf"), upload_image(api, chat)
    send_message(api, chat, "use both", attachment_ids=[doc, image])
    request = last_responses_request(mock_provider)
    assert input_image_ids(request) == [attach.provider_file_of(image)]
    assert len(user_content(request)) == 2
    assert attach.provider_file_of(doc) not in str(request.json)
    assert request.tool("file_search"), "the document is searched, not sent"


def test_images_of_earlier_turns_are_never_reused_implicitly(api, attach, mock_provider):
    chat = new_chat(attach)
    image = upload_image(api, chat)
    send_message(api, chat, "what is this?", attachment_ids=[image])
    send_message(api, chat, "and what colour is it?")
    request = last_responses_request(mock_provider)
    assert all_input_images(request) == []
    assert "user:what is this?" in request.conversation_texts()
    # an explicit re-attachment sends it again - and only it
    send_message(api, chat, "look again", attachment_ids=[image])
    again = last_responses_request(mock_provider)
    assert input_image_ids(again) == [attach.provider_file_of(image)]
    assert len(all_input_images(again)) == 1


def test_images_are_charged_per_image_in_the_reserve(api, attach, mock_provider):
    chat = new_chat(attach)
    images = [upload_image(api, chat, f"p{i}.png") for i in range(2)]
    sent = send_message(api, chat, "look", attachment_ids=images)
    turn = attach.turn(chat, sent.request_id)
    assert turn["reserve_tokens"] == catalog.reserve_for(
        PREMIUM, "look", surcharge_tokens=2 * 1000)["reserve_tokens"]


# --------------------------------------------------------------------------- #
# image guards
# --------------------------------------------------------------------------- #


def test_an_image_on_a_model_without_vision_is_400_vision_not_supported(api, attach, mock_provider):
    chat = new_chat(attach, STANDARD)
    image = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image")
    before = attach.counts(chat)
    resp = reject_message(api, chat, {"content": "what is this?", "attachment_ids": [image]})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content_type", "VISION_NOT_SUPPORTED")
    assert attach.counts(chat) == before
    assert not responses_requests(mock_provider)


def test_the_vision_check_is_made_on_the_effective_model_after_a_downgrade(fs):
    chat = new_chat(fs.attach)
    image = fs.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image")
    resp = reject_message(fs.api, chat, {"content": "what is this?", "attachment_ids": [image]})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content_type", "VISION_NOT_SUPPORTED")
    assert not responses_requests(fs.mock)
    # the same chat without the image is served by the standard model
    sent = send_message(fs.api, chat, "no image this time")
    assert sent.terminal.event == "done"
    assert sent.done["effective_model"] == STANDARD


def test_more_images_than_rag_max_images_per_message_is_400_too_many_images(sm):
    chat = new_chat(sm.attach)
    images = [upload_image(sm.api, chat, f"p{i}.png", 8, 8) for i in range(SMALL_MAX_IMAGES + 1)]
    ok = send_message(sm.api, chat, "two are fine", attachment_ids=images[:SMALL_MAX_IMAGES])
    assert ok.terminal.event == "done"

    before = sm.attach.counts(chat)
    resp = reject_message(sm.api, chat, {"content": "three are not", "attachment_ids": images})
    problem = assert_problem(resp, 400, "out_of_range")
    assert_field_violation(problem, "image_count", "TOO_MANY_IMAGES")
    assert sm.attach.counts(chat) == before
    assert len(responses_requests(sm.mock)) == 1


def test_the_default_image_limit_is_four_and_documents_do_not_count(api, attach, mock_provider):
    chat = new_chat(attach)
    images = [upload_image(api, chat, f"p{i}.png", 8, 8) for i in range(5)]
    docs = [upload_document(api, chat, f"d{i}.pdf") for i in range(2)]
    ok = send_message(api, chat, "four images and two documents", attachment_ids=images[:4] + docs)
    assert ok.terminal.event == "done", ok.names()
    assert len(all_input_images(last_responses_request(mock_provider))) == 4
    resp = reject_message(api, chat, {"content": "five", "attachment_ids": images})
    problem = assert_problem(resp, 400, "out_of_range")
    assert_field_violation(problem, "image_count", "TOO_MANY_IMAGES")


def test_an_image_message_is_refused_while_disable_images_is_on(kl):
    chat = new_chat(kl.attach)
    image = kl.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image")
    before = kl.attach.counts(chat)
    resp = reject_message(kl.api, chat, {"content": "what is this?", "attachment_ids": [image]})
    problem = assert_problem(resp, 400, "failed_precondition")
    assert_precondition(problem, "images", "FEATURE_DISABLED")
    assert kl.attach.counts(chat) == before
    assert not responses_requests(kl.mock)
    # documents and plain text are not affected
    doc = kl.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID)
    assert send_message(kl.api, chat, "about the document", attachment_ids=[doc]).terminal.event \
        == "done"


# --------------------------------------------------------------------------- #
# retry / edit carry the attachments into the provider request
# --------------------------------------------------------------------------- #


def turn_with_attachments(api, attach, *kinds):
    chat = new_chat(attach)
    ids = []
    for kind in kinds:
        ids.append({"doc": upload_document, "sheet": upload_spreadsheet,
                    "image": upload_image}[kind](api, chat))
    sent = send_message(api, chat, "look at these", attachment_ids=ids)
    assert sent.terminal.event == "done", sent.names()
    return chat, sent, ids


def test_a_retry_sends_the_same_images_and_tools_again(api, attach, mock_provider):
    chat, first, (doc, sheet, image) = turn_with_attachments(api, attach, "doc", "sheet", "image")
    original = last_responses_request(mock_provider)

    retried = mutate_stream(api, "retry", chat, first.request_id)
    assert retried.terminal.event == "done", retried.names()
    request = last_responses_request(mock_provider)
    assert input_image_ids(request) == [attach.provider_file_of(image)]
    assert user_content(request)[0] == {"type": "input_text", "text": "look at these"}
    assert tool_set(request) == tool_set(original) == {"file_search", "code_interpreter"}
    assert request.tool("file_search") == original.tool("file_search")
    assert request.tool("code_interpreter") == original.tool("code_interpreter")
    new_user = uuid_text(attach.user_message_of(chat, retried.request_id)["id"])
    assert attach.links_of(new_user) == sorted([doc, sheet, image])


def test_an_edit_sends_the_new_text_with_the_same_images(api, attach, mock_provider):
    chat, first, (image,) = turn_with_attachments(api, attach, "image")
    edited = mutate_stream(api, "edit", chat, first.request_id, content="describe the colours")
    assert edited.terminal.event == "done", edited.names()
    request = last_responses_request(mock_provider)
    assert user_content(request)[0] == {"type": "input_text", "text": "describe the colours"}
    assert input_image_ids(request) == [attach.provider_file_of(image)]
    assert "look at these" not in str(request.json), "the replaced turn is not in the context"


def test_a_deleted_attachment_is_not_carried_into_the_retry(api, attach, mock_provider):
    chat = new_chat(attach)
    first = send_message(api, chat, "look at these")
    kept = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image",
                             filename="kept.png")
    gone = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image",
                             filename="gone.png", deleted=True)
    old_user = uuid_text(attach.user_message_of(chat, first.request_id)["id"])
    for attachment in (kept, gone):
        attach.link_attachment(tenant=TENANT_A_ID, chat=chat, message=old_user,
                               attachment=attachment)

    retried = mutate_stream(api, "retry", chat, first.request_id)
    assert retried.terminal.event == "done", retried.names()
    request = last_responses_request(mock_provider)
    assert input_image_ids(request) == [attach.provider_file_of(kept)]
    assert attach.provider_file_of(gone) not in str(request.json)
    new_user = uuid_text(attach.user_message_of(chat, retried.request_id)["id"])
    assert attach.links_of(new_user) == [kept], "the deleted attachment is silently excluded"


def test_retry_and_edit_are_refused_when_the_effective_model_lost_vision(fs):
    chat = new_chat(fs.attach)
    # a premium turn with an image happened before the switch (seeded: the switch is on from the
    # start of this stack)
    pair = fs.attach.insert_pair(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID)
    image = fs.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image")
    fs.attach.link_attachment(tenant=TENANT_A_ID, chat=chat, message=pair["user_message"],
                              attachment=image)
    before = fs.attach.snapshot(chat)
    for op in ("retry", "edit"):
        resp = reject_mutation(fs.api, op, chat, pair["request_id"])
        problem = assert_problem(resp, 400, "invalid_argument")
        assert_field_violation(problem, "content_type", "VISION_NOT_SUPPORTED")
    assert fs.attach.snapshot(chat) == before, "a rejected mutation changes nothing"
    assert not responses_requests(fs.mock)


def test_retry_and_edit_are_refused_while_disable_images_is_on(kl):
    chat = new_chat(kl.attach)
    pair = kl.attach.insert_pair(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID)
    image = kl.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image")
    kl.attach.link_attachment(tenant=TENANT_A_ID, chat=chat, message=pair["user_message"],
                              attachment=image)
    before = kl.attach.snapshot(chat)
    for op in ("retry", "edit"):
        resp = reject_mutation(kl.api, op, chat, pair["request_id"])
        problem = assert_problem(resp, 400, "failed_precondition")
        assert_precondition(problem, "images", "FEATURE_DISABLED")
    assert kl.attach.snapshot(chat) == before
    assert not responses_requests(kl.mock)
    # a turn without images is not blocked
    other = new_chat(kl.attach)
    plain = kl.attach.insert_pair(tenant=TENANT_A_ID, chat=other, user=USER_A_ID)
    assert mutate_stream(kl.api, "retry", other, plain["request_id"]).terminal.event == "done"


def test_a_retry_sends_the_spreadsheet_tool_even_when_the_original_did_not_reference_it(
        api, attach, mock_provider):
    chat = new_chat(attach)
    first = send_message(api, chat, "hello")
    sheet = upload_spreadsheet(api, chat, "late.xlsx")
    retried = mutate_stream(api, "retry", chat, first.request_id)
    assert retried.terminal.event == "done"
    tool = last_responses_request(mock_provider).tool("code_interpreter")
    assert file_ids_of(tool) == {attach.provider_file_of(sheet)}
