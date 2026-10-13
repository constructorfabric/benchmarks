## ADDED Requirements

### Requirement: Upload attachment
`POST /mini-chat/v1/chats/{id}/attachments` (multipart, field `file`) SHALL, before reading the body, resolve the chat (404 chat resource type) and its model (400 `INVALID_MODEL` if removed from the catalog). It SHALL validate multipart (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`, `MISSING_CONTENT_TYPE`), infer the MIME type from the extension for `application/octet-stream`, reject unsupported types (400 `UNSUPPORTED_CONTENT_TYPE`), enforce `min(rag.uploaded_file_max_size_kb | rag.uploaded_image_max_size_kb, model max_file_size_mb)` (400 `out_of_range` `FILE_TOO_LARGE`), reject images under `disable_images` (400 FEATURE_DISABLED subject `images`) and code-interpreter-only files (XLSX) when code interpreter is unavailable (400 `CODE_INTERPRETER_UNAVAILABLE`), enforce per-chat `rag.max_documents_per_chat` (429 `document_limit`) and `rag.max_total_upload_mb_per_chat` (429 `storage_limit`), insert the row `pending`, upload to the provider Files API (`purpose=assistants`) → `uploaded`, and then: documents with `for_file_search` are added to the chat vector store (created lazily, one per chat) and polled until `completed` → `ready`; XLSX → `ready`; images get a WebP thumbnail (best effort) → `ready`. It returns 201 `AttachmentDetail`. Provider failures mark the row `failed` with an `error_code` and return 503 with `Retry-After: 10`.

#### Scenario: Image upload
- **WHEN** a PNG is uploaded
- **THEN** 201 with `kind: image`, `status: ready`, and an `img_thumbnail` with `content_type: image/webp` fitting 128x128

#### Scenario: Document upload
- **WHEN** a PDF is uploaded and the vector store reports `completed`
- **THEN** 201 with `kind: document`, `status: ready`, and a `chat_vector_stores` row exists for the chat

#### Scenario: Oversized file
- **WHEN** a file larger than the limit is uploaded
- **THEN** 400 `out_of_range` with `field_violations[content_length].reason = FILE_TOO_LARGE`

### Requirement: Asynchronous indexing
When indexing is still `in_progress` 25 s after the upload started, the upload SHALL return 201 `status: uploaded` and a background task SHALL keep polling (up to 10 min, refreshing `updated_at`), setting `ready` on completion or `failed`/`indexing_failed` with `cleanup_status=pending` and an `attachment_indexing_failed` cleanup outbox event on failure/timeout. A failed status before the deadline marks the row `failed` (`indexing_failed`), deletes the provider file best-effort and returns 503.

#### Scenario: Indexing failure
- **WHEN** the vector store file status is `failed`
- **THEN** the upload returns 503 and GET shows `status: failed`, `error_code: indexing_failed`

### Requirement: Get and delete attachment
`GET /chats/{id}/attachments/{aid}` SHALL return the `AttachmentDetail` (no provider ids; `doc_summary`/`summary_updated_at` absent; `error_code` only when failed; thumbnail only for ready images). `DELETE` SHALL soft-delete it and enqueue an `attachment_deleted` cleanup outbox event in one transaction and return 204; a repeated delete returns 204 without a new event; an attachment referenced by a submitted message returns 409 `already_exists` `resource_name = attachment_locked`. Attachments of another chat or uploaded by another user return 404 (attachment resource type).

#### Scenario: Locked attachment
- **WHEN** an attachment referenced by a sent message is deleted
- **THEN** 409 with `attachment_locked`

### Requirement: Attachments in provider requests
Ready documents SHALL enable the `file_search` tool on the chat vector store; ready XLSX SHALL enable `code_interpreter` with their provider file ids; images in `attachment_ids` SHALL be sent as `input_image` items with the provider file id for the current turn only. File citations SHALL be mapped from provider file ids to `attachment_id` and filename; unknown or deleted attachments are omitted.

#### Scenario: File search tool after document upload
- **WHEN** a chat has a ready document and a message is sent
- **THEN** the provider request contains `{"type":"file_search","vector_store_ids":[<store>],"max_num_results":N}`

### Requirement: Upload reaper
A leader-run periodic reaper SHALL mark attachments stuck in `pending`/`uploaded` (no `cleanup_status`, not deleted) with `updated_at` older than `upload_reaper.stale_after_secs` as `failed` with `error_code = upload_abandoned`, and when a `provider_file_id` exists set `cleanup_status = pending` and enqueue an `attachment_upload_abandoned` cleanup event in the same transaction.

#### Scenario: Abandoned pending upload
- **WHEN** a `pending` row is older than the stale threshold
- **THEN** the reaper marks it `failed` with `upload_abandoned`
