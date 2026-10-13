# mini-chat-context Specification

## Purpose
Context plan assembly, context budget and truncation, thread summaries and knowledge search.

## Requirements

### Requirement: Context plan assembly
The request context SHALL be: system prompt (catalog `system_prompt`) + tool guards (`context.file_search_guard` / `context.web_search_guard` when those tools are sent) as `instructions`; the thread summary (if any) as one user message prefixed with the summary preamble; recent non-deleted, non-compressed messages after the summary frontier and up to the snapshot boundary, newest `context.recent_messages_limit` (default 10) in chronological order `(created_at, id)`; then the current user message with its images.

#### Scenario: History included
- **WHEN** a second message is sent in a chat
- **THEN** the provider input contains the first user and assistant messages before the new user message

### Requirement: Context budget
`token_budget = min(max_input_tokens (0 = none), context_window - max_output_tokens_applied) - surcharges of sent tools - fixed_overhead_tokens` of the effective model. If `max_output_tokens_applied >= context_window` or the mandatory items (instructions + current message + images) exceed it, the request SHALL be rejected with 400 `out_of_range` `CONTEXT_BUDGET_EXCEEDED` before any provider call. A current message whose estimate exceeds `max_input_tokens` SHALL be rejected with `INPUT_TOO_LONG`. The summary SHALL be dropped if it does not fit; recent messages SHALL be kept newest-first while they fit and the kept range SHALL not start with an assistant message; `messages_truncated` SHALL be set when one was dropped.

#### Scenario: Oversized message on small model
- **WHEN** a message larger than the model's `max_input_tokens` is sent
- **THEN** the response is 400 `INPUT_TOO_LONG` and no provider request is made

#### Scenario: Truncation
- **WHEN** history exceeds the budget
- **THEN** the oldest whole turns are omitted from the provider input

### Requirement: Thread summary
When `thread_summary_worker.enabled`, a completed turn whose assembled context reached `compression_threshold_pct` of `min(max_input_tokens, context_window - max_output_tokens_applied)` without an existing summary, or whose context was truncated, SHALL enqueue in its finalization transaction a thread-summary outbox message with the base frontier and a frozen target frontier (latest non-deleted message before the turn; none → no enqueue). The handler SHALL call the summary model (non-streaming, `request_type = summary`) with the summary prompt over the frozen range, store the `<summary>` content in `thread_summaries` with CAS on the base frontier, mark the range `is_compressed`, and enqueue a system usage event (`billing_outcome: system_task`, `settlement_method: none`, `requester_type: system`). Provider failure SHALL keep the previous summary and retry up to `max_attempts`. A retry/edit/delete whose turn is covered by the summary SHALL delete the summary and clear `is_compressed`.

#### Scenario: Summary applied
- **WHEN** a summary exists for the chat
- **THEN** the next provider request carries the summary message and `stream_started.thread_summary_applied.token_estimate` is present

#### Scenario: Mutation invalidates summary
- **WHEN** the latest turn covered by the summary is deleted
- **THEN** the `thread_summaries` row is removed and messages are no longer `is_compressed`

### Requirement: Knowledge search
When `knowledge_search.enabled` is true and, per request, the provider entry `knowledge_search.provider_id` is of kind `openai_responses` or `anthropic_messages`, has an upstream alias for the tenant and a non-empty `api_version`, and the retriever is configured, the request SHALL include a `search_knowledge` function tool (parameters `query`, optional `top_k`) and append `knowledge_search.guard` to the instructions; otherwise knowledge search SHALL be off for the request (with a warning). It SHALL NOT be offered when `file_search` is sent. Each `search_knowledge` call SHALL run a retrieval through OAGW (`POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version=...`, `max_num_results = min(top_k, knowledge_search.top_k)`, chunks trimmed to `max_chunk_chars`) and continue the conversation with the call and its `function_call_output`; after `max_calls_per_message` retrievals further calls SHALL get a "search limit reached" output; more than `max_calls_per_message + 2` iterations SHALL fail the turn with `agentic_iterations_exceeded`. A function call for any other name (or any function call while knowledge search is off) SHALL fail the turn with `unexpected_tool_use`. Successful retrievals increment `chat_turns.file_search_completed_count`; only the final iteration's usage is settled.

#### Scenario: Knowledge search loop
- **WHEN** knowledge search is enabled and the model calls `search_knowledge` once and then answers
- **THEN** the gear runs one vector store search, sends a second provider request containing the `function_call_output`, and the turn completes

#### Scenario: Unexpected tool use
- **WHEN** knowledge search is off and the provider returns a function call
- **THEN** the client receives `error` with `code: unexpected_tool_use` and the turn is `failed`
