# Data Model: mini-chat (source: DESIGN.md §3.7, lines ~2043–2472)

All tables have `tenant_id` (Secure ORM tenant col). SQLite: UUID columns are declared TEXT but values are
written as 16-byte BLOBs (SeaORM `Uuid` default binding). Timestamps TIMESTAMPTZ (SQLite TEXT).
Simple CHECKs enforced (state, requester_type, attachment_kind, status, reaction, secondary_*, file_count>=0);
cross-column CHECKs are NOT enforced (repositories keep invariants).

## chats  (owner col `user_id`, resource `id`)
id UUID PK, tenant_id NN, user_id NN, model VARCHAR(1024) (selected model, immutable), title VARCHAR(255) NULL,
is_temporary BOOL default false, created_at NN, updated_at NN, deleted_at NULL.
Index (tenant_id,user_id,updated_at DESC) WHERE deleted_at IS NULL.

## messages  (no owner col; filtered by chat_id from owner-scoped chat query)
id UUID PK, tenant_id NN, chat_id NN FK chats ON DELETE CASCADE, request_id UUID NULL, role VARCHAR(16) NN
(user|assistant|system), content TEXT NN, content_type VARCHAR(32) NN ('text' always), token_estimate INT (0),
provider_response_id VARCHAR(128) NULL, request_kind VARCHAR(16) ('chat'), features_used JSON NN default '[]',
input_tokens BIGINT NN 0, output_tokens BIGINT NN 0, cache_read_input_tokens BIGINT 0, cache_write_input_tokens
BIGINT 0, reasoning_tokens BIGINT 0, model VARCHAR(1024) NULL (effective model on assistant), is_compressed BOOL
false, created_at NN, deleted_at NULL.
UNIQUE (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL.
Index (chat_id, created_at) WHERE deleted_at IS NULL. UNIQUE (id, chat_id).
Ordering for frontiers: (created_at ASC, id ASC).

## chat_turns
id UUID PK, tenant_id NN, chat_id NN FK cascade, request_id UUID NN, requester_type VARCHAR(16) CHECK user|system,
requester_user_id UUID NULL, state VARCHAR(16) CHECK running|completed|failed|cancelled, provider_name NULL (never),
provider_response_id VARCHAR(128) NULL, assistant_message_id UUID NULL, error_code VARCHAR(64) NULL,
reserve_tokens BIGINT NULL, max_output_tokens_applied INT NULL, reserved_credits_micro BIGINT NULL,
policy_version_applied BIGINT NULL, effective_model VARCHAR(1024) NULL, minimal_generation_floor_applied INT NULL,
error_detail TEXT NULL, deleted_at NULL, replaced_by_request_id UUID NULL, started_at NN (app clock),
last_progress_at NULL (set on create; refreshed <=every 30s on deltas/tool events), web_search_enabled BOOL NN false,
web_search_completed_count INT NN 0, code_interpreter_completed_count INT NN 0, file_search_completed_count INT NN 0,
completed_at NULL, updated_at.
UNIQUE (chat_id, request_id). Index (chat_id, started_at DESC) WHERE deleted_at IS NULL.
UNIQUE (chat_id) WHERE state='running' AND deleted_at IS NULL. Index (last_progress_at) WHERE state='running' AND deleted_at IS NULL.
Preflight cols set on INSERT for send; NULL on retry/edit insert then filled in reserve txn.
Latest turn = greatest (started_at, id) where deleted_at IS NULL. State transitions running -> terminal only (CAS).

## attachments
id UUID PK, tenant_id NN, chat_id NN FK cascade, uploaded_by_user_id NN, filename VARCHAR(255) NN,
content_type VARCHAR(128), size_bytes BIGINT, storage_backend VARCHAR(32) NN default 'azure' (provider id label),
provider_file_id VARCHAR(128) NULL, status VARCHAR(16) NN CHECK pending|uploaded|ready|failed, error_code VARCHAR(64)
NULL (file_too_large, upload_failed, storage_limit_exceeded, vector_store_failed, indexing_failed, upload_abandoned),
attachment_kind VARCHAR(16) NN CHECK document|image, for_file_search BOOL false, for_code_interpreter BOOL false,
doc_summary TEXT NULL (never), img_thumbnail BLOB NULL (webp), img_thumbnail_width INT NULL, img_thumbnail_height INT NULL,
summary_model NULL, summary_updated_at NULL, cleanup_status VARCHAR(16) NULL (pending|done|failed),
cleanup_attempts INT 0, last_cleanup_error TEXT NULL, cleanup_updated_at NULL, created_at NN, updated_at NN,
deleted_at NULL, secondary_file_id NULL, secondary_status VARCHAR(16) default 'not_attempted'
CHECK not_attempted|pending|uploaded|failed, secondary_provider_kind VARCHAR(32) NULL CHECK anthropic.
Indexes: (tenant_id, chat_id) WHERE deleted_at IS NULL; (cleanup_status) partial; idx_attachments_stale_upload
(status, cleanup_status, deleted_at, updated_at); UNIQUE (id, chat_id).

## message_attachments
PK (chat_id, message_id, attachment_id); tenant_id NN, created_at NN.
FK (message_id, chat_id) -> messages(id, chat_id) CASCADE; FK (attachment_id, chat_id) -> attachments(id, chat_id) CASCADE.
Indexes (tenant_id, chat_id), (attachment_id, chat_id).

## thread_summaries
id PK, tenant_id NN, chat_id UNIQUE FK cascade, summary_text TEXT, summarized_up_to_created_at NN,
summarized_up_to_message_id NN, token_estimate INT, created_at NN, updated_at NN.
Row created only on first successful commit; mutation dropping summary deletes row and clears is_compressed.

## chat_vector_stores
id PK, tenant_id NN, chat_id NN, vector_store_id VARCHAR(128) NULL (NULL = creation in progress),
provider VARCHAR(128) NN (storage backend label), file_count INT NN 0 CHECK >=0, created_at NN.
UNIQUE (tenant_id, chat_id). Creation protocol: insert placeholder (winner) -> provider create -> CAS set id;
loser polls (5 polls, exponential backoff) else 503; stale placeholder (>120s NULL) reclaimed.

## quota_usage  (owner col user_id)
id PK, tenant_id NN, user_id NN, period_type VARCHAR(16) (daily|monthly), period_start DATE, bucket VARCHAR(32)
(total|tier:premium), spent_credits_micro BIGINT 0, reserved_credits_micro BIGINT 0, calls INT 0, input_tokens
BIGINT 0, output_tokens BIGINT 0, file_search_calls INT 0, web_search_calls INT 0, code_interpreter_calls INT 0,
rag_retrieval_calls INT 0, image_inputs INT 0, image_upload_bytes BIGINT 0, updated_at.
UNIQUE (tenant_id, user_id, period_type, period_start, bucket).
Reserve: total.reserved += R (each period); premium tier also tier:premium.reserved += R.
Settle: total: reserved -= R, spent += actual_credits, calls += 1, input/output tokens += actual (only actual
settlements), web_search_calls/code_interpreter_calls += counts (actual & estimated, not released);
premium: reserved -= R, spent += actual, calls += 1.

## message_reactions  (owner col user_id)
id PK, message_id NN FK messages CASCADE, user_id NN, tenant_id NN, reaction VARCHAR(16) CHECK like|dislike,
created_at NN. UNIQUE (message_id, user_id).

## Outbox (platform toolkit-db outbox tables via platform migrations)
Queues (defaults): usage `mini-chat.usage_snapshot`, audit `mini-chat.audit`, attachment cleanup
`mini-chat.attachment_cleanup`, chat cleanup `mini-chat.chat_cleanup`, thread summary `mini-chat.thread_summary`;
`outbox.num_partitions` 4.
