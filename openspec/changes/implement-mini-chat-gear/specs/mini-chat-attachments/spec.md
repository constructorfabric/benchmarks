## ADDED Requirements

### Requirement: Upload attachment
`POST /v1/chats/{id}/attachments` (multipart, part `file`) SHALL, before reading the body, authorize and load the chat (404 chat type) and resolve its model (400 `INVALID_MODEL`). It SHALL validate the multipart request (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`, `MISSING_CONTENT_TYPE`), infer the MIME type from the extension for `application/octet-stream`, accept PDF, DOCX, PPTX, XLSX, TXT/MD/CSV(→text/plain)/HTML/JSON and other document types plus images PNG/JPEG/WEBP/GIF (else 400 `UNSUPPORTED_CONTENT_TYPE`), default the filename to `upload` and truncate it to 255 characters keeping the extension, and enforce `min(rag.uploaded_file_max_size_kb | rag.uploaded_image_max_size_kb, model max_file_size_mb)` while streaming (400 `out_of_range` `FILE_TOO_LARGE`). Images under `disable_images` SHALL return 400 `failed_precondition` subject `images`; XLSX while code interpreter is unavailable SHALL return 400 `CODE_INTERPRETER_UNAVAILABLE`. Per-chat limits on non-deleted non-failed attachments SHALL return 429 `document_limit` (`rag.max_documents_per_chat`) or `storage_limit` (`rag.max_total_upload_mb_per_chat`). The file SHALL be uploaded through OAGW to the provider Files API (`purpose=assistants`); documents with `for_file_search` SHALL be added to the chat vector store (created lazily, one per chat) with attribute `attachment_id` and polled until `completed`; images SHALL get a WebP thumbnail. Success SHALL return 201 `AttachmentDetail` with `status: ready` (or `uploaded` when indexing is still running at 25 s). Provider failures SHALL set `status: failed` with an `error_code` and return 503 with `Retry-After: 10`.

#### Scenario: Document upload
- **WHEN** a PDF is uploaded
- **THEN** the provider receives a file upload and a vector store add, and the response is 201 with `status: ready`, `kind: document`

#### Scenario: Image upload with thumbnail
- **WHEN** a PNG is uploaded
- **THEN** the response is 201 with `kind: image` and `img_thumbnail {content_type: image/webp, width, height, data_base64}` and no vector store call is made

#### Scenario: Too large
- **WHEN** a document exceeds the size limit
- **THEN** the response is 400 `FILE_TOO_LARGE`

### Requirement: AttachmentDetail and get
`GET /v1/chats/{id}/attachments/{attachment_id}` SHALL return `AttachmentDetail {id, filename, content_type, size_bytes, status, kind, error_code?, doc_summary?, img_thumbnail?, summary_updated_at?, created_at}` with null fields omitted, `img_thumbnail` only for ready images, and no provider identifiers. Attachments of another chat, soft-deleted, or uploaded by another user SHALL return 404 (attachment type).

#### Scenario: Failed upload visible
- **WHEN** the provider upload failed
- **THEN** GET returns `status: failed` with `error_code`

### Requirement: Delete attachment
`DELETE /v1/chats/{id}/attachments/{attachment_id}` SHALL return 409 `already_exists` `resource_name = attachment_locked` when referenced by a message; otherwise soft-delete the row, set `cleanup_status = pending` and enqueue an `attachment_deleted` cleanup message in one transaction and return 204; a repeated delete SHALL return 204 without a new outbox message.

#### Scenario: Referenced attachment locked
- **WHEN** an attachment linked to a sent message is deleted
- **THEN** the response is 409 `attachment_locked`

### Requirement: Tool availability from attachments
`file_search` SHALL be sent only when the chat has a ready non-deleted document with `for_file_search` and the effective model supports it and `disable_file_search` is off, with the chat vector store id. `code_interpreter` SHALL be sent when the chat has a ready XLSX attachment, the model supports it and the kill switch is off, with its provider file ids. Images in `attachment_ids` SHALL be sent as `input_image` with their provider file id on that turn only.

#### Scenario: File search after upload
- **WHEN** a ready document exists in the chat and a message is sent on a model with file search
- **THEN** the provider request has a `file_search` tool with the chat's vector store id

### Requirement: Indexing lifecycle and recovery
Indexing that fails or errors within the deadline SHALL mark the attachment `failed` with `error_code = indexing_failed` and return 503. Indexing still `in_progress` at 25 s SHALL return `uploaded` and continue in the background up to 10 minutes (then `failed`/`indexing_failed` with an attachment cleanup message). The upload reaper SHALL mark `pending`/`uploaded` rows older than `upload_reaper.stale_after_secs` (without `cleanup_status`) as `failed` with `error_code = upload_abandoned`, enqueueing a cleanup message when a provider file exists.

#### Scenario: Indexing failure
- **WHEN** the vector store reports the file `failed`
- **THEN** the upload returns 503 and the attachment is `failed` with `indexing_failed`

#### Scenario: Abandoned upload
- **WHEN** an attachment stays `pending` past the stale threshold
- **THEN** the reaper sets it `failed` with `upload_abandoned`
