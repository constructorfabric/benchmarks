# mini-chat-streaming Specification

## Purpose
Defines the `mini-chat` gear's send-message SSE stream, SSE event contract, idempotency/replay, parallel-turn guard, turn lifecycle, turn status and mutations (retry/edit/delete), context assembly, provider request shape, web search/tools (source: `gears/mini-chat/docs/`). Introduced by change `implement-mini-chat-gear`.

## Requirements

### Requirement: Send message stream
`POST /mini-chat/v1/chats/{id}/messages:stream` with `{content, request_id?, attachment_ids?, web_search?}` SHALL return `text/event-stream` (Cache-Control no-cache). `request_id` is any UUID; when omitted the server generates a UUID v4. Preflight failures (validation, authorization, quota, context budget, provider resolution) SHALL be JSON `Problem` responses and SHALL NOT open a stream. Every provider event SHALL be relayed as soon as it is received (no buffering; bounded channel of `streaming.sse_channel_capacity`).

#### Scenario: Normal stream
- **WHEN** a valid message is sent and the provider streams two text deltas and `response.completed`
- **THEN** the client receives `stream_started` (request_id, message_id, is_new_turn=true), two `delta` (`{type:"text",content}`), then `done`

#### Scenario: Empty content
- **WHEN** content is empty or whitespace
- **THEN** 400 `invalid_argument` with `field_violations[content].reason = EMPTY_CONTENT` and no provider call

### Requirement: Preflight order and validation
Preflight SHALL run in this order: chat lookup (404), idempotency check, parallel-turn guard, web-search kill switch (400 `failed_precondition` FEATURE_DISABLED subject `web_search`), quota cascade (429), `INPUT_TOO_LONG` (400 `out_of_range` when the message estimate exceeds the effective model's `max_input_tokens`), image guards (`disable_images` → 400 FEATURE_DISABLED subject `images`; more than `rag.max_images_per_message` → 400 `TOO_MANY_IMAGES`; effective model without `VISION_INPUT` → 400 `VISION_NOT_SUPPORTED`), context assembly (400 `CONTEXT_BUDGET_EXCEEDED`), provider resolution; then one reserve transaction (quota reserve + re-check, user message, `message_attachments`, `chat_turns` running row, `chats.updated_at`). Attachment ids that are duplicate, foreign, other-chat, not ready or too many SHALL be rejected with 400 `field_violations[attachment].reason = invalid_attachment` and roll back the reserve.

#### Scenario: Not-ready attachment
- **WHEN** a message references an attachment in status `uploaded`
- **THEN** 400 `invalid_attachment`, no turn row, no quota reserve, no provider call

### Requirement: SSE event contract
Streams SHALL follow `stream_started ping* (delta|tool)* citations? (done|error)`. `ping` (`{}`) SHALL be sent every `sse_ping_interval_seconds` only before the first delta/tool event. `tool` events carry `{phase:start|done, name, details}`. `citations` is sent once before `done` on a completed stream with mapped citations (web: url/title/snippet/span; file: attachment_id, title=filename, snippet ""). `done` SHALL carry `usage{input_tokens,output_tokens}`, `effective_model`, `selected_model`, `quota_decision` (`allow|downgrade`), `downgrade_from`/`downgrade_reason` only on downgrade, and `quota_warnings` on CAS-winning completed turns; it SHALL NOT contain provider ids or `request_id`. `error` carries `{code, message}` with a sanitized message and is terminal; the stream closes right after the terminal event.

#### Scenario: Provider failure mid-stream
- **WHEN** the provider sends `response.failed` with message containing `resp_abc123def456ghi` and `sk-abcdefghijk123`
- **THEN** the client receives `error{code:"provider_error"}` whose message has `[provider_id]` and `[credential]` in place of the ids, and the turn is `failed`

#### Scenario: Provider 429
- **WHEN** the provider responds HTTP 429
- **THEN** the client receives `error{code:"rate_limited"}`

### Requirement: Idempotent replay
A request whose `(chat_id, request_id)` matches a completed non-deleted turn SHALL be replayed: `stream_started(is_new_turn=false, message_id = persisted id)`, one `delta` with the full stored text, `done` with stored usage and models (no `downgrade_reason`, no `quota_warnings`), without provider call, quota change, or outbox enqueue. A match on a running, failed, cancelled or soft-deleted turn SHALL return 409 `aborted` with `context.reason = request_id_conflict`. The idempotency check SHALL run before the parallel-turn guard.

#### Scenario: Replay while another turn runs
- **WHEN** turn A completed, turn B is running, and A's request_id is resent
- **THEN** A is replayed (not 409 turn_already_running)

### Requirement: Parallel turn guard
At most one running turn per chat SHALL exist (enforced by a partial unique index). A new send while a turn is running SHALL return 409 `aborted` `context.reason = turn_already_running`; once the previous turn is terminal a new turn is accepted.

#### Scenario: Concurrent send
- **WHEN** a second message is sent while the first is still streaming
- **THEN** 409 `turn_already_running`

### Requirement: Turn lifecycle and finalization
Turns SHALL move `running → completed|failed|cancelled` only through a CAS update `WHERE state='running'`. Completed (including provider `incomplete`) persists the assistant message (possibly empty) and sets `assistant_message_id`; failed sets `error_code`; a client disconnect cancels the provider request and finalizes `cancelled`, persisting non-empty partial text as the assistant message (else `assistant_message_id` null). Terminal SSE events are emitted only after the finalization commit. `GET /chats/{id}/turns/{request_id}` SHALL return `{request_id, state: running|done|error|cancelled, error_code?, assistant_message_id?, updated_at}`; soft-deleted or foreign turns return 404 (turn resource type).

#### Scenario: Cancelled with partial content
- **WHEN** the client disconnects after receiving one delta
- **THEN** the turn becomes `cancelled`, its assistant message holds the partial text, and turn status reports `cancelled` with `assistant_message_id`

### Requirement: Turn mutations
Retry (`POST .../turns/{rid}/retry`), edit (`PATCH .../turns/{rid}` `{content}`) and delete (`DELETE .../turns/{rid}`) SHALL apply only to the latest non-deleted turn: running target → 400 `failed_precondition` `turn_state`/STATE (checked first); not latest or deleted → 409 `NOT_LATEST_TURN`; turn of another requester → 403; lost race on the running index → 409 `GENERATION_IN_PROGRESS`. Retry/edit run the full send preflight (quota, kill switches, image guards) before committing; the commit soft-deletes the old turn and its messages, sets `replaced_by_request_id`, inserts a new user message (retry copies content; edit uses new content) with copied non-deleted attachment links, inserts a new running turn with a server-generated request_id, deletes a thread summary covering the turn, bumps `chats.updated_at` and enqueues a `turn_retry`/`turn_edit` audit event; the response is the same SSE contract. Delete soft-deletes the turn and its messages, enqueues `turn_delete`, returns 204. Setup failures after the commit mark the new turn failed (`context_length_exceeded`, `quota_exceeded`, `turn_setup_failed`).

#### Scenario: Retry creates a new request id
- **WHEN** the latest completed turn is retried
- **THEN** `stream_started.request_id` differs from the original, the old turn status is 404, and the old request_id resent to `messages:stream` gets 409 `request_id_conflict`

### Requirement: Context assembly
The provider request SHALL contain, in order: system prompt (catalog `system_prompt`) with tool guards appended when `file_search`/`web_search` tools are sent; the thread summary as a user-role message prefixed with the summary preamble; up to `context.recent_messages_limit` (default 10) recent non-compressed, non-deleted messages after the summary frontier in `(created_at,id)` order; the current user message with `input_image` items for image attachments. Truncation drops oldest messages first (never leaving an assistant message first), then the summary; system prompt and user message are never truncated (`CONTEXT_BUDGET_EXCEEDED` when they alone exceed the budget). Budget = `min(max_input_tokens or ∞, context_window − max_output_tokens_applied) − surcharges − fixed_overhead_tokens`.

#### Scenario: History included
- **WHEN** a second message is sent in a chat
- **THEN** the provider request input contains the first user message, the first assistant answer and the new message in order

### Requirement: Provider request shape (OpenAI Responses)
Requests SHALL be `POST {alias}{api_path}` with `model` = `provider_model_id`, `stream: true`, `input`, `instructions` (system prompt), `max_output_tokens` = applied value, `user` = tenant hex + user hex (64 chars), `metadata{tenant_id,user_id,chat_id,request_type:"chat",feature}`, `store:false`, `max_tool_calls`, `tools` (`file_search` with chat vector store and `max_num_results` when the chat has ready documents and file search is supported/not killed; `web_search` with `search_context_size` when requested and supported; `code_interpreter` with container file ids and `include:["code_interpreter_call.outputs"]` when ready XLSX exists), catalog `api_params` when set.

#### Scenario: Web search tool
- **WHEN** `web_search.enabled=true` on a model with `tool_support.web_search`
- **THEN** the provider request contains a `web_search` tool and the system prompt contains the web search guard

### Requirement: Web search accounting
Web search tool events SHALL be relayed as `tool` events, web citations relayed, `chat_turns.web_search_completed_count` and `quota_usage.web_search_calls` incremented, per-message limit `quota.web_search_max_calls_per_message` enforced mid-turn (`web_search_calls_exceeded`, turn failed, estimated settlement), daily quota `quota.web_search_daily_quota` checked at preflight (429 subject `web_search`), and `disable_web_search` rejecting `enabled=true` requests with 400 FEATURE_DISABLED.

#### Scenario: Per-message limit exceeded
- **WHEN** the provider starts three web_search calls with limit 2
- **THEN** the stream ends with `error{code:"web_search_calls_exceeded"}` and the turn is failed
