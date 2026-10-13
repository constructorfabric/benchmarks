# mini-chat-streaming Specification

## Purpose
SSE streaming of chat turns: send pipeline, idempotency, turn lifecycle, context assembly, tools and knowledge search.

## Requirements

### Requirement: Send message streams a response correlated by request id
`POST /v1/chats/{id}/messages:stream` with `{content, request_id?, attachment_ids?, web_search?:{enabled}}` SHALL respond with `text/event-stream` (`Cache-Control: no-cache`). The first event is `stream_started {request_id, message_id, is_new_turn, thread_summary_applied?}`; `request_id` is the client value (any UUID version) or a server-generated v4. Deltas SHALL be relayed as they arrive from the provider (no buffering, bounded channel of `streaming.sse_channel_capacity`).

#### Scenario: Normal turn
- **WHEN** a message is sent and the provider streams two text deltas and `response.completed`
- **THEN** the client receives `stream_started`, two `delta {type:text}` events and `done`, in that order

### Requirement: Preflight validation before any provider call
Before opening the stream and before any provider request the gear SHALL validate: non-empty trimmed content (`EMPTY_CONTENT`), chat exists (404), chat model still in catalog (`INVALID_MODEL`), idempotency, parallel turn, `disable_web_search` (400 `failed_precondition` `web_search/FEATURE_DISABLED`), quota cascade (429 `resource_exhausted` with subject `tokens`/`web_search`/`code_interpreter`), `INPUT_TOO_LONG` (message estimate above `max_input_tokens`), image guards (`disable_images` → 400 `failed_precondition` `images/FEATURE_DISABLED`, `TOO_MANY_IMAGES`, `VISION_NOT_SUPPORTED` on the effective model), context budget (`CONTEXT_BUDGET_EXCEEDED`), and attachment ids (duplicates, foreign, not ready → 400 `field_violations[attachment].reason = invalid_attachment`). Attachment validation runs in the reserve transaction so a rejection leaves no reserve, message or turn.

#### Scenario: Not-ready attachment
- **WHEN** a message references an attachment whose status is not `ready`
- **THEN** the response is 400 `invalid_attachment`, no SSE stream is opened, no turn row exists and the provider is not called

### Requirement: Persistence on completion
On provider completion the finalization transaction SHALL insert the assistant message (content, `model` = effective model, token counts), CAS the turn `running → completed` with `assistant_message_id`, settle quota, and enqueue usage and audit outbox messages; `done` SHALL be sent only after the commit.

#### Scenario: Persisted assistant message
- **WHEN** a turn completes with usage 10/5
- **THEN** the assistant message has `input_tokens=10`, `output_tokens=5`, the turn state is `completed` and a usage outbox row exists

### Requirement: SSE event contract and ordering
Events SHALL follow `stream_started ping* (delta|tool)* citations? (done|error)`. `ping` (`{}`) only before the first delta/tool every `sse_ping_interval_seconds`; `tool {phase,name,details}` for `file_search`, `web_search`, `code_interpreter` from the provider events; `citations {items}` once before `done` when at least one citation maps (web: `url,title,snippet,span?`; file: `attachment_id`, filename as `title`, empty snippet); the connection closes after the terminal event.

#### Scenario: Tool and citation events
- **WHEN** the provider emits a web search call and a `url_citation` annotation
- **THEN** the client receives `tool start/done web_search` and one `citations` event with a `source: web` item before `done`

### Requirement: Done event exposes usage and quota outcome without internals
`done` SHALL carry `usage {input_tokens, output_tokens}`, `effective_model`, `selected_model`, `quota_decision` (`allow`|`downgrade`), `downgrade_from`/`downgrade_reason` only on downgrade, and `quota_warnings` on CAS-winning completed turns; it SHALL NOT carry provider ids, message id or request id.

#### Scenario: No provider identifiers
- **WHEN** the provider response id is `resp_abc123`
- **THEN** no SSE payload contains `resp_abc123`

### Requirement: Error event is terminal and sanitized
After the stream opened, failures SHALL be sent as one terminal `error {code, message}` (codes `provider_error`, `provider_timeout`, `rate_limited`, `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `unexpected_tool_use`, `message_persistence_failed`, `finalization_failed`, `stream_interrupted`). Messages SHALL replace provider ids (`resp_`, `chatcmpl-`, `cmpl-`, `msg_`; `file-`, `file_`, `assistant-`, `vs_` with ≥12 alphanumerics) with `[provider_id]`, URLs with `[url]`, `sk-…` keys and Bearer tokens with `[credential]`.

#### Scenario: Provider failure
- **WHEN** the provider sends `response.failed` with message `bad file-abcdefghijklmnop`
- **THEN** the client gets `error {code: provider_error, message: "bad [provider_id]"}` and the turn is `failed` with `error_code = provider_error`

### Requirement: Idempotent replay
A completed, non-deleted `(chat_id, request_id)` SHALL be replayed as `stream_started{is_new_turn:false, message_id = stored assistant id}`, one `delta` with the stored content and `done` with stored usage, rebuilt `quota_decision`/`downgrade_from`, no `downgrade_reason`, no citations — without provider call, reserve, quota change, outbox or audit.

#### Scenario: Replay has no side effects
- **WHEN** a completed request id is resent
- **THEN** the mock provider receives no request and `quota_usage` and outbox rows are unchanged

### Requirement: Request id conflicts and parallel turn guard
A known request id in `running`, `failed`, `cancelled` or soft-deleted SHALL be rejected with 409 `aborted` `context.reason = request_id_conflict`. Idempotency SHALL be checked before the parallel-turn guard; any other running turn in the chat SHALL be rejected with 409 `turn_already_running`, including the insert race on the unique running-turn index. A new turn SHALL be accepted after the previous one is terminal.

#### Scenario: Replay while another turn runs
- **WHEN** turn 1 completed, turn 2 is running and turn 1's request id is resent
- **THEN** the replay of turn 1 is served (not 409)

### Requirement: Turn lifecycle and turn status
`chat_turns.state` SHALL move only `running → completed|failed|cancelled` via CAS. `GET /v1/chats/{id}/turns/{request_id}` SHALL return `{request_id, state (running|done|error|cancelled), error_code?, assistant_message_id?, updated_at}`; soft-deleted or foreign turns return 404 (turn resource type). On client disconnect the turn SHALL become `cancelled`, persisting non-empty partial text as the assistant message (`assistant_message_id` set) and settling estimated; with empty text no assistant message is written.

#### Scenario: Cancelled with partial content
- **WHEN** the client disconnects after receiving one delta
- **THEN** the turn status becomes `cancelled` with `assistant_message_id` whose content is the partial text

### Requirement: Context assembly
The provider request SHALL contain the catalog `system_prompt` plus tool guards as instructions, the thread summary (as one user message with the preamble) when it fits, recent non-compressed messages (up to `context.recent_messages_limit`, truncated newest-first by whole turns within the budget `min(max_input_tokens, context_window − max_output_tokens_applied) − surcharges − fixed_overhead_tokens`), the current user message (with `input_image` items for image attachments), `max_output_tokens = min(catalog, streaming.max_output_tokens)`, `stream: true`, `user = tenant_hex+user_hex`, `metadata {tenant_id,user_id,chat_id,request_type:chat,feature}` and `max_tool_calls` (OpenAI Responses adapter).

#### Scenario: History included
- **WHEN** a second message is sent in a chat with one completed turn
- **THEN** the provider request input contains the first user message, the first answer and the new message in order

### Requirement: Tool availability
`file_search {vector_store_ids:[store], max_num_results}` SHALL be sent only when the chat has a ready, non-deleted document, the effective model's `tool_support.file_search` is true and `disable_file_search` is off; `web_search {search_context_size}` only when requested and supported; `code_interpreter {container:{type:auto,file_ids}}` with `include:["code_interpreter_call.outputs"]` only with ready XLSX attachments, support and kill switch off. Guards are appended to the instructions for sent tools.

#### Scenario: No documents, no file_search
- **WHEN** a chat has no attachments
- **THEN** the provider request has no `file_search` tool

### Requirement: Web search accounting and limits
Daily web search usage SHALL be checked at preflight only when the tool is sent (`quota.web_search_daily_quota`, 429 subject `web_search`); the per-turn limit `quota.web_search_max_calls_per_message` SHALL be enforced on tool start events, failing the turn with `web_search_calls_exceeded` and estimated settlement; completed web search calls SHALL be added to `quota_usage.web_search_calls` (bucket `total`, daily and monthly) and reported in the usage event.

#### Scenario: Per-turn limit exceeded
- **WHEN** the provider starts three web searches with the limit 2
- **THEN** the stream ends with `error {code: web_search_calls_exceeded}` and the turn is `failed`

### Requirement: Knowledge search
When `knowledge_search.enabled` is true and the configured provider (`knowledge_search.provider_id`, kind `openai_responses` or `anthropic_messages`, with an `api_version`) is usable, and no chat-level `file_search` tool is sent, the provider request SHALL include the `search_knowledge` function tool and the knowledge search guard. Each `search_knowledge` call SHALL be executed against `knowledge_search.vector_store_id` through OAGW (`top_k` and `max_chunk_chars` bounds), fed back as a `function_call_output` and the provider request reissued; calls beyond `max_calls_per_message` SHALL receive a limit message instead of a search, more than `max_calls_per_message + 2` iterations SHALL fail the turn with `agentic_iterations_exceeded`, and a function call for a tool that was not offered SHALL fail the turn with `unexpected_tool_use`. Successful retrievals count toward `file_search_completed_count`.

#### Scenario: Knowledge search round trip
- **WHEN** knowledge search is enabled and the model calls `search_knowledge`
- **THEN** the gear searches the configured vector store, sends the results back as `function_call_output` and streams the model's final answer ending with `done`

#### Scenario: Unknown tool requested
- **WHEN** the model emits a function call for a tool that was not offered
- **THEN** the stream ends with `error {code: unexpected_tool_use}` and the turn is `failed`
