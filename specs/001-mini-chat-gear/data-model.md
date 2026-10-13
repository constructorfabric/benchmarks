# Data Model: Mini Chat

Normative source: DESIGN.md §3.7. Types: PG / SQLite. UUID = `UUID` / `BLOB` (16 bytes);
TS = `TIMESTAMPTZ` / `TEXT` (RFC 3339); JSON = `JSONB` / `TEXT`. Every table has `tenant_id`
(Secure ORM tenant column).

## chats
`id` PK, `tenant_id`, `user_id` (owner), `model` VARCHAR(1024) NOT NULL, `title` VARCHAR(255) NULL,
`is_temporary` BOOL default false, `created_at`, `updated_at` NOT NULL, `deleted_at` NULL.
Index `(tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL`.
Rules: `model` immutable; `updated_at` bumped on create, rename, delete, send, retry, edit.

## messages
`id` PK, `tenant_id`, `chat_id` FK→chats CASCADE, `request_id` UUID NULL, `role` (`user|assistant|system`),
`content` TEXT, `content_type` (`text`), `token_estimate` INT 0, `provider_response_id` NULL,
`request_kind` (`chat`), `features_used` JSON `[]`, `input_tokens`, `output_tokens`,
`cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens` BIGINT 0, `model` NULL,
`is_compressed` BOOL false, `created_at`, `deleted_at` NULL.
UNIQUE `(chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL`;
UNIQUE `(id, chat_id)`; index `(chat_id, created_at) WHERE deleted_at IS NULL`.
Order key: `(created_at, id)`.

## chat_turns
`id` PK, `tenant_id`, `chat_id` FK, `request_id`, `requester_type` (`user|system`), `requester_user_id`,
`state` (`running|completed|failed|cancelled`), `provider_name` NULL, `provider_response_id`,
`assistant_message_id`, `error_code`, `reserve_tokens`, `max_output_tokens_applied`,
`reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`,
`error_detail`, `deleted_at`, `replaced_by_request_id`, `started_at`, `last_progress_at`,
`web_search_enabled` BOOL, `web_search_completed_count`, `code_interpreter_completed_count`,
`file_search_completed_count` INT 0, `completed_at`, `updated_at`.
UNIQUE `(chat_id, request_id)`; UNIQUE `(chat_id) WHERE state='running' AND deleted_at IS NULL`;
index `(chat_id, started_at DESC) WHERE deleted_at IS NULL`; index `(last_progress_at) WHERE state='running' AND deleted_at IS NULL`.

State machine: `running → completed | failed | cancelled`; terminal immutable; every terminal write is
`UPDATE ... WHERE id=? AND state='running'` (CAS). Latest turn = max `(started_at, id)` with
`deleted_at IS NULL`. API mapping: completed→done, failed→error.

## attachments
`id` PK, `tenant_id`, `chat_id` FK, `uploaded_by_user_id`, `filename`, `content_type`, `size_bytes`,
`storage_backend` default `azure`, `provider_file_id`, `status` (`pending|uploaded|ready|failed`),
`error_code`, `attachment_kind` (`document|image`), `for_file_search`, `for_code_interpreter`,
`doc_summary`, `img_thumbnail` BLOB, `img_thumbnail_width`, `img_thumbnail_height`, `summary_model`,
`summary_updated_at`, `cleanup_status` (`pending|done|failed`), `cleanup_attempts` 0,
`last_cleanup_error`, `cleanup_updated_at`, `created_at`, `updated_at`, `deleted_at`,
`secondary_file_id`, `secondary_status` default `not_attempted`, `secondary_provider_kind`.
UNIQUE `(id, chat_id)`; index `(tenant_id, chat_id) WHERE deleted_at IS NULL`;
`idx_attachments_stale_upload (status, cleanup_status, deleted_at, updated_at)`.
Status: `pending → uploaded → ready | failed`.

## message_attachments
PK `(chat_id, message_id, attachment_id)`, `tenant_id`, `created_at`;
FK `(message_id, chat_id)`→messages, `(attachment_id, chat_id)`→attachments.

## thread_summaries
`id` PK, `tenant_id`, `chat_id` UNIQUE FK, `summary_text`, `summarized_up_to_created_at`,
`summarized_up_to_message_id`, `token_estimate`, `created_at`, `updated_at`.

## chat_vector_stores
`id` PK, `tenant_id`, `chat_id`, `vector_store_id` NULL (in creation), `provider`, `file_count` 0,
`created_at`. UNIQUE `(tenant_id, chat_id)`.

## quota_usage
`id` PK, `tenant_id`, `user_id`, `period_type` (`daily|monthly`), `period_start` DATE, `bucket`
(`total|tier:premium`), `spent_credits_micro`, `reserved_credits_micro` BIGINT, `calls`,
`input_tokens`, `output_tokens`, `file_search_calls`, `web_search_calls`, `code_interpreter_calls`,
`rag_retrieval_calls`, `image_inputs`, `image_upload_bytes`, `updated_at`.
UNIQUE `(tenant_id, user_id, period_type, period_start, bucket)`.

## message_reactions
`id` PK, `message_id` FK, `user_id`, `tenant_id`, `reaction` (`like|dislike`), `created_at`.
UNIQUE `(message_id, user_id)`.

## Outbox payloads (JSON)
- **UsageEvent** (`mini-chat.usage_snapshot`): `tenant_id, user_id?, chat_id, turn_id?, request_id,
  effective_model, selected_model, terminal_state, billing_outcome, usage?{input_tokens,
  output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_tokens},
  actual_credits_micro, settlement_method, policy_version_applied, web_search_calls,
  code_interpreter_calls, file_search_calls, timestamp, requester_type, dedupe_key,
  system_task_type?` — dedupe `{tenant}/{turn}/{request}` (simple hex).
- **TurnAuditEvent / mutation audit** (`mini-chat.audit`): `event_type` (`turn_completed|turn_failed|
  turn_retry|turn_edit|turn_delete`), identities, models, usage, latency, tool calls, quota decision.
- **AttachmentCleanup** (`mini-chat.attachment_cleanup`): `event_type, tenant_id, chat_id,
  attachment_id, provider_file_id, vector_store_id(null), storage_backend, attachment_kind,
  deleted_at, secondary_ref`.
- **ChatCleanup** (`mini-chat.chat_cleanup`): `tenant_id, chat_id, system_request_id,
  reason="chat_soft_delete", chat_deleted_at`.
- **ThreadSummaryTask** (`mini-chat.thread_summary`): `tenant_id, chat_id, system_request_id,
  base_frontier_created_at?, base_frontier_message_id?, frozen_target_created_at,
  frozen_target_message_id, system_task_type="thread_summary_update"`.
