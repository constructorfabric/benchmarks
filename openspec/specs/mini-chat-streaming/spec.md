# mini-chat-streaming Specification

## Purpose
Send-message streaming pipeline of the mini-chat gear: preflight, context assembly, provider streaming through OAGW, the SSE event contract, idempotent replay, the parallel-turn guard and finalization on completion, failure or disconnect.

## Requirements

### Requirement: Send message streams SSE correlated by request id
`POST /v1/chats/{id}/messages:stream` with `{content, request_id?, attachment_ids?, web_search?}` SHALL respond with `text/event-stream` whose first event is `stream_started {request_id, message_id, is_new_turn, thread_summary_applied?}` (server-generated UUID v4 `request_id` when omitted; pre-allocated assistant `message_id`), followed by `ping* (delta|tool)* citations? (done|error)` and then close.

#### Scenario: Happy path
- **WHEN** a user sends `"hello"` and the provider streams `"Hi"`, `" there"` and `response.completed` with usage
- **THEN** the client receives `stream_started` (is_new_turn true), two `delta {type:"text"}` events, and `done {usage, effective_model, selected_model, quota_decision:"allow", quota_warnings}`; the persisted assistant message id equals `stream_started.message_id`

### Requirement: Streaming is never buffered
Provider events SHALL be relayed one by one through a bounded channel (`streaming.sse_channel_capacity`, 16–64) as they arrive; the relay MUST NOT wait for the provider to finish.

#### Scenario: First delta before provider completion
- **WHEN** the provider sends one delta and then pauses
- **THEN** the client receives that delta before the provider sends its terminal event

### Requirement: Preflight validation before any provider call
Before any provider call and before opening a stream the gear SHALL validate: empty/whitespace content (400 `EMPTY_CONTENT`), chat model removed from catalog (400 `INVALID_MODEL`), web search kill switch (400 `failed_precondition`, `web_search`/`FEATURE_DISABLED`), quota (429 `resource_exhausted`, scope `tokens`/`web_search`/`code_interpreter`), message above `max_input_tokens` (400 `out_of_range` `INPUT_TOO_LONG`), images: count (400 `TOO_MANY_IMAGES`), `disable_images` (400 `images`/`FEATURE_DISABLED`), vision on the effective model (400 `VISION_NOT_SUPPORTED`), mandatory context over budget (400 `CONTEXT_BUDGET_EXCEEDED`), and attachment ids (duplicate, foreign, other chat, not ready, too many → 400 `field_violations[attachment].reason = invalid_attachment`, validated inside the reserve transaction so the reserve and user message roll back).

#### Scenario: Not-ready attachment
- **WHEN** a message references an attachment in another chat
- **THEN** the response is 400 `invalid_attachment`, no provider request is made, no turn or message is persisted and the quota reservation is unchanged

### Requirement: Context window budget
The input budget SHALL be `min(max_input_tokens, context_window − max_output_tokens_applied) − surcharges of tools in the request − fixed_overhead_tokens` of the effective model; the current message alone above `max_input_tokens` is `INPUT_TOO_LONG`; mandatory items (system prompt + guards + current message + images) above the budget are `CONTEXT_BUDGET_EXCEEDED`; otherwise the thread summary is dropped if it does not fit and recent messages are kept newest-first while they fit, never starting with an assistant message. Recent messages are the last `context.recent_messages_limit` non-deleted, non-compressed messages with a request id after the summary frontier, regardless of turn state: the user message of a failed or cancelled turn stays in the history (without an assistant reply), messages of deleted turns do not.

#### Scenario: History truncated
- **WHEN** the history does not fit the budget
- **THEN** the provider request contains the system prompt, the newest whole turns that fit and the current message, and the oldest turns are omitted

### Requirement: Context assembly and provider request
The provider request (OpenAI Responses adapter) SHALL contain `model` = effective `provider_model_id`, `instructions` = catalog system prompt plus tool guards for included tools, `input` = [thread summary as a user message with the fixed preamble, up to `context.recent_messages_limit` recent uncompressed messages, current user message with `input_image` file ids], `stream: true`, `max_output_tokens` = `min(catalog max_output_tokens, streaming.max_output_tokens)`, `user` = tenant and user ids in simple form (64 chars), `metadata {tenant_id,user_id,chat_id,request_type:"chat",feature}`, `tools` with `file_search` (chat has ready documents, model supports it, kill switch off; `vector_store_ids`, `max_num_results`), `web_search` (requested and supported; `search_context_size`), `code_interpreter` (ready XLSX, supported, kill switch off; `container.file_ids`, `include: ["code_interpreter_call.outputs"]`), and `max_tool_calls` when tools are present.

#### Scenario: Web search tool and guard
- **WHEN** a request enables web search on a model with `tool_support.web_search`
- **THEN** the provider request has a `web_search` tool and the instructions end with the web search guard text

### Requirement: Assistant message and usage persisted on completion
On provider completion the gear SHALL, in one transaction, insert the assistant message (content, effective model, token counts, provider response id), CAS the turn `running → completed` with `assistant_message_id`, settle quota and enqueue usage and audit outbox events; `done` is emitted only after commit.

#### Scenario: Turn completed
- **WHEN** the stream completes
- **THEN** `chat_turns.state = completed`, `assistant_message_id` is set, the message row has the provider usage, and `quota_usage` reserved is released and spent increased by the actual credits

### Requirement: SSE event contract
The gear SHALL map provider events: text deltas → `delta`, file_search/web_search searching → `tool {phase:start}`, completed → `tool {phase:done}` (`files_searched` for file search), code interpreter in_progress → start, `output_item.done` code interpreter → done with `output`, annotations → one `citations` event before `done` (web: `url`, `title`, `snippet`, `span`; file: `attachment_id`, `title` = filename, `snippet` ""; unknown/deleted files omitted), `response.incomplete` → `done`, `response.failed`/`error` → `error {code:"provider_error"}`, provider 429 → `rate_limited`, timeouts → `provider_timeout`. `ping {}` is sent every `sse_ping_interval_seconds` only before the first content event.

#### Scenario: Citations and tools
- **WHEN** the provider emits a web search call and an `output_text` annotation of type `url_citation`
- **THEN** the client receives `tool start`, `tool done`, deltas, then `citations` with a `source:"web"` item, then `done`

### Requirement: Done event exposes usage and quota outcome without internals
`done` SHALL contain `usage {input_tokens, output_tokens}` only, `effective_model`, `selected_model`, `quota_decision`, `downgrade_from`/`downgrade_reason` when downgraded, and `quota_warnings` on live completed turns; it MUST NOT contain provider response ids, `request_id` or message id.

#### Scenario: No provider identifiers
- **WHEN** a turn completes with provider response id `resp_abc123`
- **THEN** no SSE payload contains `resp_abc123`

### Requirement: Error event is terminal and sanitized
`event: error {code, message}` SHALL be the last event; `message` SHALL replace provider ids (`resp_`, `chatcmpl-`, `cmpl-`, `msg_` + alphanumerics; `file-`, `file_`, `assistant-`, `vs_` + ≥12 alphanumerics) with `[provider_id]`, URLs with `[url]`, `sk-` keys (≥10 chars) and `Bearer` tokens with `[credential]`. The turn is finalized `failed` with the error code before the event is sent.

#### Scenario: Provider failure
- **WHEN** the provider sends `response.failed` with message `"bad file file-abcdefghijkl1234 at https://x.y"`
- **THEN** the client gets `error {code:"provider_error", message:"bad file [provider_id] at [url]"}` and the turn is `failed` with `error_code = provider_error`

### Requirement: Idempotent replay
A request whose `(chat_id, request_id)` matches a completed, non-deleted turn SHALL be replayed as `stream_started {is_new_turn:false, message_id: persisted id}`, one `delta` with the full text, and `done` rebuilt from stored models (no `downgrade_reason`, no `quota_warnings`), without a provider call, quota change or outbox event; a running, failed, cancelled or soft-deleted turn returns 409 `aborted` `context.reason = request_id_conflict`. Replay/conflict SHALL be checked before the parallel-turn guard.

#### Scenario: Replay while another turn runs
- **WHEN** turn A completed and turn B is running in the same chat, and the client resends A's request id
- **THEN** A is replayed (not 409 `turn_already_running`)

### Requirement: One running turn per chat
A new stream into a chat with a running turn SHALL return 409 `aborted` `context.reason = turn_already_running` (also when the insert loses the unique-index race); once that turn is terminal a new turn is accepted.

#### Scenario: Parallel send
- **WHEN** a second message is sent while the first turn is still streaming
- **THEN** the second request gets 409 `turn_already_running`, and after the first turn completes a new send succeeds

### Requirement: Client disconnect cancels the turn
When the client disconnects before the terminal event, the provider request SHALL be cancelled and the turn finalized `cancelled` (non-empty partial text persisted as the assistant message, else `assistant_message_id = null`), settled with the estimated formula, without an SSE error.

#### Scenario: Disconnect mid-stream
- **WHEN** the client closes the connection after one delta
- **THEN** the turn becomes `cancelled`, a partial assistant message with that delta exists, and quota is settled `estimated`
