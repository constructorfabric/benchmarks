## ADDED Requirements

### Requirement: Outbox-driven provider cleanup
Chat deletion SHALL enqueue a chat-cleanup message (`tenant_id`, `chat_id`, `system_request_id`, `reason: chat_soft_delete`, `chat_deleted_at`). Its handler SHALL delete each `pending` attachment's provider file through OAGW (2xx/404 = success → `cleanup_status = done`; other → `cleanup_attempts += 1`, `last_cleanup_error`, retry; at `cleanup_worker.max_attempts` → `failed`), and once no attachment is `pending`, delete the chat vector store (2xx/404 → delete the `chat_vector_stores` row). A message for an active chat SHALL be rejected. The attachment-cleanup handler SHALL skip attachments of deleted chats, mark done without a provider file, else delete the file and mark done.

#### Scenario: Chat deletion cleanup
- **WHEN** a chat with an uploaded document is deleted
- **THEN** the provider receives a file DELETE and a vector store DELETE, the attachment ends `cleanup_status = done` and the `chat_vector_stores` row is removed

### Requirement: Orphan watchdog
When `orphan_watchdog.enabled`, every `scan_interval_secs` the watchdog SHALL finalize turns `running`, non-deleted, with `COALESCE(last_progress_at, started_at) <= now - timeout_secs` (validated `90..=3600`) via a CAS that re-checks those predicates, setting `failed`/`orphan_timeout`, settling estimated quota and enqueuing a usage event (`billing_outcome: aborted`) and audit event in the same transaction.

#### Scenario: Stale turn
- **WHEN** a running turn's progress is older than the timeout
- **THEN** it becomes `failed` with `error_code = orphan_timeout` and its reserve is released

### Requirement: Gear configuration validation
The gear configuration SHALL follow DESIGN Appendix B: unknown keys rejected at top level and in `streaming`, `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`, `providers.<id>`, `thumbnail`, `knowledge_search`; ranges validated at init (e.g. `sse_ping_interval_seconds 5..=60`, `sse_channel_capacity 16..=64`, `overshoot_tolerance_factor 1.0..=1.5`, `orphan_watchdog.timeout_secs 90..=3600`, `upload_reaper.stale_after_secs 60..=86400`); `${VAR}` expansion in provider `host`/`auth_config` and `client_credentials`.

#### Scenario: Unknown key
- **WHEN** the config contains `streaming.web_search_context_size`
- **THEN** gear init fails
