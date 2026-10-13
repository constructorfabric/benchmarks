## ADDED Requirements

### Requirement: Usage published reliably once per turn
The usage outbox handler SHALL deserialize each `UsageEvent` (malformed → Reject), call `publish_usage` on the model policy plugin resolved by vendor (resolution or transient errors → Retry), and acknowledge on success; one event is enqueued per finalized turn.

#### Scenario: Usage event payload
- **WHEN** a turn completes
- **THEN** exactly one usage outbox message with `billing_outcome = "completed"`, `settlement_method = "actual"`, `requester_type = "user"` and the turn's dedupe key is enqueued

### Requirement: Chat deletion cleanup
Chat soft-delete SHALL set `cleanup_status = pending` on the chat's attachments and enqueue a chat-cleanup message `{tenant_id, chat_id, system_request_id, reason:"chat_soft_delete", chat_deleted_at}` in the same transaction; the handler deletes each pending attachment's provider file (2xx/404 success, failures counted in `cleanup_attempts` up to `cleanup_worker.max_attempts`), then deletes the vector store and its `chat_vector_stores` row once all attachments are terminal; messages for active chats are rejected.

#### Scenario: Provider resources removed
- **WHEN** a chat with an uploaded document is deleted
- **THEN** the provider receives DELETE requests for the file and the vector store, the attachment has `cleanup_status = done` and the `chat_vector_stores` row is removed

### Requirement: Thread summary lifecycle
On a completed turn with `thread_summary_worker.enabled`, when context assembly truncated messages or (no summary exists and the assembled tokens reach `compression_threshold_pct` of the input budget), the finalization transaction SHALL enqueue a thread-summary task with a frozen target frontier (latest message before the turn); the handler summarizes exactly the uncompressed messages in `(base, target]` with a non-streaming request to the summary model, stores the `<summary>` text, advances the frontier with a CAS, marks those messages compressed and enqueues a `system_task` usage event; provider failures keep the old summary and retry up to `max_attempts`; a retry/edit/delete whose turn is covered by the summary deletes it and clears `is_compressed`.

#### Scenario: Summary applied on next turn
- **WHEN** a summary was committed for a chat
- **THEN** the next stream's `stream_started` contains `thread_summary_applied {token_estimate}` and the provider request contains the summary preamble message instead of the compressed messages

### Requirement: Orphan watchdog
Every `orphan_watchdog.scan_interval_secs` the watchdog SHALL finalize running, non-deleted turns whose `COALESCE(last_progress_at, started_at)` is older than `timeout_secs` as `failed` / `orphan_timeout` via a CAS that re-checks staleness, settle estimated quota and enqueue a usage event with `billing_outcome = aborted`.

#### Scenario: Stale running turn
- **WHEN** a running turn has not progressed for longer than the timeout
- **THEN** it becomes `failed` with `error_code = orphan_timeout` and its reserve is released with an estimated charge

### Requirement: OAGW provisioning and bundled plugins
At start the gear SHALL obtain an S2S context with `client_credentials`, register an OAGW upstream (host, port, `use_http`, auth plugin) and routes for each provider entry under its alias, and send all provider traffic through OAGW to `{alias}{api_path}`. The bundled `static-mini-chat-model-policy-plugin` (catalog, kill switches, default limits; multiplier validation) and `static-mini-chat-audit-plugin` SHALL register in types-registry and ClientHub for their vendor.

#### Scenario: Static plugin limits
- **WHEN** the static plugin config omits `default_standard_limits`
- **THEN** user limits are daily 100,000,000 and monthly 1,000,000,000 micro-credits for the `total` bucket
