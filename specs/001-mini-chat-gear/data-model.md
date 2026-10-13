# Data Model: Mini Chat Gear

Normative source: `gears/mini-chat/docs/DESIGN.md` §3.7 (tables), §5 (quota), Appendix B. Every
table has `tenant_id` (Secure ORM tenant column). UUID columns are SeaORM `Uuid` (16-byte BLOB on
SQLite). Timestamps are `TIMESTAMPTZ` (PG) / text (SQLite) via `time::OffsetDateTime`.

## chats  (owner-scoped: tenant `tenant_id`, owner `user_id`, resource `id`)

| column | type | notes |
|---|---|---|
| id | uuid PK | |
| tenant_id, user_id | uuid NOT NULL | |
| model | varchar(1024) NOT NULL | selected model, immutable |
| title | varchar(255) NULL | omitted from JSON when null |
| is_temporary | bool NOT NULL default false | |
| created_at, updated_at | ts NOT NULL | updated_at bumped on create/rename/delete/send/retry/edit |
| deleted_at | ts NULL | soft delete |

Index `(tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL`.

## messages  (tenant-scoped; reached through owner-scoped chat)

id, tenant_id, chat_id (FK cascade), request_id NULL, role (`user|assistant|system`), content,
content_type (`text`), token_estimate (0), provider_response_id NULL, request_kind (`chat`),
features_used JSON (`[]`), input_tokens/output_tokens/cache_read_input_tokens/
cache_write_input_tokens/reasoning_tokens BIGINT default 0, model NULL (effective model on
assistant rows), is_compressed bool default false, created_at, deleted_at NULL.
UNIQUE `(chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL`;
UNIQUE `(id, chat_id)`; index `(chat_id, created_at) WHERE deleted_at IS NULL`.

Ordering: `(created_at, id)` strict total order.

## chat_turns

id, tenant_id, chat_id (FK cascade), request_id, requester_type (`user|system`),
requester_user_id NULL, state (`running|completed|failed|cancelled`), provider_name NULL,
provider_response_id NULL, assistant_message_id NULL, error_code NULL, error_detail NULL,
reserve_tokens NULL, max_output_tokens_applied NULL, reserved_credits_micro NULL,
policy_version_applied NULL, effective_model NULL, minimal_generation_floor_applied NULL,
deleted_at NULL, replaced_by_request_id NULL, started_at, last_progress_at NULL,
web_search_enabled bool, web_search_completed_count, code_interpreter_completed_count,
file_search_completed_count (int default 0), completed_at NULL, updated_at.
UNIQUE `(chat_id, request_id)`; UNIQUE `(chat_id) WHERE state='running' AND deleted_at IS NULL`;
index `(chat_id, started_at DESC) WHERE deleted_at IS NULL`; index `(last_progress_at) WHERE
state='running' AND deleted_at IS NULL`. CHECKs on `state`, `requester_type`.

State machine: `running → completed | failed | cancelled` (CAS `WHERE state='running'`); terminal
states immutable. API mapping: completed→done, failed→error, cancelled→cancelled.
Latest turn: max `(started_at, id)` with `deleted_at IS NULL`.

## attachments

id, tenant_id, chat_id (FK cascade), uploaded_by_user_id, filename(255), content_type(128),
size_bytes, storage_backend (default `azure`), provider_file_id NULL, status
(`pending|uploaded|ready|failed`), error_code NULL, attachment_kind (`document|image`),
for_file_search, for_code_interpreter, doc_summary NULL, img_thumbnail BLOB NULL,
img_thumbnail_width/height NULL, summary_model NULL, summary_updated_at NULL, cleanup_status NULL
(`pending|done|failed`), cleanup_attempts default 0, last_cleanup_error NULL, cleanup_updated_at
NULL, created_at, updated_at, deleted_at NULL, secondary_file_id NULL, secondary_status default
`not_attempted`, secondary_provider_kind NULL.
UNIQUE `(id, chat_id)`; indexes `(tenant_id, chat_id) WHERE deleted_at IS NULL`,
`(cleanup_status) WHERE …`, `idx_attachments_stale_upload (status, cleanup_status, deleted_at,
updated_at)`.

Lifecycle: `pending → uploaded → ready`, or `→ failed` (error codes `upload_failed`,
`indexing_failed`, `upload_abandoned`, ...). Kind from MIME (png/jpeg/webp/gif = image).
Purpose: XLSX → `for_code_interpreter`; other documents → `for_file_search`; images → neither.

## message_attachments

PK `(chat_id, message_id, attachment_id)`; tenant_id; created_at. Composite FKs
`(message_id, chat_id) → messages(id, chat_id)`, `(attachment_id, chat_id) → attachments(id,
chat_id)` ON DELETE CASCADE.

## thread_summaries

id, tenant_id, chat_id UNIQUE (FK), summary_text, summarized_up_to_created_at,
summarized_up_to_message_id, token_estimate, created_at, updated_at. Inclusive frontier in
`(created_at, id)` order; CAS on base frontier when committing.

## chat_vector_stores

id, tenant_id, chat_id, vector_store_id NULL (NULL = creation in progress), provider,
file_count (0), created_at. UNIQUE `(tenant_id, chat_id)`. Insert-placeholder-then-CAS creation
protocol; stale placeholder (>120 s) reclaim.

## quota_usage  (owner-scoped: tenant `tenant_id`, owner `user_id`)

id, tenant_id, user_id, period_type (`daily|monthly`), period_start DATE, bucket
(`total|tier:premium`), spent_credits_micro, reserved_credits_micro, calls, input_tokens,
output_tokens, file_search_calls, web_search_calls, code_interpreter_calls, rag_retrieval_calls,
image_inputs, image_upload_bytes, updated_at. UNIQUE `(tenant_id, user_id, period_type,
period_start, bucket)`.

Reserve: `total` always (+ `tier:premium` for premium) `reserved += r`. Settle: `reserved -= r;
spent += committed; calls += 1` (+ tokens/tool calls on `total` per method rules).

## message_reactions  (owner-scoped)

id, message_id (FK cascade), user_id, tenant_id, reaction (`like|dislike`), created_at.
UNIQUE `(message_id, user_id)`.

## Outbox payloads (shared toolkit-db outbox)

| queue (default) | payload | partition key |
|---|---|---|
| `mini-chat.usage_snapshot` | SDK `UsageEvent` JSON (DESIGN §5.6/§5.7, A.3) | tenant_id |
| `mini-chat.audit` | SDK audit event JSON (`turn_completed`, `turn_failed`, `turn_retry`, `turn_edit`, `turn_delete`) | tenant_id |
| `mini-chat.attachment_cleanup` | `{event_type, tenant_id, chat_id, attachment_id, provider_file_id, vector_store_id:null, storage_backend, attachment_kind, deleted_at, secondary_ref}` | tenant_id |
| `mini-chat.chat_cleanup` | `{tenant_id, chat_id, system_request_id, reason:"chat_soft_delete", chat_deleted_at}` | chat_id |
| `mini-chat.thread_summary` | `{tenant_id, chat_id, system_request_id, base_frontier_*, frozen_target_*, system_task_type}` | chat_id |

## Policy (SDK types, from the model-policy plugin)

`PolicySnapshot { policy_version, model_catalog: Vec<ModelCatalogEntry>, kill_switches }`,
`ModelCatalogEntry` (fields per DESIGN §4 Model Catalog / A.1, with defaults), `EstimationBudgets`,
`ModelGeneralConfig { api_params, features, tool_support, supported_endpoints, max_file_size_mb,
type, available_from }`, `KillSwitches` (6 bools), `UserLimits { user_id, policy_version, standard,
premium: TierLimits }`.
