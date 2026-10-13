# Data Model: Mini Chat Gear

Normative source: `gears/mini-chat/docs/DESIGN.md` §3.7 (tables), §3.9 (turn mutations), §4 (turn lifecycle), §5 (quota). Table and column names below are exact; the hidden acceptance suite inspects them. SQLite stores UUID columns as 16-byte BLOBs (SeaORM `Uuid` binding); timestamps as RFC 3339 text via `TimestampWithTimeZone`.

Every table has `tenant_id` (Secure ORM tenant column). Owner column `user_id` on `chats`, `message_reactions`, `quota_usage`.

## chats
| column | type | notes |
|---|---|---|
| id | uuid PK | |
| tenant_id, user_id | uuid NOT NULL | owner |
| model | varchar(1024) NOT NULL | selected model, immutable |
| title | varchar(255) NULL | omitted from JSON when NULL |
| is_temporary | bool NOT NULL default false | |
| created_at, updated_at | timestamptz NOT NULL | updated_at bumped on create/rename/delete/send/retry/edit |
| deleted_at | timestamptz NULL | soft delete |

Index `(tenant_id, user_id, updated_at)` where deleted_at IS NULL.

## messages
id, tenant_id, chat_id (FK cascade), request_id (nullable uuid), role (`user|assistant|system`), content (text), content_type (`text`), token_estimate (int, 0), provider_response_id (varchar 128 null), request_kind (`chat`), features_used (json text `[]`), input_tokens/output_tokens/cache_read_input_tokens/cache_write_input_tokens/reasoning_tokens (bigint default 0), model (varchar 1024 null; effective model on assistant rows), is_compressed (bool default false), created_at, deleted_at.
Unique `(chat_id, request_id, role)` where request_id not null and deleted_at null; unique `(id, chat_id)`; index `(chat_id, created_at)`.

## chat_turns
id, tenant_id, chat_id (FK), request_id, requester_type (`user|system`), requester_user_id, state (`running|completed|failed|cancelled`), provider_name (null), provider_response_id, assistant_message_id, error_code, reserve_tokens (bigint null), max_output_tokens_applied (int null), reserved_credits_micro (bigint null), policy_version_applied (bigint null), effective_model (varchar null), minimal_generation_floor_applied (int null), error_detail (text), deleted_at, replaced_by_request_id, started_at, last_progress_at, web_search_enabled (bool), web_search_completed_count, code_interpreter_completed_count, file_search_completed_count (int default 0), completed_at, updated_at.
Unique `(chat_id, request_id)`; unique `(chat_id)` where state='running' and deleted_at null; index `(chat_id, started_at)` where deleted_at null; index `(last_progress_at)` where running.

State machine: `running → completed | failed | cancelled` (CAS `WHERE state='running'`); terminal immutable. Orphan CAS also requires `deleted_at IS NULL AND COALESCE(last_progress_at, started_at) <= cutoff`.

## attachments
id, tenant_id, chat_id, uploaded_by_user_id, filename (255), content_type (128), size_bytes, storage_backend (32, default `azure`), provider_file_id (128 null), status (`pending|uploaded|ready|failed`), error_code (64), attachment_kind (`document|image`), for_file_search, for_code_interpreter (bool), doc_summary (null), img_thumbnail (blob), img_thumbnail_width/height, summary_model, summary_updated_at, cleanup_status (`pending|done|failed` null), cleanup_attempts (int 0), last_cleanup_error, cleanup_updated_at, created_at, updated_at, deleted_at, secondary_file_id, secondary_status (`not_attempted` default), secondary_provider_kind.
Status transitions: `pending → uploaded → ready`, any → `failed`. Error codes: `upload_failed`, `indexing_failed`, `vector_store_failed`, `upload_abandoned`, ...

## message_attachments
PK `(chat_id, message_id, attachment_id)`; tenant_id, created_at. Composite FKs to `messages(id, chat_id)` and `attachments(id, chat_id)`.

## thread_summaries
id, tenant_id, chat_id (unique), summary_text, summarized_up_to_created_at, summarized_up_to_message_id, token_estimate, created_at, updated_at.

## chat_vector_stores
id, tenant_id, chat_id (unique with tenant), vector_store_id (nullable = creation in progress), provider, file_count (0), created_at.

## quota_usage
id, tenant_id, user_id, period_type (`daily|monthly`), period_start (date), bucket (`total|tier:premium`), spent_credits_micro, reserved_credits_micro, calls, input_tokens, output_tokens, file_search_calls, web_search_calls, code_interpreter_calls, rag_retrieval_calls, image_inputs, image_upload_bytes, updated_at. Unique `(tenant_id, user_id, period_type, period_start, bucket)`.

## message_reactions
id, message_id (FK), user_id, tenant_id, reaction (`like|dislike`), created_at. Unique `(message_id, user_id)`.

## Outbox (shared toolkit-db outbox tables)
Queues: `mini-chat.usage_snapshot` (UsageEvent JSON), `mini-chat.audit` (TurnAuditEvent / mutation audit), `mini-chat.attachment_cleanup`, `mini-chat.chat_cleanup`, `mini-chat.thread_summary`.

## Domain value objects (transient)
- **PolicySnapshot**: `policy_version`, `model_catalog: [ModelCatalogEntry]`, `kill_switches`.
- **UserLimits**: `standard`/`premium` `TierLimits{limit_daily_credits_micro, limit_monthly_credits_micro}`.
- **PreflightDecision**: effective model, tier, decision (`allow|downgrade`), downgrade reason, reserve tokens/credits, max_output_tokens_applied, floor, policy version, period starts, tool flags.
- **ContextPlan**: system instructions, summary (with preamble), recent messages, current user content + images, tools, `messages_truncated`, `assembled_context_tokens`.
