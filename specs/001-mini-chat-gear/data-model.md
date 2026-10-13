# Data Model: mini-chat

Source: DESIGN §3.7 (normative). SQLite stores UUIDs as 16-byte BLOBs in columns declared `TEXT`
(SeaORM/sqlx default `Uuid` encoding), timestamps as RFC 3339 text, `period_start` as a
`YYYY-MM-DD` date. All tables carry `tenant_id` (Secure ORM tenant column).

## chats

| Column | Type | Notes |
|---|---|---|
| id | UUID PK | resource col |
| tenant_id | UUID NOT NULL | tenant col |
| user_id | UUID NOT NULL | owner col |
| model | VARCHAR(1024) NOT NULL | selected model, immutable |
| title | VARCHAR(255) NULL | trimmed, 1–255 |
| is_temporary | BOOLEAN NOT NULL default false | |
| created_at, updated_at | TIMESTAMPTZ NOT NULL | updated on create/rename/delete/send/retry/edit |
| deleted_at | TIMESTAMPTZ NULL | soft delete |

Index `(tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL`.

## messages

id, tenant_id, chat_id (FK cascade), request_id (nullable), role (`user|assistant|system`),
content TEXT, content_type (`text`), token_estimate (0), provider_response_id, request_kind
(`chat`), features_used JSON (`[]`), input_tokens, output_tokens, cache_read_input_tokens,
cache_write_input_tokens, reasoning_tokens (BIGINT default 0), model (effective), is_compressed
(bool), created_at, deleted_at. UNIQUE `(chat_id, request_id, role) WHERE request_id IS NOT NULL
AND deleted_at IS NULL`; UNIQUE `(id, chat_id)`; index `(chat_id, created_at) WHERE deleted_at IS
NULL`.

## chat_turns

id, tenant_id, chat_id (FK), request_id, requester_type (`user|system`, CHECK), requester_user_id,
state (`running|completed|failed|cancelled`, CHECK), provider_name (NULL), provider_response_id,
assistant_message_id, error_code, reserve_tokens, max_output_tokens_applied,
reserved_credits_micro, policy_version_applied, effective_model, minimal_generation_floor_applied,
error_detail, deleted_at, replaced_by_request_id, started_at, last_progress_at,
web_search_enabled (bool), web_search_completed_count, code_interpreter_completed_count,
file_search_completed_count, completed_at, updated_at.
UNIQUE `(chat_id, request_id)`; UNIQUE `(chat_id) WHERE state='running' AND deleted_at IS NULL`;
index `(chat_id, started_at DESC) WHERE deleted_at IS NULL`; index `(last_progress_at) WHERE
state='running' AND deleted_at IS NULL`.

State machine: `running → completed | failed | cancelled`; terminal immutable; every transition is
`UPDATE … WHERE id=? AND state='running'` (orphan CAS adds `deleted_at IS NULL` and stale cutoff).

## attachments

id, tenant_id, chat_id (FK), uploaded_by_user_id, filename (≤255), content_type, size_bytes,
storage_backend (default `azure`), provider_file_id, status (`pending|uploaded|ready|failed`),
error_code, attachment_kind (`document|image`), for_file_search, for_code_interpreter,
doc_summary (NULL), img_thumbnail BLOB, img_thumbnail_width, img_thumbnail_height, summary_model,
summary_updated_at, cleanup_status (`pending|done|failed` NULL), cleanup_attempts (0),
last_cleanup_error, cleanup_updated_at, created_at, updated_at, deleted_at, secondary_file_id,
secondary_status (`not_attempted` default), secondary_provider_kind.
Indexes: `(tenant_id, chat_id) WHERE deleted_at IS NULL`; `(cleanup_status) WHERE cleanup_status IS
NOT NULL AND deleted_at IS NULL`; `idx_attachments_stale_upload (status, cleanup_status,
deleted_at, updated_at)`; UNIQUE `(id, chat_id)`.

Lifecycle: `pending → uploaded → ready | failed`; reaper: `pending|uploaded → failed
(upload_abandoned)`.

## message_attachments

tenant_id, chat_id, message_id, attachment_id, created_at. PK `(chat_id, message_id,
attachment_id)`; FK `(message_id, chat_id) → messages(id, chat_id)`; FK `(attachment_id, chat_id) →
attachments(id, chat_id)`; indexes `(tenant_id, chat_id)`, `(attachment_id, chat_id)`.

## thread_summaries

id, tenant_id, chat_id (UNIQUE, FK), summary_text, summarized_up_to_created_at,
summarized_up_to_message_id, token_estimate, created_at, updated_at.

## chat_vector_stores

id, tenant_id, chat_id, vector_store_id (NULL while creating), provider, file_count (0),
created_at. UNIQUE `(tenant_id, chat_id)`.

## quota_usage

id, tenant_id, user_id, period_type (`daily|monthly`), period_start DATE, bucket
(`total|tier:premium`), spent_credits_micro, reserved_credits_micro, calls, input_tokens,
output_tokens, file_search_calls, web_search_calls, code_interpreter_calls, rag_retrieval_calls,
image_inputs, image_upload_bytes, updated_at. UNIQUE `(tenant_id, user_id, period_type,
period_start, bucket)`.

## message_reactions

id, message_id (FK cascade), user_id, tenant_id, reaction (`like|dislike`, CHECK), created_at.
UNIQUE `(message_id, user_id)`.

## Outbox

Shared toolkit-db outbox tables (created by the platform outbox migrations included in the gear's
migration list). Queues: `mini-chat.usage_snapshot`, `mini-chat.attachment_cleanup`,
`mini-chat.chat_cleanup`, `mini-chat.thread_summary`, `mini-chat.audit` (4 partitions).

Payloads: `UsageEvent` (DESIGN §5.6/§5.7, A.3), `TurnAuditEvent` / mutation audit events,
attachment cleanup event (DESIGN §3.6 "Attachment Deletion"), chat cleanup event
(`tenant_id, chat_id, system_request_id, reason="chat_soft_delete", chat_deleted_at`), thread
summary task (`tenant_id, chat_id, system_request_id, base_frontier_*, frozen_target_*,
system_task_type="thread_summary_update"`).

## Credit arithmetic

`credits_micro(i, o, in, out) = ceil(i*in/1e6) + ceil(o*out/1e6)` with checked bounds
(tokens ≤ 10,000,000; multipliers 1..=10,000,000,000).
