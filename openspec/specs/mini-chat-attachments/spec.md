# mini-chat-attachments Specification

## Purpose
Attachments of the mini-chat gear: document and image upload to the provider Files API, vector store indexing, limits, availability to provider tools, deletion cleanup and abandoned-upload recovery.

## Requirements

### Requirement: Attachment upload, get and delete
`POST /v1/chats/{id}/attachments` (multipart field `file`) SHALL upload the file through OAGW to the RAG provider Files API (`purpose=assistants`), insert the row `pending → uploaded → ready`, and return 201 `AttachmentDetail {id, filename, content_type, size_bytes, status, kind, error_code?, img_thumbnail?, created_at}`. Before reading the body it SHALL resolve the chat (404) and its model (400 `INVALID_MODEL`). Kind is `image` for png/jpeg/webp/gif, else `document`; XLSX is `for_code_interpreter`, other documents `for_file_search`. `GET` returns the detail (404 for other chats, other uploaders, deleted rows). `DELETE` soft-deletes and enqueues attachment cleanup (204; repeat 204 without a new event; 409 `already_exists` `resource_name = attachment_locked` when referenced by a message).

#### Scenario: Image upload with thumbnail
- **WHEN** a PNG is uploaded
- **THEN** the response is 201 with `kind = image`, `status = ready`, and `img_thumbnail {content_type:"image/webp", width<=128, height<=128, data_base64}`

### Requirement: Size, type and per-chat limits
Uploads SHALL be rejected with 400 `out_of_range` `FILE_TOO_LARGE` above `min(rag.uploaded_file_max_size_kb | rag.uploaded_image_max_size_kb, model max_file_size_mb)`, 400 `UNSUPPORTED_CONTENT_TYPE` for unsupported MIME types (octet-stream inferred from extension), 400 multipart errors (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`, `MISSING_CONTENT_TYPE`), 400 `CODE_INTERPRETER_UNAVAILABLE` for XLSX when code interpreter is unavailable, 400 `failed_precondition` `images`/`FEATURE_DISABLED` for images under `disable_images`, and 429 `resource_exhausted` `document_limit`/`storage_limit` for per-chat limits counted over non-deleted, non-failed attachments.

#### Scenario: Document limit
- **WHEN** a chat already has `rag.max_documents_per_chat` documents and another document is uploaded
- **THEN** the response is 429 with violation subject `document_limit`

### Requirement: Indexing lifecycle
Documents with `for_file_search` SHALL be added to the chat's vector store (created lazily, one per chat, `chat_vector_stores` row) and polled until `completed` (ready) within 25 s of the upload start; `failed`/`cancelled`/non-transient read errors mark the row `failed` with `error_code = indexing_failed` and return 503 `Retry-After: 10`; still `in_progress` at the deadline returns 201 `uploaded` and a background task keeps polling up to 10 minutes, then marks `ready` or `failed` (`indexing_failed`, cleanup event). Provider upload failure marks the row `failed` (`upload_failed`) and returns 503.

#### Scenario: Provider upload failure
- **WHEN** the provider Files API returns 500
- **THEN** the response is 503 with `Retry-After: 10` and `GET` shows `status = failed` with an `error_code`

### Requirement: Attachments available to provider tools
Ready documents SHALL enable `file_search` with the chat vector store id, ready XLSX files SHALL enable `code_interpreter` with their provider file ids, and images referenced in `attachment_ids` SHALL be sent as `input_image` items of the current user message only; file citations resolve provider file ids to attachment id and filename.

#### Scenario: File search tool
- **WHEN** a chat has a ready PDF and the user sends a message
- **THEN** the provider request contains a `file_search` tool with the chat's vector store id

### Requirement: Cleanup and abandoned uploads
Attachment deletion SHALL enqueue an `attachment_deleted` cleanup event; the cleanup handler deletes the provider file (2xx/404 success), marks `cleanup_status = done`, and after `cleanup_worker.max_attempts` failures marks `failed`. The upload reaper SHALL mark `pending`/`uploaded` rows older than `upload_reaper.stale_after_secs` (and `cleanup_status IS NULL`) as `failed` with `error_code = upload_abandoned` and, when a provider file exists, set `cleanup_status = pending` and enqueue `attachment_upload_abandoned`.

#### Scenario: Reaped upload
- **WHEN** an attachment row stays `uploaded` past the stale threshold
- **THEN** the reaper marks it `failed`/`upload_abandoned` and the provider file is deleted
