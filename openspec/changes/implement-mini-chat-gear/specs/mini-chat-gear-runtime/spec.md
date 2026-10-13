## ADDED Requirements

### Requirement: SDK crate
The workspace SHALL contain the crate `gears/mini-chat/mini-chat-sdk` with package name `cf-gears-mini-chat-sdk` and library name `mini_chat_sdk`; it SHALL be a workspace member and the gear crate SHALL depend on it. The SDK SHALL define the model catalog types (`ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `ModelApiParams`, `ModelToolSupport`, `ModelPreference`, `ModelTier`), `PolicySnapshot`, `KillSwitches` (all six switches required on deserialize), `UserLimits`, `TierLimits`, `UsageEvent`, `UsageTokens`, the audit event types, and the plugin contracts `MiniChatModelPolicyPluginClientV1` / `MiniChatModelPolicyPluginSpecV1` and `MiniChatAuditPluginClientV1` / `MiniChatAuditPluginSpecV1`.

#### Scenario: Catalog entry defaults
- **WHEN** a catalog entry omits `enabled`, `estimation_budgets`, `max_tool_calls` and `web_search_context_size`
- **THEN** it deserializes with `enabled = false`, default estimation budgets (4 bytes/token, 100 overhead, 10% margin, 1000 image, 500 tool, 500 web search, 1000 code interpreter, floor 50), `max_tool_calls = 2` and `web_search_context_size = low`

#### Scenario: Usage event serialization
- **WHEN** a system-task usage event is serialized
- **THEN** `user_id` and `turn_id` are absent from the JSON and `system_task_type` is present; for a user turn `system_task_type` is absent

### Requirement: Gear registration and lifecycle
The crate SHALL register a gear named `mini-chat` that depends on types-registry, authn-resolver, authz-resolver and OAGW, uses the database, REST and stateful capabilities, and serves its routes under `url_prefix` (default `/mini-chat`). `init` SHALL load and validate the configuration, `start` SHALL provision OAGW upstreams/routes, start the outbox pipeline (usage, attachment cleanup, chat cleanup, thread summary, audit queues) and spawn the orphan watchdog and upload reaper; `stop` SHALL cancel and join the background workers.

#### Scenario: Server starts with mini-chat
- **WHEN** `cf-gears-example-server` built with features `mini-chat,static-authn,static-authz,single-tenant,static-credstore` is started with a config containing a `mini-chat` section
- **THEN** the gear runs its migrations, registers its routes under `/mini-chat/v1` and answers `GET /mini-chat/v1/models`

### Requirement: Configuration keys and validation
The gear configuration SHALL accept the keys and defaults of DESIGN Appendix B (`url_prefix`, `vendor`, `client_credentials`, `metrics`, `providers.<id>`, `streaming`, `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `thumbnail`, `knowledge_search`, `orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker`). Unknown keys SHALL be rejected at the top level and in `streaming`, `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`, `providers.<id>` (and tenant overrides), `thumbnail`, `knowledge_search`; worker sections SHALL ignore unknown keys. Out-of-range values (e.g. `sse_ping_interval_seconds` outside 5..=60, `sse_channel_capacity` outside 16..=64, `overshoot_tolerance_factor` outside 1.0..=1.5, `orphan_watchdog.timeout_secs` outside 90..=3600, `minimal_generation_floor` not in 1..=`streaming.max_output_tokens`, `storage_kind: azure` without `api_version`) SHALL fail gear init.

#### Scenario: Invalid ping interval
- **WHEN** the config sets `streaming.sse_ping_interval_seconds: 3`
- **THEN** gear init fails with a descriptive error

#### Scenario: Removed key
- **WHEN** the config sets `streaming.web_search_context_size`
- **THEN** gear init fails (unknown key)

### Requirement: Bundled plugins
The crate SHALL provide the plugin gears `static-mini-chat-model-policy-plugin` (module `infra::plugins::static_model_policy`) and `static-mini-chat-audit-plugin` (module `infra::plugins::static_audit`). The policy plugin SHALL serve a fixed snapshot (policy version 1) built from its `model_catalog` and `kill_switches` config, per-user limits from `default_standard_limits` (default daily 100_000_000 / monthly 1_000_000_000) and `default_premium_limits` (default 50_000_000 / 500_000_000), reject multipliers outside `1..=10_000_000_000` at init, and log published usage. The audit plugin SHALL log events when `enabled`.

#### Scenario: Static snapshot
- **WHEN** the gear asks the static policy plugin for the current policy version and snapshot
- **THEN** it receives version 1 and the configured catalog and kill switches

### Requirement: OAGW provisioning
At start the gear SHALL exchange `client_credentials` for an S2S security context and register an OAGW upstream (alias = `upstream_alias` or host) and routes (chat `api_path`, RAG file/vector-store routes for entries with `storage_kind`) for every provider entry and tenant override; an entry whose upstream already exists SHALL be reused. Provider requests SHALL be sent through the in-process OAGW proxy to `{alias}{api_path}` with `{model}` replaced by `provider_model_id`.

#### Scenario: Chat request routing
- **WHEN** a turn runs on a model whose provider entry has host `h` and `api_path` `/v1/responses`
- **THEN** the provider receives `POST /v1/responses` through OAGW with the model's `provider_model_id` in the body

### Requirement: Persistence schema
Migrations SHALL create the tables `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions` with the columns, uniqueness constraints and indexes of DESIGN §3.7 (including `UNIQUE(chat_id, request_id)` and the partial unique index of one running turn per chat on `chat_turns`, `UNIQUE(tenant_id, user_id, period_type, period_start, bucket)` on `quota_usage`, `UNIQUE(message_id, user_id)` on `message_reactions`), on both SQLite and PostgreSQL, together with the shared outbox migrations.

#### Scenario: One running turn per chat
- **WHEN** a second `running` turn row is inserted for a chat that already has a non-deleted running turn
- **THEN** the insert fails with a unique violation

### Requirement: Deferred provisioning is retried on demand
An upstream whose provisioning is deferred (e.g. its `secret_ref` is not readable yet) SHALL be retried in the background with a backoff starting at 2 s and doubling up to 60 s; additionally, a turn or upload that targets a provider alias still pending SHALL trigger an immediate provisioning retry and wait up to 5 s for it before calling the provider.

#### Scenario: Secret created after start
- **WHEN** the provider's `secret_ref` becomes readable after the gear started and a message is sent right afterwards
- **THEN** the upstream is provisioned on demand and the provider receives the request with the injected credential

### Requirement: Write transactions survive lock contention
User-facing write transactions (send reserve, turn finalization, retry/edit/delete, chat delete, attachment delete) SHALL be re-run as whole transactions with backoff when the database reports transient lock contention (SQLite busy/locked, PostgreSQL serialization failure or deadlock), and contention SHALL never be reported as a message persistence failure.

#### Scenario: Concurrent sends to different chats
- **WHEN** many messages are sent concurrently to different chats of one user on SQLite
- **THEN** every turn completes with `done` and no request fails with a 500 caused by lock contention
