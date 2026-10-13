# mini-chat-background Specification

## Purpose
Defines the `mini-chat` gear's outbox handlers (usage, audit, attachment/chat cleanup, thread summary), orphan watchdog (source: `gears/mini-chat/docs/`). Introduced by change `implement-mini-chat-gear`.

## Requirements

### Requirement: Outbox queues
The gear SHALL register five outbox queues (defaults `mini-chat.usage_snapshot`, `mini-chat.attachment_cleanup`, `mini-chat.chat_cleanup`, `mini-chat.thread_summary`, `mini-chat.audit`) with `outbox.num_partitions` partitions, each with a handler returning Ok/Retry/Reject.

#### Scenario: Chat deletion enqueues cleanup
- **WHEN** a chat is deleted
- **THEN** a chat-cleanup outbox message with `tenant_id`, `chat_id`, `system_request_id`, `reason: "chat_soft_delete"`, `chat_deleted_at` is enqueued in the same transaction

### Requirement: Chat cleanup
The chat cleanup handler SHALL delete each `cleanup_status=pending` attachment's provider file (2xx/404 = success → `done`; other failures increment `cleanup_attempts`, record `last_cleanup_error`, and become `failed` at `cleanup_worker.max_attempts`), then, when no attachment is pending, delete the chat vector store (2xx/404 → delete the `chat_vector_stores` row). Messages for chats not soft-deleted are rejected.

#### Scenario: Provider resources removed
- **WHEN** a chat with an uploaded document is deleted
- **THEN** the provider receives a file DELETE and a vector store DELETE, the attachment becomes `cleanup_status=done` and the `chat_vector_stores` row is removed

### Requirement: Attachment cleanup
The attachment cleanup handler SHALL ack without action when the parent chat is soft-deleted, mark done when there is no provider file, otherwise delete the provider file with the same retry/terminal rules.

#### Scenario: Deleted attachment file removed
- **WHEN** an unreferenced attachment is deleted
- **THEN** the provider receives DELETE for its file and `cleanup_status` becomes `done`

### Requirement: Thread summary
After a completed turn whose assembled context reached `compression_threshold_pct` of the effective budget (and no summary exists) or required truncation, the finalization transaction SHALL enqueue a thread-summary task with a frozen target frontier (latest message before the causing turn). The handler SHALL load messages in `(base, target]`, call the summary model non-streaming, store the `<summary>` content in `thread_summaries` with CAS on the base frontier, mark the range `is_compressed`, and enqueue a system usage event; provider failure keeps the previous summary and retries up to `max_attempts`. A retry/edit/delete of a turn covered by the summary deletes it and clears `is_compressed`.

#### Scenario: Summary applied on next turn
- **WHEN** a summary exists for a chat
- **THEN** the next `stream_started` carries `thread_summary_applied.token_estimate` and the provider input starts with the summary message

### Requirement: Orphan watchdog
A periodic watchdog SHALL finalize running turns whose `COALESCE(last_progress_at, started_at)` is older than `orphan_watchdog.timeout_secs` as `failed`/`orphan_timeout` via CAS, settle with the estimated formula and enqueue usage (`billing_outcome: aborted`) and audit events in the same transaction.

#### Scenario: Stale turn finalized
- **WHEN** a running turn's last progress is older than the timeout
- **THEN** the turn becomes `failed` with `orphan_timeout`, its reserve is released and an estimated charge is spent

### Requirement: Audit delivery
The audit handler SHALL deserialize the payload first (reject corrupt payloads), deliver to the audit plugin with a 30 s timeout, ack-and-drop when no plugin is registered, retry transient errors and reject on the 120th attempt.

#### Scenario: Corrupt audit payload
- **WHEN** the audit handler receives invalid JSON
- **THEN** it returns Reject
