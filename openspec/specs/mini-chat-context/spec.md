# mini-chat-context Specification

## Purpose

Context plan assembly and truncation, provider request shape, tool availability, web search reporting and thread summaries.

## Requirements

### Requirement: Context plan assembly
For each turn the service SHALL assemble, in order: the effective model's `system_prompt` plus tool guards (`context.file_search_guard` when `file_search` is sent, `context.web_search_guard` when `web_search` is sent) as `instructions`; the thread summary (if any) as one `user` message prefixed with the summary preamble; up to `context.recent_messages_limit` (default 10) recent non-deleted, non-compressed messages after the summary frontier and at or before the snapshot boundary, in `(created_at, id)` order; and the current user message with its images (`input_text` + `input_image {file_id}` items).

#### Scenario: History in request
- **WHEN** a second message is sent in a chat with one completed turn
- **THEN** the provider request `input` contains the previous user and assistant messages followed by the new user message

### Requirement: Context budget enforcement
The service SHALL compute `token_budget = min(max_input_tokens or ∞, context_window - max_output_tokens_applied) - surcharges of sent tools - fixed_overhead_tokens` for the effective model and reject with 400 `out_of_range` `CONTEXT_BUDGET_EXCEEDED` when the budget is non-positive or the mandatory items (instructions, current message, images) exceed it; the current message estimate above a non-zero `max_input_tokens` SHALL be rejected with 400 `INPUT_TOO_LONG`. Otherwise the summary is kept if it fits, recent messages are kept newest-first while they fit, and a leading assistant message is dropped so whole turns are removed; `messages_truncated` is set when any recent message was dropped.

#### Scenario: Truncation drops oldest turns
- **WHEN** the history does not fit the budget of a small-context model
- **THEN** the oldest turns are dropped from the provider request while the system prompt and current message are kept

### Requirement: Provider request shape
The OpenAI Responses request SHALL contain `model` (= `provider_model_id`), `input`, `instructions`, `stream: true`, `store: false`, `max_output_tokens` (= `max_output_tokens_applied`), `user` (`{tenant_hex}{user_hex}`, 64 chars), `metadata {tenant_id, user_id, chat_id, request_type: "chat", feature}`, optional `api_params` (temperature, top_p, ... only when set), `tools` only when non-empty with `max_tool_calls`, `include: ["code_interpreter_call.outputs"]` when code interpreter is sent.

#### Scenario: Metadata
- **WHEN** a plain message is sent
- **THEN** the request has `metadata.request_type = "chat"`, `metadata.feature = "none"`, no `tools`, and `user` of 64 hex characters

### Requirement: Tool availability and guidance
`web_search` (`{type: "web_search", search_context_size}`) SHALL be sent only with `web_search.enabled` and catalog `tool_support.web_search`; `file_search` only with ready documents, `tool_support.file_search` and `disable_file_search` off; `code_interpreter` only with ready XLSX, `tool_support.code_interpreter` and `disable_code_interpreter` off. Guards SHALL be appended only for tools that are sent.

#### Scenario: Web search tool
- **WHEN** a message with `web_search.enabled = true` is sent on a model supporting web search
- **THEN** the request includes the `web_search` tool and the instructions end with the web search guard

### Requirement: Web search reporting and citations
Provider `web_search_call.searching`/`completed` SHALL produce `tool` start/done events named `web_search`; completed calls SHALL be counted on the turn and in quota usage; `url_citation` annotations SHALL produce a `citations` event with `source: web`, `url`, `title`, `snippet`, `span`; `file_citation` annotations SHALL map provider file ids to `attachment_id` + filename (`snippet: ""`, no span) and drop unknown ids.

#### Scenario: Web citation
- **WHEN** the provider output contains a `url_citation` annotation
- **THEN** the client receives one `citations` event before `done` with a `source: "web"` item carrying the URL

### Requirement: Thread summary lifecycle
When `thread_summary_worker.enabled`, a completed turn whose context was truncated, or whose assembled estimate reached `compression_threshold_pct` of `min(max_input_tokens, context_window - max_output_tokens_applied)` with no existing summary, SHALL enqueue a thread-summary outbox message (frozen target = latest message before the causing turn) in the finalization transaction. The handler SHALL call the summary model (non-streaming, `request_type: "summary"`), parse the `<summary>` block, and in one CAS-guarded transaction upsert `thread_summaries`, mark the summarized messages `is_compressed` and enqueue a `system_task` usage event; provider failures SHALL keep the previous summary and retry up to `max_attempts`. Retry/edit/delete of a turn covered by the summary SHALL delete the summary and clear `is_compressed`. Later turns SHALL include the summary and report `thread_summary_applied` in `stream_started`.

#### Scenario: Summary generated
- **WHEN** a small-context chat crosses the threshold
- **THEN** a `thread_summaries` row is created, older messages get `is_compressed = true` and the next turn's `stream_started` has `thread_summary_applied`
