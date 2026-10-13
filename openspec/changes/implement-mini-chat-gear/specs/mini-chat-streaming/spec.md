## ADDED Requirements

### Requirement: Send message endpoint
`POST /v1/chats/{id}/messages:stream` SHALL accept `{content, request_id?, attachment_ids?, web_search?: {enabled}}` and respond with `text/event-stream` (`Cache-Control: no-cache`). An omitted `request_id` SHALL be generated (UUID v4); any UUID version is accepted. All validation, authorization and quota failures before the stream opens SHALL be JSON Problem responses and SHALL NOT open an SSE stream or call the provider.

#### Scenario: Successful stream
- **WHEN** a message is sent to an existing chat and the provider streams text then `response.completed`
- **THEN** the client receives `stream_started` (with the resolved `request_id`, pre-allocated `message_id`, `is_new_turn: true`), `delta` events, and `done`

### Requirement: Preflight validation order
Before any provider call the service SHALL, in order: authorize `send_message` and load the chat (404 if missing/foreign/deleted); run the idempotency check (replay of a completed turn, else 409), then the parallel-turn check; resolve the chat model (400 `INVALID_MODEL` if removed from the catalog); reject empty/whitespace `content` (400 `EMPTY_CONTENT`); reject duplicate or more than `rag.max_documents_per_chat + rag.max_images_per_message` `attachment_ids` (400 `field_violations[attachment].reason = invalid_attachment`); reject `web_search.enabled` under `disable_web_search` (400 `failed_precondition`, `violations[{subject: web_search, type: FEATURE_DISABLED}]`); reject more than `rag.max_images_per_message` images (400 `out_of_range` `TOO_MANY_IMAGES`); run the quota preflight (429); reject a message estimate above `max_input_tokens` (400 `out_of_range` `INPUT_TOO_LONG`); reject images under `disable_images` (400 `failed_precondition` subject `images`) and images on an effective model without `VISION_INPUT` (400 `VISION_NOT_SUPPORTED`); assemble context (400 `CONTEXT_BUDGET_EXCEEDED`); then in one transaction reserve quota, insert the user message, validate and link `attachment_ids` (each must be in the chat, uploaded by the caller, non-deleted and `ready`, else 400 `invalid_attachment` and rollback) and insert the `running` turn.

#### Scenario: Empty content
- **WHEN** `content` is `"   "`
- **THEN** the response is 400 with `EMPTY_CONTENT` and the provider receives no request

#### Scenario: Not-ready attachment
- **WHEN** `attachment_ids` names an attachment of another chat
- **THEN** the response is 400 with `invalid_attachment`, no user message, turn or reserve remains

### Requirement: SSE event contract
The stream SHALL follow `stream_started ping* (delta | tool)* citations? (done | error)` and close after the terminal event. The stored assistant content SHALL be exactly the concatenated `delta` text (empty when the provider sent none). `delta` is `{type: "text", content}`. `tool` is `{phase: start|done, name, details}` mapped from `response.file_search_call.searching|completed` (`details.files_searched` on done), `response.web_search_call.searching|completed`, `response.code_interpreter_call.in_progress` (start) and `response.output_item.done` with a `code_interpreter_call` item (done, `details.output` = joined logs capped at 8192 chars + `...[truncated]`). `citations` is `{items: [...]}` sent once before `done` when the completed response has mapped annotations: web `{source: "web", title, url, snippet, span?}`, file `{source: "file", title: filename, attachment_id, snippet: ""}` (unknown or deleted files omitted). `ping` (`data: {}`) SHALL be sent every `streaming.sse_ping_interval_seconds` only before the first `delta`/`tool`. The provider event name SHALL be taken from the SSE `event:` line, or from the data `type` field when the line is missing or `message`.

#### Scenario: Tool and citation events
- **WHEN** the provider emits `response.web_search_call.searching`, `response.web_search_call.completed`, text, and a completed response with a `url_citation` annotation
- **THEN** the client receives `tool` start, `tool` done, `delta`, `citations` with a web item, then `done`

### Requirement: Done event
`done` SHALL contain `usage: {input_tokens, output_tokens}`, `effective_model`, `selected_model`, `quota_decision` (`allow`|`downgrade`), `downgrade_from` and `downgrade_reason` only on downgrade, and `quota_warnings` (`[{tier, period, remaining_percentage, warning, exhausted, next_reset?}]`) on CAS-winning completions. It SHALL NOT contain provider identifiers (no `response.id`). `done` SHALL be sent only after the finalization transaction committed the turn as `completed`.

#### Scenario: No internal ids
- **WHEN** a stream completes with provider response id `resp_abc123...`
- **THEN** no SSE payload contains that id, and `chat_turns.provider_response_id` stores it

### Requirement: Error event
After the stream opened, failures SHALL produce one terminal `event: error` with `{code, message}`: `provider_error` (non-429 error, `response.failed`, SSE `error` event, invalid response), `provider_timeout`, `rate_limited` (provider 429; message includes `Retry-After` seconds when numeric), `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `unexpected_tool_use`, `message_persistence_failed`, `finalization_failed`, `stream_interrupted`. The `message` SHALL be sanitized: provider ids (`resp_`, `chatcmpl-`, `cmpl-`, `msg_` + alnum; `file-`, `file_`, `assistant-`, `vs_` + at least 12 alnum) → `[provider_id]`, URLs → `[url]`, `sk-` keys (10+ alnum) and `Bearer` tokens → `[credential]`. The turn SHALL be `failed` with the same `error_code`.

#### Scenario: Provider failure sanitized
- **WHEN** the provider sends `response.failed` with message `"bad file file-abcdefghijklmnop at https://x.y/z"`
- **THEN** the client receives `error` with `code: provider_error` and message `"bad file [provider_id] at [url]"`

### Requirement: No buffering and bounded channels
The relay SHALL forward each provider event as it arrives over a bounded channel of capacity `streaming.sse_channel_capacity` (default 32, 16–64); the full response SHALL NOT be collected before relaying.

#### Scenario: First delta before completion
- **WHEN** the provider sends one delta and pauses before completing
- **THEN** the client receives the delta before the provider completes

### Requirement: Idempotent replay
A request whose `(chat_id, request_id)` matches a `completed`, non-deleted turn SHALL be replayed without provider call, quota change, outbox or audit event: `stream_started` (`is_new_turn: false`, persisted `message_id`), one `delta` with the full stored text, `done` with stored usage, rebuilt `quota_decision`/`downgrade_from` and no `downgrade_reason`/`quota_warnings`. A `running`, `failed`, `cancelled` or soft-deleted turn with that key SHALL return 409 `aborted` `context.reason = request_id_conflict`. The idempotency check SHALL run before the parallel-turn guard.

#### Scenario: Replay during another running turn
- **WHEN** turn A completed, turn B is running, and A's `request_id` is resent
- **THEN** A is replayed (not 409 `turn_already_running`)

### Requirement: Parallel turn guard
At most one `running` turn per chat SHALL exist (unique partial index). A new send while a turn runs SHALL return 409 `aborted` `context.reason = turn_already_running`, also when losing the insert race. After the running turn reaches a terminal state, a new send SHALL be accepted.

#### Scenario: Concurrent send
- **WHEN** a second message is sent while the first is streaming
- **THEN** the second gets 409 `turn_already_running`, and succeeds after the first completes

### Requirement: Turn lifecycle and status API
Turns SHALL transition `running → completed | failed | cancelled` via CAS (`WHERE state='running'`), terminal states immutable. A provider `response.completed` or `response.incomplete` SHALL complete the turn and persist the assistant message (possibly empty) in the same transaction. A client disconnect SHALL cancel the provider request and finalize `cancelled`, persisting a partial assistant message when text was received (else none). `GET /v1/chats/{id}/turns/{request_id}` SHALL return `{request_id, state: running|done|error|cancelled, error_code?, assistant_message_id?, updated_at}`; soft-deleted or foreign turns SHALL return 404 (turn type).

#### Scenario: Cancelled with partial content
- **WHEN** the client disconnects after receiving text
- **THEN** the turn becomes `cancelled` with an `assistant_message_id` whose content is the received text

#### Scenario: Failed turn status
- **WHEN** the provider fails the stream
- **THEN** turn status is `error` with `error_code: provider_error` and no `assistant_message_id`

### Requirement: Provider request shape
The OpenAI Responses adapter SHALL `POST {alias}{api_path}` through OAGW with `model` = effective model's `provider_model_id`, `stream: true`, `instructions` = system prompt + tool guards, `input` = [thread summary as user message with preamble?, recent messages, current user message (content array with `input_text` and `input_image{file_id}` items when images)], `max_output_tokens` = `max_output_tokens_applied`, `user` = `{tenant_hex}{user_hex}`, `metadata` `{tenant_id, user_id, chat_id, request_type: "chat", feature}`, `store: false`, catalog `api_params` that are set, and `tools` per the tool rules (`file_search` with `vector_store_ids` and `max_num_results`; `web_search` with `search_context_size`; `code_interpreter` with `container {type: auto, file_ids}` plus `include: ["code_interpreter_call.outputs"]`), and `max_tool_calls` when tools are present.

#### Scenario: Basic request
- **WHEN** a text-only message is sent on `gpt-4.1`
- **THEN** the provider receives model `gpt-4.1`, `stream: true`, `max_output_tokens` 32768, a 64-char `user`, and no `tools`

### Requirement: Canonical REST error contract
All REST errors SHALL be RFC 9457 Problems (`type`, `title`, `status`, `detail`, `instance`, `trace_id`, `context`) without a top-level `code`, using the canonical categories and statuses of ADR-0004 (e.g. `aborted` 409 with `context.reason`, `failed_precondition` 400 with `context.violations`, `resource_exhausted` 429 with `violations[{subject, description: "quota_exceeded"}]`, `service_unavailable` 503 with `Retry-After`). Non-UUID path parameters SHALL return 400.

#### Scenario: Conflict shape
- **WHEN** a second stream is opened while a turn runs
- **THEN** the 409 body has `context.reason = turn_already_running` and no `code` field
