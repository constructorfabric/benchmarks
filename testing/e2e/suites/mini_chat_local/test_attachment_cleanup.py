"""Provider-side cleanup and abandoned-upload recovery, black-box.

* Attachment deletion: the outbox ``mini-chat.attachment_cleanup`` queue deletes the provider file
  (retries with backoff, ``cleanup_worker.max_attempts``, dead letter).
* Chat deletion: the outbox ``mini-chat.chat_cleanup`` queue deletes every provider file and then
  the chat's vector store; ``chat_vector_stores`` is the durable marker of the open work.
* The upload reaper fails abandoned ``pending`` / ``uploaded`` rows (``upload_abandoned``) and hands
  their provider file to the attachment cleanup.

Shared server for the happy paths and the retry of a failing provider (default outbox backoff: 1 s,
doubling); the ``attachments_small`` stack (``cleanup_worker.max_attempts = 2``, upload reaper on with
1 s scans and the minimum ``stale_after_secs`` of 60) for the terminal failures and the reaper.
Rows dating from 2025 are stale for any ``stale_after_secs``, so no test waits for an upload to age.

Source: DESIGN "Cleanup on Chat Deletion" (sequence, boundaries, "Attachment cleanup state machine",
"Outbox payload and execution semantics", "Vector store cleanup ordering", "Additional invariants"),
"Attachment Deletion" Phase 2, B.9.2, B.9.3, B.9.5, 3.3 "Upload Attachment" (abandoned uploads).
"""

from __future__ import annotations

import time
from types import SimpleNamespace

import pytest

from . import catalog
from .attachment_support import (
    ATTACHMENT_QUEUE,
    CHAT_CLEANUP_QUEUE,
    PDF,
    PDF_BYTES,
    SMALL_CLEANUP_ATTEMPTS,
    SMALL_MAX_DOCUMENTS,
    AttachmentSeeder,
    assert_resource_type,
    delete_attachment,
    get_attachment,
    now_stored,
    upload,
    upload_document,
    upload_image,
    upload_spreadsheet,
)
from .mock_openai_provider import FILE, VECTOR_STORE, error_response
from .support import (
    TENANT_A_ID,
    USER_A_ID,
    MiniChatApi,
    assert_field_violation,
    assert_problem,
    reject_message,
    send_message,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
WAIT_S = 40.0


def new_chat(attach, model=PREMIUM):
    return attach.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model)


@pytest.fixture()
def sm(small):
    api = MiniChatApi(small.server.base_url)
    try:
        yield SimpleNamespace(api=api, attach=AttachmentSeeder(small.server.db_path),
                              mock=small.mock, server=small.server)
    finally:
        api.close()


def full_chat(api, attach):
    """A chat with two documents, a spreadsheet and an image (all uploaded through the API)."""
    chat = new_chat(attach)
    ids = [upload_document(api, chat, "one.pdf"), upload_document(api, chat, "two.pdf"),
           upload_spreadsheet(api, chat), upload_image(api, chat)]
    return chat, ids


def delete_chat(api, chat):
    resp = api.delete(f"/v1/chats/{chat}")
    assert resp.status_code == 204, resp.text
    return resp


def stores_gone(attach, chat):
    return lambda: not attach.stores_of(chat)


# --------------------------------------------------------------------------- #
# attachment deletion
# --------------------------------------------------------------------------- #


def test_deleting_an_attachment_deletes_its_provider_file_through_the_outbox(api, attach,
                                                                             mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    provider_file = attach.provider_file_of(attachment)

    assert delete_attachment(api, chat, attachment).status_code == 204
    row = attach.wait_cleanup(attachment, "done", WAIT_S)
    assert row["cleanup_updated_at"] is not None
    assert row["cleanup_attempts"] == 0, "only failed attempts are counted"
    assert row["last_cleanup_error"] is None and row["deleted_at"] is not None
    assert mock_provider.deleted_file_ids() == [provider_file]
    request = mock_provider.file_deletes()[0]
    assert request.method == "DELETE" and request.path == f"/v1/files/{provider_file}"
    assert not mock_provider.vector_store_deletes()


def test_a_provider_404_counts_as_success(api, attach, mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    mock_provider.script(*FILE, error_response(404, "No such file"))
    assert delete_attachment(api, chat, attachment).status_code == 204
    row = attach.wait_cleanup(attachment, "done", WAIT_S)
    assert row["cleanup_attempts"] == 0
    assert len(mock_provider.file_deletes()) == 1, "a 404 is not retried"


def test_a_provider_error_is_retried_by_the_outbox_until_the_delete_succeeds(api, attach,
                                                                             mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    mock_provider.fail(FILE, 500, "provider-secret-detail", times=2)

    assert delete_attachment(api, chat, attachment).status_code == 204
    row = attach.wait_cleanup(attachment, "done", WAIT_S)
    assert row["cleanup_attempts"] == 2, "two failed attempts were recorded before the success"
    assert len(mock_provider.file_deletes()) == 3
    assert get_attachment(api, chat, attachment).status_code == 404


def test_an_attachment_that_never_reached_the_provider_is_done_without_a_call(api, attach,
                                                                              mock_provider):
    chat = new_chat(attach)
    attachment = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="failed",
                                   error_code="upload_failed", provider_file_id=None)
    assert delete_attachment(api, chat, attachment).status_code == 204
    attach.wait_cleanup(attachment, "done", WAIT_S)
    assert not mock_provider.file_deletes()


def test_an_image_is_cleaned_like_a_document(api, attach, mock_provider):
    chat = new_chat(attach)
    image = upload_image(api, chat)
    provider_file = attach.provider_file_of(image)
    assert delete_attachment(api, chat, image).status_code == 204
    attach.wait_cleanup(image, "done", WAIT_S)
    assert mock_provider.deleted_file_ids() == [provider_file]


def test_a_message_that_never_succeeds_is_dead_lettered_and_the_row_becomes_failed(sm):
    chat = new_chat(sm.attach)
    attachment = upload_document(sm.api, chat, "a.pdf")
    dead_before = sm.attach.dead_letter_count()
    sm.mock.fail_always(FILE, 500, "boom")

    assert delete_attachment(sm.api, chat, attachment).status_code == 204
    row = sm.attach.wait_cleanup(attachment, "failed", WAIT_S)
    assert row["cleanup_attempts"] == SMALL_CLEANUP_ATTEMPTS, "cleanup_worker.max_attempts"
    assert row["last_cleanup_error"]
    wait_for(lambda: sm.attach.dead_letter_count() == dead_before + 1, "the dead-lettered message")
    assert len(sm.mock.file_deletes()) == SMALL_CLEANUP_ATTEMPTS, "no attempt after the limit"
    assert get_attachment(sm.api, chat, attachment).status_code == 404


# --------------------------------------------------------------------------- #
# chat deletion
# --------------------------------------------------------------------------- #


def test_deleting_a_chat_deletes_every_provider_file_then_the_vector_store(api, attach,
                                                                           mock_provider):
    chat, ids = full_chat(api, attach)
    files = sorted(attach.provider_file_of(i) for i in ids)
    store = attach.vector_store_of(chat)
    send_message(api, chat, "hello", attachment_ids=[ids[0]])
    messages_before = len(attach.messages_of(chat))
    sequence = attach.outbox_sequenced(CHAT_CLEANUP_QUEUE)

    delete_chat(api, chat)
    # the sequencer allocates the sequence number asynchronously (default outbox profile)
    wait_for(lambda: attach.outbox_sequenced(CHAT_CLEANUP_QUEUE) == sequence + 1,
             "the chat-cleanup message to be sequenced", timeout=WAIT_S)
    for attachment in ids:
        row = attach.wait_cleanup(attachment, "done", WAIT_S)
        assert row["cleanup_updated_at"] is not None
        assert row["deleted_at"] is None, "chat cleanup does not soft-delete attachment rows"
    wait_for(stores_gone(attach, chat), "the chat_vector_stores row to go", timeout=WAIT_S)

    assert sorted(mock_provider.deleted_file_ids()) == files, "one delete per provider file"
    store_deletes = mock_provider.vector_store_deletes()
    assert len(store_deletes) == 1 and store_deletes[0].path == f"/v1/vector_stores/{store}"
    last = mock_provider.storage_requests()[-1]
    assert last.route == "DELETE /v1/vector_stores/{vs_id}", "the store goes after every file"
    assert len(attach.messages_of(chat)) == messages_before, "soft-deleted rows are never purged"
    assert attach.chat_deleted(chat)


def test_a_chat_without_provider_resources_needs_no_provider_call(api, attach, mock_provider):
    chat = new_chat(attach)
    send_message(api, chat, "hello")
    dead_before = attach.dead_letter_count()
    delete_chat(api, chat)
    time.sleep(3.0)
    assert not mock_provider.storage_requests()
    assert attach.dead_letter_count() == dead_before


def test_a_failing_provider_is_retried_until_the_chat_is_fully_cleaned(api, attach, mock_provider):
    chat, ids = full_chat(api, attach)
    mock_provider.fail(FILE, 500, "boom")
    mock_provider.fail(VECTOR_STORE, 500, "boom")
    dead_before = attach.dead_letter_count()

    delete_chat(api, chat)
    for attachment in ids:
        attach.wait_cleanup(attachment, "done", 60.0)
    wait_for(stores_gone(attach, chat), "the store to be deleted after the retries", timeout=60.0)
    assert len(mock_provider.vector_store_deletes()) >= 2, "the failed store delete was retried"
    assert attach.dead_letter_count() == dead_before


def test_provider_404s_count_as_success_for_files_and_the_store(api, attach, mock_provider):
    chat, ids = full_chat(api, attach)
    mock_provider.fail(FILE, 404, "gone", times=len(ids))
    mock_provider.fail(VECTOR_STORE, 404, "gone")
    delete_chat(api, chat)
    for attachment in ids:
        attach.wait_cleanup(attachment, "done", WAIT_S)
    wait_for(stores_gone(attach, chat), "the store row to go on a 404", timeout=WAIT_S)


def test_an_attachment_deleted_before_its_chat_is_cleaned_by_the_chat_cleanup(api, attach,
                                                                             mock_provider):
    from .mock_openai_provider import ProviderResponse

    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    provider_file = attach.provider_file_of(attachment)
    # the attachment cleanup is slow: the chat is deleted while it is still queued
    mock_provider.script(*FILE, ProviderResponse(json_body={"deleted": True}, delay=3.0))
    assert delete_attachment(api, chat, attachment).status_code == 204
    delete_chat(api, chat)
    attach.wait_cleanup(attachment, "done", WAIT_S)
    assert provider_file in mock_provider.deleted_file_ids()
    wait_for(stores_gone(attach, chat), "the store to go", timeout=WAIT_S)


def test_attachments_of_other_chats_are_untouched(api, attach, mock_provider):
    chat, ids = full_chat(api, attach)
    other = new_chat(attach)
    survivor = upload_document(api, other, "keep.pdf")
    survivor_store = attach.vector_store_of(other)

    delete_chat(api, chat)
    for attachment in ids:
        attach.wait_cleanup(attachment, "done", WAIT_S)
    wait_for(stores_gone(attach, chat), "the chat store to go", timeout=WAIT_S)
    row = attach.row(survivor)
    assert row["cleanup_status"] is None and row["deleted_at"] is None
    assert attach.vector_store_of(other) == survivor_store
    assert attach.provider_file_of(survivor) not in mock_provider.deleted_file_ids()
    assert survivor_store not in [r.path.rsplit("/", 1)[-1] for r in
                                  mock_provider.vector_store_deletes()]


def test_a_chat_with_attachments_that_never_reached_the_provider_needs_no_file_delete(
        api, attach, mock_provider):
    chat = new_chat(attach)
    rows = [attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status=status,
                              provider_file_id=None)
            for status in ("pending", "failed")]
    delete_chat(api, chat)
    for attachment in rows:
        attach.wait_cleanup(attachment, "done", WAIT_S)
    assert not mock_provider.file_deletes()


def test_a_deleted_chat_can_not_be_deleted_again(api, attach):
    chat = new_chat(attach)
    delete_chat(api, chat)
    assert_resource_type(assert_problem(api.delete(f"/v1/chats/{chat}"), 404, "not_found"), "chat")


def test_a_failed_attachment_does_not_block_the_store_delete(sm):
    chat = new_chat(sm.attach)
    attachment = upload_document(sm.api, chat, "a.pdf")
    sm.mock.fail_always(FILE, 500, "boom")
    delete_chat(sm.api, chat)

    row = sm.attach.wait_cleanup(attachment, "failed", WAIT_S)
    assert row["cleanup_attempts"] == SMALL_CLEANUP_ATTEMPTS
    wait_for(stores_gone(sm.attach, chat), "the store to be deleted despite the failed file",
             timeout=WAIT_S)
    assert len(sm.mock.vector_store_deletes()) == 1


def test_a_store_that_cannot_be_deleted_ends_in_the_dead_letter_store_with_its_row_kept(sm):
    chat = new_chat(sm.attach)
    attachment = upload_document(sm.api, chat, "a.pdf")
    dead_before = sm.attach.dead_letter_count()
    sm.mock.fail_always(VECTOR_STORE, 500, "boom")
    delete_chat(sm.api, chat)

    sm.attach.wait_cleanup(attachment, "done", WAIT_S)
    wait_for(lambda: sm.attach.dead_letter_count() == dead_before + 1,
             "the chat cleanup message to be dead-lettered", timeout=WAIT_S)
    assert len(sm.attach.stores_of(chat)) == 1, "the row is the durable marker of the open work"
    assert len(sm.mock.vector_store_deletes()) == SMALL_CLEANUP_ATTEMPTS


# --------------------------------------------------------------------------- #
# the upload reaper (small stack: scans every second, stale after 60 s)
# --------------------------------------------------------------------------- #


def test_a_stale_uploaded_row_is_failed_and_its_provider_file_is_deleted(sm):
    chat = new_chat(sm.attach)
    attachment = sm.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                      status="uploaded", provider_file_id="file-abandoned-001")
    sequence = sm.attach.outbox_sequenced(ATTACHMENT_QUEUE)

    row = sm.attach.wait_status(attachment, "failed", WAIT_S)
    assert row["error_code"] == "upload_abandoned"
    assert row["deleted_at"] is None, "failed, not soft-deleted"
    assert sm.attach.outbox_sequenced(ATTACHMENT_QUEUE) == sequence + 1
    done = sm.attach.wait_cleanup(attachment, "done", WAIT_S)
    assert done["status"] == "failed"
    assert sm.mock.deleted_file_ids() == ["file-abandoned-001"]

    # still visible through GET, with the failure and no provider id
    got = get_attachment(sm.api, chat, attachment)
    assert got.status_code == 200, got.text
    body = got.json()
    assert body["status"] == "failed" and body["error_code"] == "upload_abandoned"
    assert "file-abandoned-001" not in got.text


def test_a_stale_pending_row_without_a_provider_file_is_failed_without_a_message(sm):
    chat = new_chat(sm.attach)
    attachment = sm.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                      status="pending", provider_file_id=None)
    sequence = sm.attach.outbox_sequenced(ATTACHMENT_QUEUE)
    row = sm.attach.wait_status(attachment, "failed", WAIT_S)
    assert row["error_code"] == "upload_abandoned" and row["cleanup_status"] is None
    time.sleep(2.0)
    assert sm.attach.outbox_sequenced(ATTACHMENT_QUEUE) == sequence
    assert not sm.mock.file_deletes()


def test_rows_the_reaper_must_not_take_stay_as_they_are(sm):
    chat = new_chat(sm.attach)
    attach = sm.attach
    ready = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="ready")
    failed = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="failed",
                               error_code="upload_failed")
    deleted = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="uploaded",
                                deleted=True, provider_file_id=None)
    marked = [attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="uploaded",
                                cleanup_status=state, provider_file_id=None)
              for state in ("pending", "done", "failed")]
    fresh = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="uploaded",
                              provider_file_id=None, created=now_stored(), updated=now_stored())
    # a stale row proves that scans happen while the others stay put
    stale = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="pending",
                              provider_file_id=None)
    attach.wait_status(stale, "failed", WAIT_S)
    time.sleep(2.5)

    assert attach.row(ready)["status"] == "ready"
    assert attach.row(failed)["error_code"] == "upload_failed"
    assert attach.row(deleted)["status"] == "uploaded"
    for attachment in marked:
        assert attach.row(attachment)["status"] == "uploaded", "chat cleanup owns these rows"
    assert attach.row(fresh)["status"] == "uploaded"


def test_a_reaped_row_no_longer_counts_toward_the_document_limit(sm):
    chat = new_chat(sm.attach)
    stuck = [sm.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="uploaded",
                                  provider_file_id=None, filename=f"stuck{i}.pdf")
             for i in range(SMALL_MAX_DOCUMENTS)]
    for attachment in stuck:
        sm.attach.wait_status(attachment, "failed", WAIT_S)
    resp = upload(sm.api, chat, "new.pdf", PDF, PDF_BYTES)
    assert resp.status_code == 201, resp.text


def test_a_reaped_attachment_can_not_be_referenced_by_a_message_and_can_be_deleted(sm):
    chat = new_chat(sm.attach)
    attachment = sm.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                      status="uploaded", provider_file_id=None)
    sm.attach.wait_status(attachment, "failed", WAIT_S)
    resp = reject_message(sm.api, chat, {"content": "q", "attachment_ids": [attachment]})
    assert_field_violation(assert_problem(resp, 400, "invalid_argument"), "attachment",
                           "invalid_attachment")
    assert delete_attachment(sm.api, chat, attachment).status_code == 204
