# mini-chat-attachments Specification

## Purpose

Document and image attachments: upload, indexing, retrieval tools, deletion, provider cleanup and abandoned-upload recovery.

## Requirements

### Requirement: Upload attachment
`POST /v1/chats/{id}/attachments` (multipart field `file`) SHALL, before reading the body, resolve the chat (404 chat type) and its model (400 `INVALID_MODEL` if removed); then validate: missing boundary (400 `BOUNDARY_REQUIRED`), unreadable multipart (400 `MULTIPART_ERROR`), no `file` field (400 `MISSING_FILE`), no part content type (400 `MISSING_CONTENT_TYPE`), unsupported MIME (400 `UNSUPPORTED_CONTENT_TYPE`; `application/octet-stream` is inferred from the extension), size above `min(rag.uploaded_file_max_size_kb | rag.uploaded_image_max_size_kb, model max_file_size_mb)` (400 `out_of_range` `FILE_TOO_LARGE`), XLSX when code interpreter is unavailable (400 `CODE_INTERPRETER_UNAVAILABLE`), image under `disable_images` (400 `FEATURE_DISABLED`), per-chat document count / total size (429 `document_limit` / `storage_limit`). It SHALL insert the row as `pending`, upload the file to the provider Files API (`purpose=assistants`), mark it `uploaded`, then for documents with `for_file_search` add it to the chat vector store (created lazily, one per chat) and poll indexing until `completed` (→ `ready`) for at most 25 s; images get a WebP thumbnail (best effort) and become `ready`; XLSX becomes `ready` without indexing. Success SHALL return 201 with `AttachmentDetail`. Provider failures SHALL mark the row `failed` with an `error_code` and return 503 with `Retry-After: 10`. Filenames default to `upload` and are truncated to 255 characters.

#### Scenario: Document upload ready
- **WHEN** a PDF is uploaded and the provider reports indexing `completed`
- **THEN** the response is 201 with `status: ready`, `kind: document` and no provider ids

#### Scenario: Unsupported type
- **WHEN** a file with content type `application/x-msdownload` is uploaded
- **THEN** the response is 400 `UNSUPPORTED_CONTENT_TYPE`

#### Scenario: Image thumbnail
- **WHEN** a PNG is uploaded
- **THEN** the response has `kind: image`, `status: ready` and `img_thumbnail {content_type: "image/webp", width, height, data_base64}` fitting 128x128

### Requirement: Asynchronous indexing lifecycle
When indexing is still `in_progress` at the 25 s deadline the upload SHALL return 201 `status: uploaded` and a background task SHALL keep polling (rounds refreshing `updated_at`) for up to 10 minutes: `completed` → `ready`; `failed`/`cancelled`/error/timeout → `failed` with `error_code = indexing_failed`, `cleanup_status = pending` and an `attachment_indexing_failed` cleanup outbox event in one transaction. A `failed` indexing within the deadline SHALL return 503 and set `indexing_failed`.

#### Scenario: Indexing failure
- **WHEN** the vector store reports the file `failed`
- **THEN** the upload returns 503 and the attachment GET shows `status: failed`, `error_code: indexing_failed`

### Requirement: Get and delete attachment
`GET /v1/chats/{id}/attachments/{attachment_id}` SHALL return `AttachmentDetail` (`id`, `filename`, `content_type`, `size_bytes`, `status`, `kind`, `error_code` only when failed, `img_thumbnail` only for ready images, `doc_summary` and `summary_updated_at` absent, `created_at`). `DELETE` SHALL soft-delete the attachment and enqueue an `attachment_deleted` cleanup outbox event in one transaction and return 204; a repeated delete SHALL return 204 without a new event; an attachment referenced by a message SHALL return 409 `already_exists` `resource_name = attachment_locked`. Attachments of another chat, of another uploader, unknown or deleted SHALL return 404 with the attachment resource type (except the idempotent repeated delete).

#### Scenario: Locked attachment
- **WHEN** an attachment referenced by a sent message is deleted
- **THEN** the response is 409 with `resource_name = attachment_locked`

### Requirement: Attachments made available to provider tools
Ready documents (`for_file_search`) SHALL make the `file_search` tool (with the chat vector store id and catalog `max_num_results`) part of every provider request of the chat when the effective model supports it and `disable_file_search` is off; ready XLSX attachments SHALL add `code_interpreter` with `container.file_ids` and `include: ["code_interpreter_call.outputs"]`; images in `attachment_ids` SHALL be sent as `input_image` items with their provider file ids. File citations SHALL be mapped to `attachment_id` + filename and unknown or deleted files omitted.

#### Scenario: File search tool
- **WHEN** a message is sent in a chat with a ready document
- **THEN** the provider request contains a `file_search` tool with `vector_store_ids` of the chat's store

### Requirement: Cleanup and abandoned-upload recovery
The attachment cleanup handler SHALL delete the provider file (2xx/404 = success → `cleanup_status = done`; other failures increment `cleanup_attempts`, record `last_cleanup_error` and retry; at `cleanup_worker.max_attempts` → `failed` and reject), skipping attachments of deleted chats. The chat cleanup handler SHALL delete provider files of all pending attachments of a soft-deleted chat, then delete the vector store and its `chat_vector_stores` row once all are terminal (404 = success), and reject messages for non-deleted chats. The upload reaper SHALL mark `pending`/`uploaded` rows older than `upload_reaper.stale_after_secs` with no `cleanup_status` as `failed`, `error_code = upload_abandoned`, enqueueing an `attachment_upload_abandoned` cleanup when a provider file exists.

#### Scenario: Chat deletion cleanup
- **WHEN** a chat with one ready document is deleted
- **THEN** the provider receives a file DELETE and a vector store DELETE, the attachment ends `cleanup_status = done` and the `chat_vector_stores` row is removed

#### Scenario: Abandoned upload
- **WHEN** an attachment row stays `pending` longer than the stale threshold
- **THEN** the reaper marks it `failed` with `error_code = upload_abandoned`
