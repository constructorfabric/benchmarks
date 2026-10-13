## ADDED Requirements

### Requirement: Send message streams a response
`POST /v1/chats/{id}/messages:stream` with `{content, request_id?, attachment_ids?, web_search?}` SHALL return `200 text/event-stream` (`Cache-Control: no-cache`) whose first event is `stream_started {request_id, message_id, is_new_turn: true, thread_summary_applied?}`. `request_id` is the client value (any UUID version) or a server-generated UUID v4; `message_id` is the pre-allocated assistant message id. Authorization uses action `send_message`.

#### Scenario: Server-generated request id
- **WHEN** a message is sent without `request_id`
- **THEN** `stream_started.request_id` is a UUID v4 and the persisted user and assistant messages carry it

### Requirement: Preflight validation before any provider call
Before opening the stream and before any provider call the service SHALL reject with a JSON `Problem` (no SSE): empty/whitespace `content` (400 `EMPTY_CONTENT`), malformed body (422) or JSON (400), unknown/foreign/deleted chat (404), removed chat model (400 `INVALID_MODEL`), `web_search.enabled` under `disable_web_search` (400 `failed_precondition` `FEATURE_DISABLED` subject `web_search`, checked before the quota cascade), quota exhaustion (429 `resource_exhausted`, subject `tokens`/`web_search`/`code_interpreter`), images under `disable_images` (400 `FEATURE_DISABLED` subject `images`), more than `rag.max_images_per_message` images (400 `out_of_range` `TOO_MANY_IMAGES`), image on a non-vision effective model (400 `VISION_NOT_SUPPORTED`), message above `max_input_tokens` (400 `out_of_range` `INPUT_TOO_LONG`), mandatory context over budget (400 `CONTEXT_BUDGET_EXCEEDED`), duplicate/foreign/not-ready/unknown `attachment_ids` or more than `max_documents_per_chat + max_images_per_message` ids (400 `invalid_attachment`). Attachment checks run inside the reserve transaction so a rejection leaves no reserve, message or turn.

#### Scenario: Empty content
- **WHEN** a message with content `"  "` is sent
- **THEN** the response is 400 JSON with `field_violations[content].reason = EMPTY_CONTENT` and the provider is not called

#### Scenario: Not-ready attachment
- **WHEN** a message references an attachment of another chat
- **THEN** the response is 400 `invalid_attachment`, no turn row exists and `quota_usage.reserved_credits_micro` is unchanged

### Requirement: SSE event contract and ordering
Streams SHALL follow `stream_started ping* (delta | tool)* citations? (done | error)`: `delta {type: "text", content}` relayed as each provider chunk arrives (no buffering, bounded channel `streaming.sse_channel_capacity`); `tool {phase: start|done, name, details}` for `file_search`, `web_search`, `code_interpreter`; at most one `citations {items}` after all deltas and only on a normally completed stream with mapped citations; `ping {}` every `sse_ping_interval_seconds` only before the first `delta`/`tool`; exactly one terminal `done` or `error`, after which the stream closes.

#### Scenario: Ordered events
- **WHEN** the provider streams two text deltas and completes
- **THEN** the client receives `stream_started`, two `delta` events in order, then `done`, and nothing after `done`

#### Scenario: Keepalive before content
- **WHEN** the provider is silent for longer than the ping interval before its first delta
- **THEN** the client receives at least one `ping` event with data `{}` before the first `delta`

### Requirement: Completion event
`done` SHALL carry `usage {input_tokens, output_tokens}` (token counts only), `effective_model`, `selected_model`, `quota_decision` (`allow`|`downgrade`), and when downgraded `downgrade_from` (= selected model) and `downgrade_reason` (`premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier`, `model_disabled`), plus `quota_warnings` on CAS-winning completions. It SHALL NOT contain provider ids, `request_id`, message ids, reserve or billing internals. `done` SHALL be sent only after the finalization transaction committed the turn as `completed`.

#### Scenario: Done payload
- **WHEN** a turn completes with provider usage 12/34
- **THEN** `done.usage` is `{input_tokens: 12, output_tokens: 34}`, `quota_decision` is `allow` and `effective_model == selected_model`

### Requirement: Error event
After the stream opens, failures SHALL be reported as a single terminal `error {code, message}` with stable codes (`provider_error`, `provider_timeout`, `rate_limited`, `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `message_persistence_failed`, `finalization_failed`, `stream_interrupted`, ...), and `message` SHALL be sanitized: provider ids (`resp_`, `chatcmpl-`, `cmpl-`, `msg_` + alnum; `file-`, `file_`, `assistant-`, `vs_` + ≥12 alnum) → `[provider_id]`, URLs → `[url]`, `sk-` keys (≥10 alnum) and `Bearer` tokens → `[credential]`. The turn SHALL be `failed` with that `error_code`.

#### Scenario: Provider failure
- **WHEN** the provider sends `response.failed` with message "bad thing resp_abc123 at https://x.y/z"
- **THEN** the client receives `error {code: "provider_error"}` whose message contains `[provider_id]` and `[url]` and not the raw values, and the turn status is `error` with `error_code = provider_error`

#### Scenario: Provider 429
- **WHEN** the provider returns HTTP 429
- **THEN** the client receives `error {code: "rate_limited"}`

### Requirement: Idempotency and replay
A request whose `(chat_id, request_id)` matches a completed, non-deleted turn SHALL be replayed: `stream_started {is_new_turn: false, message_id: <persisted id>}`, one `delta` with the full stored text, `done` rebuilt from stored data (usage from the assistant message, `quota_decision`/`downgrade_from` from stored models, no `downgrade_reason`, no `quota_warnings`, no citations), with no provider call, no quota change and no outbox/audit event. A `request_id` of a running, failed, cancelled or soft-deleted turn SHALL return 409 `aborted` `request_id_conflict`. The idempotency check SHALL run before the parallel-turn check.

#### Scenario: Replay is side-effect free
- **WHEN** a completed turn's request id is resent
- **THEN** the replay stream is returned, the provider receives no request and `quota_usage` and outbox rows are unchanged

#### Scenario: Replay during running turn
- **WHEN** a completed turn's request id is resent while another turn runs in the chat
- **THEN** the replay is served (not 409)

### Requirement: Parallel turn guard
A chat SHALL have at most one running turn; a new stream request while one is running SHALL return 409 `aborted` with `context.reason = turn_already_running` (also when losing the insert race). A new request SHALL be accepted once the previous turn is terminal.

#### Scenario: Concurrent send
- **WHEN** a second message is sent while the first stream is still running
- **THEN** the second request gets 409 `turn_already_running`

### Requirement: Turn lifecycle and status
Turns SHALL move `running → completed | failed | cancelled` only through CAS updates guarded by `state = 'running'`; terminal states are immutable. `GET /v1/chats/{id}/turns/{request_id}` SHALL return `{request_id, state: running|done|error|cancelled, error_code?, assistant_message_id?, updated_at}`; soft-deleted or foreign turns return 404 (turn resource type). A client disconnect SHALL cancel the provider request and finalize the turn `cancelled`, persisting non-empty partial text as the assistant message (then `assistant_message_id` is set) and sending no SSE event; empty partial text leaves no assistant message.

#### Scenario: Cancellation with partial content
- **WHEN** the client disconnects after receiving some deltas
- **THEN** the turn becomes `cancelled`, the partial text is stored as the assistant message and the turn status shows `assistant_message_id`

#### Scenario: Status of completed turn
- **WHEN** the turn status of a completed turn is read
- **THEN** `state` is `done` and `assistant_message_id` is the persisted assistant message id

### Requirement: Persistence on completion
On provider completion (`response.completed` or `response.incomplete`) the finalization transaction SHALL insert the assistant message (content, `model` = effective model, token counts incl. cache/reasoning, `provider_response_id`), CAS the turn to `completed` with `assistant_message_id`, settle quota and enqueue the usage and audit events; the user message, `message_attachments` and the running turn SHALL have been committed in the reserve transaction before the provider call.

#### Scenario: Completed turn rows
- **WHEN** a turn completes
- **THEN** `chat_turns.state = completed`, `assistant_message_id` references a `messages` row with role `assistant`, and the outbox contains one usage event for the turn

### Requirement: Canonical error mapping
REST errors SHALL be canonical `Problem` objects (`type`, `title`, `status`, `detail`, `context`) with the categories, statuses and machine-readable reasons of DESIGN §3.3 / ADR-0004 (404 not_found with `resource_type`, 400 invalid_argument with `field_violations`, 400 out_of_range, 400 failed_precondition with `violations`, 409 aborted with `reason`, 409 already_exists with `resource_name`, 429 resource_exhausted with `violations`, 403 permission_denied `AUTHZ_DENIED`, 503 service_unavailable with `Retry-After`, 500 internal), consistently across REST endpoints and the pre-stream phase of streaming endpoints.

#### Scenario: Non-UUID path
- **WHEN** `GET /v1/chats/not-a-uuid` is called
- **THEN** the response is 400 `invalid_argument`
