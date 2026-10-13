## ADDED Requirements

### Requirement: Upload attachments with validation
`POST /v1/chats/{id}/attachments` (multipart, field `file`) SHALL check the chat (404) and the chat model (`INVALID_MODEL`) before reading the body, then validate: boundary (`BOUNDARY_REQUIRED`), multipart (`MULTIPART_ERROR`), `file` present (`MISSING_FILE`), part content type (`MISSING_CONTENT_TYPE`), MIME allowlist (`UNSUPPORTED_CONTENT_TYPE`; `application/octet-stream` inferred from extension; CSV → `text/plain` when allowed), `disable_images` for images (400 `failed_precondition` `images/FEATURE_DISABLED`), XLSX without code interpreter (400 `CODE_INTERPRETER_UNAVAILABLE`), size `min(rag limit, model max_file_size_mb)` (400 `out_of_range` `FILE_TOO_LARGE`), per-chat document count and total size (429 `document_limit`/`storage_limit`), concurrency (503 `Retry-After: 5`). Filenames default to `upload` and are truncated to 255 characters keeping the extension.

#### Scenario: Unsupported type
- **WHEN** a `application/x-msdownload` file is uploaded
- **THEN** the response is 400 `invalid_argument` with reason `UNSUPPORTED_CONTENT_TYPE`

#### Scenario: Document limit
- **WHEN** the chat already has `rag.max_documents_per_chat` ready documents
- **THEN** another document upload returns 429 with violation subject `document_limit`

### Requirement: Upload lifecycle and indexing
An accepted upload SHALL insert the row `pending`, upload to the provider Files API (`purpose=assistants`) → `uploaded` with `provider_file_id`; documents with `for_file_search` SHALL be added to the chat's single vector store (created lazily, `chat_vector_stores`) with attribute `attachment_id` and polled until `completed` (→ `ready`) for at most 25 s after start; `failed`/`cancelled`/read errors → `failed` with `error_code = indexing_failed`, provider file deleted, 503 `Retry-After: 10`; still `in_progress` at the deadline → 201 `uploaded` and background polling up to 10 min. XLSX (code interpreter only) and images SHALL become `ready` without the vector store; images get a WebP thumbnail fitting `thumbnail.width×height` (null on failure). Provider failures SHALL return 503 `service_unavailable` `Retry-After: 10` and leave the row `failed`.

#### Scenario: Document ready
- **WHEN** a PDF is uploaded and the provider reports `completed`
- **THEN** the response is 201 with `status: ready`, `kind: document` and no provider ids

#### Scenario: Provider upload failure
- **WHEN** the provider Files API returns 500
- **THEN** the response is 503 and `GET` of the attachment shows `status: failed` with an `error_code`

### Requirement: Get and delete attachments
`GET /v1/chats/{id}/attachments/{attachment_id}` SHALL return `AttachmentDetail` (`id, filename, content_type, size_bytes, status, kind, error_code?, img_thumbnail? (ready images only), created_at`); unknown, deleted, other-chat or other-uploader attachments return 404 with the attachment resource type. `DELETE` SHALL return 409 `already_exists` `resource_name = attachment_locked` when referenced by a message, otherwise soft-delete, enqueue an `attachment_deleted` cleanup message and return 204; repeating it returns 204 without a new outbox message.

#### Scenario: Locked attachment
- **WHEN** an attachment referenced by a sent message is deleted
- **THEN** the response is 409 with `resource_name = attachment_locked`

### Requirement: Attachments available to provider tools
Ready documents SHALL enable `file_search` on the chat vector store; ready XLSX SHALL enable `code_interpreter` with their file ids; images listed in `attachment_ids` SHALL be sent as `input_image` with their provider file id for that turn only. File citations SHALL map provider file ids to `attachment_id` and filename, omitting unknown or deleted ones.

#### Scenario: Image in request
- **WHEN** a message references a ready image
- **THEN** the provider request's user content contains `{type: input_image, file_id: <provider id>}`

### Requirement: Cleanup and abandoned upload recovery
The attachment cleanup handler SHALL delete the provider file (2xx/404 success), mark `cleanup_status = done`, count failed attempts in `cleanup_attempts` and give up as `failed` after `cleanup_worker.max_attempts`. The upload reaper SHALL mark rows `pending`/`uploaded` older than `upload_reaper.stale_after_secs` (and `cleanup_status IS NULL`) as `failed` `upload_abandoned`, enqueuing provider-file cleanup when a `provider_file_id` exists.

#### Scenario: Deleted attachment is cleaned at the provider
- **WHEN** an unreferenced attachment is deleted
- **THEN** the mock provider receives `DELETE .../files/<provider_file_id>` and the row gets `cleanup_status = done`
