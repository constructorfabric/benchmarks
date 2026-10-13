# mini-chat-background Specification

## Purpose
Background work of the mini-chat gear: OAGW provisioning, outbox handlers (usage, audit, cleanup, thread summary), orphan watchdog and upload reaper.

## Requirements

### Requirement: Chat deletion cleanup
The chat-cleanup handler (`mini-chat.chat_cleanup`) SHALL, for a soft-deleted chat, delete each pending attachment's provider file (2xx/404 success → `done`, failures counted up to `cleanup_worker.max_attempts` → `failed`), and once no attachment is `pending` delete the chat vector store (2xx/404) and its `chat_vector_stores` row. A message for a non-deleted chat SHALL be rejected.

#### Scenario: Vector store removed after files
- **WHEN** a chat with one indexed document is deleted
- **THEN** the mock provider receives the file delete and then the vector store delete, and the `chat_vector_stores` row is removed

### Requirement: Thread summary
When `thread_summary_worker.enabled`, finalization of a completed turn SHALL enqueue a thread-summary task (`mini-chat.thread_summary`, payload with `system_request_id`, base and frozen target frontiers) when context assembly truncated messages, or no summary exists and the assembled tokens reach `compression_threshold_pct` of the input budget; the frozen target is the latest message not belonging to the turn. The handler SHALL call the summary model non-streaming, store the `<summary>` block in `thread_summaries` with CAS on the base frontier, mark the range `is_compressed`, and enqueue a system usage event (`billing_outcome = system_task`, `requester_type = system`). Provider failures keep the previous summary and retry up to `max_attempts`. Retry/edit/delete of a turn covered by the summary SHALL delete it and clear `is_compressed`.

#### Scenario: Summary used in next turn
- **WHEN** a summary exists for a chat
- **THEN** the next provider request contains the summary preamble message and `stream_started.thread_summary_applied.token_estimate` is present

### Requirement: Orphan watchdog
Every `orphan_watchdog.scan_interval_secs` the watchdog SHALL CAS stale running turns (`COALESCE(last_progress_at, started_at) <= now - timeout_secs`) to `failed`/`orphan_timeout`, settle estimated and enqueue usage (`aborted`/`estimated`) and audit events.

#### Scenario: Stale running turn
- **WHEN** a running turn's `last_progress_at` is older than the timeout
- **THEN** its status becomes `error` with `error_code = orphan_timeout`

### Requirement: Audit delivery
Turn finalization SHALL enqueue a `turn_completed`/`turn_failed` audit event and mutations `turn_retry`/`turn_edit`/`turn_delete` to `mini-chat.audit`; the handler delivers them to the audit plugin (bundled `static_audit` logs) and drops them when no plugin is registered.

#### Scenario: Audit outbox row on completion
- **WHEN** a turn completes
- **THEN** an audit outbox message with `event_type = turn_completed` is enqueued

### Requirement: Provisioning and bundled plugins
At start the gear SHALL exchange `client_credentials` for an S2S context and create OAGW upstreams (alias = `upstream_alias` or host) and routes for each provider entry; the bundled `static-mini-chat-model-policy-plugin` SHALL serve `model_catalog`, `kill_switches`, `default_standard_limits`, `default_premium_limits` (version 1) and `static-mini-chat-audit-plugin` SHALL log events.

#### Scenario: Requests reach the mock provider
- **WHEN** a provider entry points at `127.0.0.1:<port>` with `use_http: true`
- **THEN** a sent message produces a `POST {api_path}` request at the mock with the configured auth header
