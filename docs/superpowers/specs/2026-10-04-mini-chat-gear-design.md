# Mini Chat Gear + SDK — Implementation Design Spec

Date: 2026-10-04
Status: approved (brainstorming, architectural path)

## 0. Purpose and sources of truth

Implement the `mini-chat` gear (`gears/mini-chat/mini-chat`, package `cf-gears-mini-chat`, lib
`mini_chat`) and create its SDK crate (`gears/mini-chat/mini-chat-sdk`, package
`cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`). Add the SDK to the workspace and make the gear
depend on it.

The **contract** is `gears/mini-chat/docs/`: `PRD.md`, `DESIGN.md`, `ADR/*.md` (accepted ADRs
are normative and override older DESIGN/PRD text), `acceptance-criteria.md`. The generated
OpenAPI `docs/api/api.json` (paths `/mini-chat/v1/...`, schemas `ChatDetailDto`,
`MiniChatMessageDto`, `StreamMessageRequest`, `DoneData`, …) is the wire reference for request
and response bodies. When anything in this spec appears to disagree with those documents, the
documents win; this spec only pins choices the documents leave open and maps the work onto
crates/modules. Section references below like "D§3.7" mean DESIGN.md section 3.7, "P§5.1"
PRD.md, "ADR-0004" etc.

Platform conventions come from `docs/toolkit_unified_system/*.md` and the existing gears
(`gears/system/types-registry` for DB + outbox + REST + stateful lifecycle,
`gears/system/tenant-resolver` for the plugin pattern, `gears/system/oagw/oagw-sdk` for the
OAGW client, `libs/toolkit-db` for SecureORM/outbox/OData, `libs/toolkit-canonical-errors` for
`Problem`).

### Hard constraints

- Build: `cargo build --bin cf-gears-example-server --no-default-features --features
  mini-chat,static-authn,static-authz,single-tenant,static-credstore` must succeed.
- Offline: only crates already in `Cargo.lock`. The gear `Cargo.toml` dependency list is
  already complete; the SDK may only use workspace deps that are already locked.
- The example server imports `mini_chat` and the two modules
  `mini_chat::infra::plugins::static_audit` and `mini_chat::infra::plugins::static_model_policy`
  — these paths must keep existing and must contain the two bundled plugin gears.
- Own tests MUST NOT live in `testing/e2e/suites/mini_chat/`.
- Servers started manually are stopped by recorded pid only.

### Out of scope (per ADRs)

MCP (ADR-0006, no tables/endpoints/config keys — a config with `mcp` must fail as unknown key);
document summary (ADR-0007); chunk cap; per-user daily file_search & image quotas (ADR-0007/0008);
PolicySnapshot cache / `policy:notify` (ADR-0008); hard purge, audit content/redaction,
chat-deletion audit (ADR-0009); temporary chats / group chats (P2). Where a doc says
"Not implemented", we do not implement it, but we keep the related columns/fields (e.g.
`quota_usage.image_inputs`, `attachments.doc_summary`) exactly as specified.

## 1. Architecture

```
gears/mini-chat/
  mini-chat-sdk/           cf-gears-mini-chat-sdk  (lib mini_chat_sdk)
    src/lib.rs
    src/models.rs          policy/catalog/limits/usage/audit types (serde)
    src/plugin_api.rs      MiniChatModelPolicyPluginClientV1, MiniChatAuditPluginClientV1
    src/gts.rs             MiniChatModelPolicyPluginSpecV1, MiniChatAuditPluginSpecV1
    src/errors.rs          PolicyPluginError, PublishError, AuditPluginError
  mini-chat/               cf-gears-mini-chat (lib mini_chat)
    src/lib.rs
    src/gear.rs            #[toolkit::gear(name="mini-chat", deps=[types-registry, authn-resolver, authz-resolver, oagw], capabilities=[db, rest, stateful])]
    src/config.rs          MiniChatConfig (D Appendix B.1–B.9), validation
    src/api/rest/          routes.rs, dto.rs, error.rs (DomainError -> Problem), handlers/*, sse.rs
    src/domain/            error.rs, authz.rs, models.rs, services (chat, message, attachment,
                           turn, stream, finalization, quota, estimation, context, model
                           resolution, reactions, replay, thread_summary, cleanup), billing.rs
    src/infra/
      db/                  entities/, migrations/, repos/
      llm/                 types.rs, provider_resolver.rs, sse_parser.rs, sanitize.rs,
                           providers/{openai_responses,openai_chat_completions,vllm_responses,anthropic_messages}.rs,
                           storage/{files,vector_stores}.rs (openai/azure dispatch), anthropic_files.rs
      oagw_provisioning.rs
      outbox/              enqueuer.rs, handlers/{usage,audit,attachment_cleanup,chat_cleanup,thread_summary}.rs
      gateways/            policy_gateway.rs, audit_gateway.rs (types-registry plugin resolution)
      plugins/static_audit/, plugins/static_model_policy/   (bundled plugin gears)
      workers/             orphan_watchdog.rs, upload_reaper.rs, leader.rs
      metrics.rs, thumbnail.rs, mime.rs
    tests/                 integration tests (in-memory SQLite, fake OAGW, fake PDP, fake plugins)
testing/e2e/suites/mini_chat_dev/   own black-box suite (python, mock OpenAI provider)
```

Layering follows DDD-light (D§1.3). Handlers call domain services; services use repositories
over `toolkit_db` SecureConn/transactions and ports for LLM/storage/outbox/plugins (traits, so
integration tests can inject fakes). `#[domain_model]` is used where cheap; strict lint
conformance (`cargo gears lint`) is not a goal of this run.

### 1.1 Gear lifecycle (D§3.2 "Gear lifecycle")

- **init**: parse `gears.mini-chat.config` (unknown keys rejected in the strict sections, see
  §3), validate every section (startup fails on invalid values), register the two plugin GTS
  schemas in types-registry, create policy & audit gateways (lazy plugin resolution), resolve
  `AuthZResolverApi`, `ServiceGatewayClientV1`, `AuthNResolverClient` from ClientHub, build the
  provider resolver (fill `upstream_alias = host` when unset, also per tenant override), storage
  dispatchers, metrics, outbox enqueuer, domain services. REST routes are registered in the
  RestApiCapability step from the services built in init. Gear migrations = mini-chat schema
  migrations + `toolkit_db::outbox::outbox_migrations()` (default `toolkit_outbox_*` tables).
- **start** (stateful entry): exchange `client_credentials` via `authn_resolver` for an S2S
  `SecurityContext`; provision OAGW upstream + routes per provider entry and tenant override
  (create or reuse existing alias; deterministic misconfiguration fails startup; credstore-not-ready
  entries retried by a background reconcile loop: 2 s, doubling to 60 s, warning after 2 min).
  Start the outbox pipeline with the five leased queues (usage, attachment cleanup, chat cleanup,
  thread summary [lease = `thread_summary_worker.claim_timeout_secs`], audit [lease 60 s]);
  `outbox.num_partitions` partitions each. Then spawn orphan watchdog and upload reaper (leader
  elector: no-op elector without the `k8s` feature). Log an error (do not fail) if the summary
  model is missing/disabled while summaries are enabled.
- **stop**: cancel workers, join with a bounded timeout, stop the outbox pipeline.

### 1.2 OAGW usage decision

All proxy calls (chat streaming, summary, files, vector stores, cleanup) go through
`ServiceGatewayClientV1::proxy_request` with URI `/{alias}{path}` using the **S2S security
context** obtained at start (re-exchanged lazily if needed). Tenant/user identity is carried in
the provider request body (`user`, `metadata`), not in the OAGW context. Provider routing per
tenant uses `tenant_overrides` (alias per tenant).

## 2. SDK crate (`mini_chat_sdk`)

All types `serde` (de)serializable; field names exactly as in D§5.2.1, D Appendix A.1/A.3,
D§3.2 "System Task Attribution Rules".

- `ModelTier { Premium, Standard }` — serde accepts `Premium`/`Standard` (as in
  `config/mini-chat.yaml`) and lowercase `premium`/`standard` (as in D Appendix A.1);
  serializes lowercase.
- `ModelCatalogEntry` with required fields `id, provider_model_id, display_name, provider_id,
  provider_display_name, tier, context_window, max_output_tokens, max_input_tokens,
  input_tokens_credit_multiplier_micro, output_tokens_credit_multiplier_micro, max_num_results,
  general_config`; defaulted fields `description, icon, multiplier_display, system_prompt,
  thread_summary_prompt` (""), `enabled` (false), `multimodal_capabilities` ([]),
  `estimation_budgets` (defaults: bytes_per_token_conservative 4, fixed_overhead_tokens 100,
  safety_margin_pct 10, image_token_budget 1000, tool_surcharge_tokens 500,
  web_search_surcharge_tokens 500, code_interpreter_surcharge_tokens 1000,
  minimal_generation_floor 50), `web_search_context_size` (`low`), `max_tool_calls` (2),
  `preference` (None). Unknown keys ignored.
- `ModelGeneralConfig { type (rename "type"), available_from, max_file_size_mb, api_params:
  ApiParams { temperature?, top_p?, frequency_penalty?, presence_penalty?, stop: Vec<String>,
  extra_body?: Map, reasoning_effort?: String }, features: ModelFeatures { streaming,
  structured_output }, tool_support: ModelToolSupport { web_search, file_search,
  image_generation, code_interpreter, mcp }, supported_endpoints: SupportedEndpoints {…7 bools} }`.
- `ModelPreference { is_default: bool, sort_order: i32 }`.
- `KillSwitches { disable_premium_tier, force_standard_tier, disable_web_search,
  disable_file_search, disable_images, disable_code_interpreter }` — every field required in the
  SDK type (the static plugin's config mirror defaults missing ones to false and rejects unknown
  keys).
- `PolicySnapshot { policy_version: u64, model_catalog: Vec<ModelCatalogEntry>, kill_switches }`,
  `PolicyVersionInfo { policy_version, generated_at }`.
- `TierLimits { limit_daily_credits_micro: i64, limit_monthly_credits_micro: i64 }`,
  `UserLimits { user_id, policy_version, standard: TierLimits, premium: TierLimits }`.
- `UsageTokens { input_tokens, output_tokens, cache_read_input_tokens,
  cache_write_input_tokens, reasoning_tokens }` (i64).
- `UsageEvent` — fields of D§5.6 / A.3: `tenant_id, user_id?, chat_id, turn_id?, request_id,
  effective_model, selected_model, terminal_state, billing_outcome, usage: Option<UsageTokens>,
  actual_credits_micro, settlement_method, policy_version_applied, web_search_calls,
  code_interpreter_calls, file_search_calls, timestamp, requester_type, dedupe_key,
  system_task_type?`. `user_id`, `turn_id`, `system_task_type` are omitted when None
  (`skip_serializing_if`); `usage` serializes as `null` when None.
- Audit: `TurnAuditEvent` (event_type `turn_completed|turn_failed`, tenant_id, user_id, chat_id,
  turn_id, request_id, selected_model, effective_model, terminal_state, error_code?, usage,
  latency_ms {ttft_ms?, total_ms}, tool_calls {web_search_calls, file_search_calls},
  policy_decisions {quota {decision, downgrade_from?, downgrade_reason?}, license: null},
  prompt/response/attachments empty, quota_scope null, trace_id?, timestamp) and
  `TurnMutationAuditEvent` (event_type `turn_retry|turn_edit|turn_delete`, actor_user_id,
  tenant_id, chat_id, original_request_id?/new_request_id? or request_id, timestamp). Wrapped in
  `MiniChatAuditEvent { Turn(TurnAuditEvent), Mutation(TurnMutationAuditEvent) }` serialized
  `#[serde(untagged)]` (each variant carries its own `event_type` string field); the audit plugin
  receives this enum and the audit outbox payload is its JSON.
- Plugin traits (async_trait, Send + Sync):
  - `MiniChatModelPolicyPluginClientV1`: `get_current_policy_version(user_id) ->
    PolicyVersionInfo`, `get_policy_snapshot(user_id, version) -> PolicySnapshot`,
    `get_user_limits(user_id, version) -> UserLimits`, `publish_usage(UsageEvent) -> Result<(),
    PublishError>`, `check_user_license(...)` with default body `active: false` (never called).
  - `MiniChatAuditPluginClientV1`: `emit(MiniChatAuditEvent) -> Result<(), AuditPluginError>`.
- Errors: `PublishError { Transient(String), Permanent(String) }`, `AuditPluginError {
  Transient, Permanent, PluginTimeout }`, `PolicyPluginError`.
- GTS specs via `toolkit_gts::gts_type_schema` with base `PluginV1`:
  `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~` and
  `...~cf.core.mini_chat.audit_plugin.v1~` (exact strings are internal; follow the
  tenant-resolver SDK pattern).

## 3. Configuration

Gear section `gears.mini-chat.config` (D Appendix B). Top level and the sections `streaming`,
`estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`,
`providers.<id>` (+ `tenant_overrides.<tenant>`), `thumbnail`, `knowledge_search` use
`deny_unknown_fields`; `orphan_watchdog`, `upload_reaper`, `thread_summary_worker`,
`cleanup_worker` accept unknown keys. Every section has defaults (so the hidden config may set
only some keys). Defaults and ranges (validate at init, fail startup):

| Key | Default | Validation |
|---|---|---|
| `url_prefix` | `/mini-chat` | |
| `vendor` | `constructorfabric` | non-empty |
| `client_credentials.client_id/client_secret` | required | non-empty; `${VAR}` expansion; secret redacted in Debug |
| `metrics.prefix` | `""` → `mini_chat` | |
| `streaming.sse_ping_interval_seconds` | 15 | 5..=60 |
| `streaming.sse_channel_capacity` | 32 | 16..=64 |
| `streaming.max_output_tokens` | 32768 | >0 |
| `estimation_budgets.minimal_generation_floor` | 50 | >0, <= streaming.max_output_tokens; other fields deprecated (parsed, warn if non-default) |
| `quota.overshoot_tolerance_factor` | 1.10 | 1.0..=1.5 |
| `quota.warning_threshold_pct` | 80 | 1..=99 |
| `quota.web_search_max_calls_per_message` | 2 | >0 |
| `quota.web_search_daily_quota` | 75 | >0 |
| `quota.code_interpreter_max_calls_per_message` | 10 | >0 |
| `quota.code_interpreter_daily_quota` | 50 | >0 |
| `outbox.queue_name` | `mini-chat.usage_snapshot` | non-empty (all names) |
| `outbox.cleanup_queue_name` | `mini-chat.attachment_cleanup` | |
| `outbox.chat_cleanup_queue_name` | `mini-chat.chat_cleanup` | |
| `outbox.thread_summary_queue_name` | `mini-chat.thread_summary` | |
| `outbox.audit_queue_name` | `mini-chat.audit` | |
| `outbox.num_partitions` | 4 | power of 2 in 1..=64 |
| `context.recent_messages_limit` | 10 | 0..=100 |
| `context.web_search_guard` | P§5.2 guard text | |
| `context.file_search_guard` | built-in text | |
| `rag.uploaded_file_max_size_kb` | 25600 | >0 |
| `rag.uploaded_image_max_size_kb` | 5120 | >0 |
| `rag.max_images_per_message` | 4 | >0 |
| `rag.max_documents_per_chat` | 50 | >0 |
| `rag.max_total_upload_mb_per_chat` | 100 | >0 |
| `rag.allow_csv_upload` | true | |
| `rag.max_concurrent_uploads` | 10 | 1..=256 |
| `thumbnail.width/height` | 128/128 | >0 |
| `thumbnail.max_bytes` | 131072 | >0 |
| `thumbnail.max_pixels` | 100000000 | >0 |
| `thumbnail.max_decode_bytes` | 33554432 | >0 |
| `orphan_watchdog.enabled/timeout_secs/scan_interval_secs` | true/300/60 | 90..=3600 / 1..=3600 |
| `upload_reaper.enabled/scan_interval_secs/stale_after_secs` | true/60/300 | 1..=3600 / 60..=86400 |
| `thread_summary_worker.enabled` | true | |
| `thread_summary_worker.claim_timeout_secs` | 300 | 30..=3600 |
| `thread_summary_worker.max_attempts` | 3 | >0 |
| `thread_summary_worker.compression_threshold_pct` | 80 | 1..=99 |
| `thread_summary_worker.summary_model_id` | "" (= `gpt-4.1-mini`) | |
| `thread_summary_worker.summary_system_prompt` | built-in (D B.5.5) | |
| `thread_summary_worker.message_content_limit` | 4000 | 0 = no truncation |
| `thread_summary_worker.reconcile_interval_secs` | 60 | deprecated |
| `cleanup_worker.max_attempts` | 5 | >0; other cleanup_worker fields deprecated |
| `knowledge_search.*` | enabled false; max_calls_per_message 3; top_k 5; max_chunk_chars 2000; guard built-in | vector_store_id + provider_id required when enabled |
| `providers` | one entry `openai` (openai_responses, api.openai.com, `storage_kind=openai`, apikey auth `cred://openai-key`) | see below |

Provider entry: `kind` (required: `openai_responses|openai_chat_completions|vllm_responses|anthropic_messages`),
`host` (required, `${VAR}` expansion, allowed chars `[A-Za-z0-9.\-_:\[\]]`), `port` (443, or 80 when
`use_http`), `use_http` (false), `upstream_alias` (defaults to host), `api_path` (`/v1/responses`,
`{model}` placeholder), `auth_plugin_type`, `auth_config` (map, `${VAR}` expansion),
`storage_kind` (`openai|azure`, required), `storage_backend` (defaults to provider id),
`api_version` (required + charset `[A-Za-z0-9.-]` when `storage_kind=azure`), `rag_provider`
(must name an existing entry), `tenant_overrides.<uuid>` (`host`, `upstream_alias`,
`auth_plugin_type`, `auth_config`; must set host or upstream_alias). Unknown keys (e.g. the removed
`supports_file_search_filters`) fail startup. `streaming.web_search_context_size` likewise.

Plugin sections (separate gears inside the gear crate, `deny_unknown_fields` top level):
- `static-mini-chat-model-policy-plugin`: `vendor` (constructorfabric), `priority` (100),
  `model_catalog` (required when section present; [] when absent), `kill_switches` (all false;
  missing fields false; unknown key = error), `default_standard_limits` (daily 100_000_000 /
  monthly 1_000_000_000), `default_premium_limits` (50_000_000 / 500_000_000). Multipliers must be
  in 1..=10_000_000_000 and `bytes_per_token_conservative` > 0 (init error). Fixed policy version 1.
  `publish_usage` logs.
- `static-mini-chat-audit-plugin`: `enabled` (true), `vendor`, `priority`. Logs events.

## 4. Database schema (D§3.7 — exact names)

One migration set, PostgreSQL + SQLite variants where dialects differ. UUID columns are written via
SeaORM `Uuid` (SQLite stores 16-byte BLOB). Timestamps `TIMESTAMPTZ` (SeaORM `DateTimeUtc` /
`time::OffsetDateTime` mapping consistent with the platform). Tables and columns:

- `chats`: id, tenant_id, user_id, model VARCHAR(1024), title VARCHAR(255) NULL, is_temporary BOOL
  default false, created_at, updated_at, deleted_at NULL. Index `(tenant_id,user_id,updated_at)`
  partial `WHERE deleted_at IS NULL`. Secure: tenant `tenant_id`, owner `user_id`, resource `id`.
- `messages`: id, tenant_id, chat_id FK cascade, request_id NULL, role VARCHAR(16)
  (user|assistant|system), content TEXT, content_type VARCHAR(32) (`text`), token_estimate INT (0),
  provider_response_id VARCHAR(128) NULL, request_kind VARCHAR(16) (`chat`), features_used JSON
  (`[]`), input_tokens BIGINT 0, output_tokens BIGINT 0, cache_read_input_tokens BIGINT 0,
  cache_write_input_tokens BIGINT 0, reasoning_tokens BIGINT 0, model VARCHAR(1024) NULL,
  is_compressed BOOL false, created_at, deleted_at NULL. UNIQUE `(chat_id, request_id, role)`
  WHERE request_id NOT NULL AND deleted_at IS NULL; index `(chat_id, created_at)` partial; UNIQUE
  `(id, chat_id)`.
- `chat_turns`: id, tenant_id, chat_id FK, request_id, requester_type (user|system, CHECK),
  requester_user_id NULL, state (CHECK running|completed|failed|cancelled), provider_name NULL,
  provider_response_id NULL, assistant_message_id NULL, error_code VARCHAR(64) NULL,
  reserve_tokens BIGINT NULL, max_output_tokens_applied INT NULL, reserved_credits_micro BIGINT
  NULL, policy_version_applied BIGINT NULL, effective_model VARCHAR(1024) NULL,
  minimal_generation_floor_applied INT NULL, error_detail TEXT NULL, deleted_at NULL,
  replaced_by_request_id NULL, started_at (no DB default), last_progress_at NULL,
  web_search_enabled BOOL false, web_search_completed_count INT 0,
  code_interpreter_completed_count INT 0, file_search_completed_count INT 0, completed_at NULL,
  updated_at. UNIQUE `(chat_id, request_id)`; index `(chat_id, started_at)` partial non-deleted;
  UNIQUE `(chat_id) WHERE state='running' AND deleted_at IS NULL`; index `(last_progress_at)`
  partial running.
- `attachments`: id, tenant_id, chat_id FK, uploaded_by_user_id, filename VARCHAR(255),
  content_type VARCHAR(128), size_bytes BIGINT, storage_backend VARCHAR(32) default 'azure',
  provider_file_id VARCHAR(128) NULL, status (CHECK pending|uploaded|ready|failed), error_code
  VARCHAR(64) NULL, attachment_kind (CHECK document|image), for_file_search BOOL false,
  for_code_interpreter BOOL false, doc_summary TEXT NULL, img_thumbnail BLOB NULL,
  img_thumbnail_width INT NULL, img_thumbnail_height INT NULL, summary_model NULL,
  summary_updated_at NULL, cleanup_status VARCHAR(16) NULL, cleanup_attempts INT 0,
  last_cleanup_error TEXT NULL, cleanup_updated_at NULL, created_at, updated_at, deleted_at NULL,
  secondary_file_id NULL, secondary_status VARCHAR(16) default 'not_attempted' (CHECK
  not_attempted|pending|uploaded|failed), secondary_provider_kind NULL (CHECK anthropic|NULL).
  Indexes per D§3.7 (incl. `idx_attachments_stale_upload` plain index); UNIQUE `(id, chat_id)`.
- `message_attachments`: tenant_id, chat_id, message_id, attachment_id, created_at; PK
  `(chat_id, message_id, attachment_id)`; composite FKs to `messages(id,chat_id)` and
  `attachments(id,chat_id)` cascade; indexes `(tenant_id,chat_id)`, `(attachment_id,chat_id)`.
- `thread_summaries`: id, tenant_id, chat_id UNIQUE FK, summary_text, summarized_up_to_created_at,
  summarized_up_to_message_id, token_estimate INT, created_at, updated_at.
- `chat_vector_stores`: id, tenant_id, chat_id, vector_store_id VARCHAR(128) NULL, provider
  VARCHAR(128), file_count INT 0 CHECK >=0, created_at; UNIQUE `(tenant_id, chat_id)`.
- `quota_usage`: id, tenant_id, user_id, period_type VARCHAR(16), period_start DATE, bucket
  VARCHAR(32), spent_credits_micro BIGINT 0, reserved_credits_micro BIGINT 0, calls INT 0,
  input_tokens BIGINT 0, output_tokens BIGINT 0, file_search_calls INT 0, web_search_calls INT 0,
  code_interpreter_calls INT 0, rag_retrieval_calls INT 0, image_inputs INT 0, image_upload_bytes
  BIGINT 0, updated_at; UNIQUE `(tenant_id,user_id,period_type,period_start,bucket)`.
- `message_reactions`: id, message_id FK cascade, user_id, tenant_id, reaction (CHECK like|dislike),
  created_at; UNIQUE `(message_id, user_id)`.
- Outbox: `toolkit_db::outbox::outbox_migrations()` (default prefix) in the same migration list.

## 5. REST API contract (D§3.3, P§7, OpenAPI)

All routes under `{url_prefix}/v1` (default `/mini-chat/v1`), `.authenticated()`, license gate
= platform base feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` (ADR-0008). operation_ids
`mini_chat.<resource>.<action>`.

| Method | Path | Success |
|---|---|---|
| POST | /chats | 201 ChatDetailDto + `Location: /mini-chat/v1/chats/{id}` (request path as gear sees it + id) |
| GET | /chats | 200 Page<ChatDetailDto> (OData $filter/$orderby on updated_at,id,title; default `updated_at desc, id desc`; limit default 20, max 100 clamp, 0 → 400) |
| GET | /chats/{id} | 200 ChatDetailDto |
| PATCH | /chats/{id} | 200 ChatDetailDto (title only; trim; 1..=255) |
| DELETE | /chats/{id} | 204 |
| GET | /chats/{id}/messages | 200 Page<MiniChatMessageDto> (OData on created_at,id,role; default `created_at asc, id asc`) |
| POST | /chats/{id}/messages:stream | 200 text/event-stream |
| POST | /chats/{id}/attachments | 201 AttachmentDetailDto (multipart, field `file`) |
| GET | /chats/{id}/attachments/{attachment_id} | 200 AttachmentDetailDto |
| DELETE | /chats/{id}/attachments/{attachment_id} | 204 |
| GET | /chats/{id}/turns/{request_id} | 200 TurnStatusResponse |
| POST | /chats/{id}/turns/{request_id}/retry | 200 SSE |
| PATCH | /chats/{id}/turns/{request_id} | 200 SSE (body EditTurnRequest) |
| DELETE | /chats/{id}/turns/{request_id} | 204 |
| PUT | /chats/{id}/messages/{msg_id}/reaction | 200 MiniChatReactionDto |
| DELETE | /chats/{id}/messages/{msg_id}/reaction | 204 |
| GET | /models | 200 ModelListDto |
| GET | /models/{id} | 200 ModelDto |
| GET | /quota/status | 200 QuotaStatusResponse |

The `:stream` path suffix must be routable by axum (`/chats/{id}/messages:stream`); verify with a
test and, if axum's matcher cannot express it directly, register a route that captures the
segment and dispatches on the literal suffix.

### 5.1 Serialization rules (OpenAPI + D§3.3)

- ChatDetailDto: `title` omitted when None. `message_count` = non-deleted messages of the chat.
- MiniChatMessageDto: `request_id` always present (a stored NULL request_id → 500), `attachments`
  always an array (only non-deleted attachments via `message_attachments`; summary fields
  attachment_id, kind, filename, status, img_thumbnail only for ready images), `my_reaction`
  always present (`null` allowed; always null for non-assistant), `model` omitted when None,
  `input_tokens`/`output_tokens` omitted when 0. Soft-deleted messages excluded.
- AttachmentDetailDto: optional fields (`error_code`, `doc_summary`, `img_thumbnail`,
  `summary_updated_at`) omitted when None; `img_thumbnail` only for ready images; `error_code` only
  when failed. Thumbnail `content_type = image/webp`, `data_base64` standard base64.
- TurnStatusResponse: `state` running|done|error|cancelled; `error_code` and
  `assistant_message_id` omitted when None.
- ModelDto: `model_id, display_name, tier (lowercase), multiplier_display, description (omitted
  when empty), multimodal_capabilities, context_window`; never provider ids, multipliers, is_default.
- QuotaStatusResponse per D§3.2 "Quota Status Endpoint": tiers `premium` then `total`; periods
  daily, monthly; `used = spent + reserved`; `remaining = max(limit - used, 0)`;
  `remaining_percentage = floor(remaining*100/limit)`; `warning = pct <= 100 - threshold`;
  `exhausted = pct == 0`; periods with limit <= 0 omitted; `next_reset` RFC 3339 (tomorrow 00:00Z
  / 1st of next month 00:00Z).

### 5.2 Error contract (ADR-0004 table — implement every row)

`DomainError` → `toolkit_canonical_errors::CanonicalError`/`Problem` with category, HTTP status,
and machine-readable field exactly as in the ADR-0004 table (resource types
`gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`, OData
`gts.cf.core.odata.query.v1~`; `context.reason` for aborted/permission_denied;
`field_violations[{field, reason}]` for invalid_argument/out_of_range; `violations[{subject,
description, type}]` for failed_precondition; `violations[{subject, description:
"quota_exceeded"}]` for resource_exhausted; `Retry-After` header + `context.retry_after_seconds`
for 503). `detail` texts never contain provider identifiers or internal ids. Platform extractor
behaviors (422 invalid_json_body, 400 json_syntax_error, 415 missing_json_content_type, 400
invalid_path_params) come from using the platform extractors (`toolkit::api` Json/Path) — use them
rather than raw axum ones; verify each with a test.

Validation order details that matter:
- POST /chats: title validated before authz and model lookup.
- PUT reaction: reaction value validated before authz.
- Upload: chat lookup (404) and model resolution (400 INVALID_MODEL) before reading the body.
- messages:stream: see §6.1.

## 6. Streaming pipeline (D§3.6 "Send Message", D§4 "Turn Lifecycle", D§5.7–5.9)

### 6.1 Send (`POST /messages:stream`) order

1. Body schema (platform extractor); `content.trim()` empty → 400 EMPTY_CONTENT; `attachment_ids`
   duplicates or count > `max_documents_per_chat + max_images_per_message` → 400 invalid_attachment
   (before any query that takes the list).
2. PEP `send_message` on the chat (owner predicate added) → 403/503; load chat scoped → 404 chat.
3. Resolve chat model without enabled filter: missing from catalog → 400 INVALID_MODEL; plugin
   failure → 500 internal.
4. Idempotency (request_id given): existing `(chat_id, request_id)`: completed & not deleted →
   **replay** (separate code path; no reserve/settlement/outbox/audit/provider call); otherwise
   (running/failed/cancelled or soft-deleted) → 409 `request_id_conflict`.
5. Parallel guard: any running turn in chat → 409 `turn_already_running`.
6. Snapshot boundary; prior_context_tokens; attachment facts (ready docs present, ready XLSX present,
   images among attachment_ids via a scoped lookup — image count > max_images → 400
   TOO_MANY_IMAGES).
7. Kill switch: `web_search.enabled && disable_web_search` → 400 failed_precondition web_search
   FEATURE_DISABLED (before the quota cascade). Images present && `disable_images` → 400 images
   FEATURE_DISABLED.
8. Quota preflight (§7): cascade → effective model or 429 tokens; daily web_search quota (only if
   enabled & effective model supports web search) → 429 web_search; daily code_interpreter quota
   (only if tool would be sent) → 429 code_interpreter.
9. Vision check on effective model → 400 VISION_NOT_SUPPORTED. `INPUT_TOO_LONG` (current message
   estimate > effective model `max_input_tokens`, when > 0).
10. Context assembly (§8) → 400 CONTEXT_BUDGET_EXCEEDED. Resolve provider for effective model's
    `provider_id` + tenant → 500 on failure.
11. Reserve transaction: quota reserve increments + limit re-check (over → rollback, 429 tokens);
    insert user message (+ bump `chats.updated_at`); validate attachment_ids inside the txn
    (tenant, uploader = caller, chat, status ready, not deleted → else 400 invalid_attachment) and
    insert `message_attachments`; insert `chat_turns` running with all preflight columns,
    `web_search_enabled`, `started_at = last_progress_at = now`. Unique violation on running
    index → 409 `turn_already_running`; on `(chat_id, request_id)` → re-read → 409
    `request_id_conflict`.
12. Open SSE: `stream_started {request_id, message_id (pre-allocated), is_new_turn: true,
    thread_summary_applied?}`.

Steps 2–11 run inside a spawned task whose result the handler awaits (client disconnect during
setup does not abort setup; a committed turn always gets its provider task).

### 6.2 Provider task and relay

- Provider task sends the adapter request through OAGW, parses provider SSE into internal events
  and forwards them over a bounded mpsc (`sse_channel_capacity`) to the SSE writer — no buffering of
  the full response. Deltas → `delta {type:"text"|"reasoning", content}`; tool events → `tool
  {phase, name, details}`; accumulate text, citations (mapped per D§4 "Citation File ID and Title
  Resolution" via an in-memory `provider_file_id → (attachment_id, filename)` map built only when
  file_search is enabled; unknown/deleted → omitted), usage, provider response id.
- Per-turn tool limits counted on tool `start` events across the turn: web_search >
  `web_search_max_calls_per_message` → cancel provider stream, finalize failed
  `web_search_calls_exceeded` (estimated settlement), SSE error; same for code_interpreter.
  Completed tool counts (done events) → `chat_turns.*_completed_count` (incremented during stream).
- `last_progress_at` refreshed at most every 30 s on deltas and tool events.
- Ping: `event: ping`, `data: {}` every `sse_ping_interval_seconds` of idle time only between
  `stream_started` and the first delta/tool; afterwards axum keep-alive comment every 30 s.
- Cancellation: dropping the SSE response (client disconnect) cancels a `CancellationToken` → the
  provider request is dropped, turn finalized `cancelled` (partial text persisted as assistant
  message when non-empty, best effort), estimated settlement, no SSE error. A failed channel send
  is treated the same.
- Terminal: on provider `completed`/`incomplete` → finalization (completed); on provider error →
  finalization (failed, code per D "Streaming error codes"). The terminal SSE event is emitted
  **only after** the finalization transaction commits (CAS winner). Completed → `citations` (if
  any mapped, at most one, after all deltas) then `done`. If the provider task ends without a
  terminal event (CAS lost, panic) the relay sends `error{stream_interrupted}`. Finalization
  failure on completed → `error{finalization_failed}` (turn stays running); message persistence
  failure → turn failed + `error{message_persistence_failed}`.
- `done` payload: `usage {input_tokens, output_tokens}`, `effective_model`, `selected_model`
  (`chats.model`), `quota_decision` (allow|downgrade), `downgrade_from` (= selected) and
  `downgrade_reason` only on downgrade, `quota_warnings` (CAS-winning completed turns; entries with
  `next_reset` only when warning/exhausted; periods with limit <= 0 skipped).
- SSE `error` payload `{code, message}` with sanitized message (§9.4).
- Stream closes right after the terminal event.

### 6.3 Finalization (single transaction, CAS)

`UPDATE chat_turns SET state=:s, completed_at, updated_at, error_code?, assistant_message_id?,
provider_response_id? WHERE id=:id AND state='running'`. rows=0 → no-op. Winner, in the same
transaction: insert assistant message for completed (always, even empty text; `model =
effective_model`, tokens, provider_response_id, `request_id` = turn request_id, id = pre-allocated
message_id) or for cancelled with non-empty text; quota settlement (§7) with the turn's
persisted preflight fields (finalization does not touch `chats.updated_at`; it is bumped only on
create, rename, delete, send, retry and edit); enqueue `UsageEvent` (queue `outbox.queue_name`, partition by tenant) and
`TurnAuditEvent` (audit queue); evaluate thread-summary trigger (completed turns only, when
enabled) and enqueue summary work (§10.2). Billing outcome derivation per D§5.8 table (shared
function, unit-tested per row).

### 6.4 Replay

`stream_started {is_new_turn:false, message_id: persisted assistant id}`, one `delta` with full
persisted text, `done` with usage from the assistant message, `effective_model` = message.model,
`selected_model` = chats.model, `quota_decision` = downgrade iff they differ, `downgrade_from` on
downgrade, no `downgrade_reason`, no `quota_warnings`, no citations.

### 6.5 Turn status, mutations (D§3.9)

- `GET /turns/{request_id}`: PEP `read_turn`; soft-deleted or missing → 404 turn. Mapping
  running→running, completed→done, failed→error, cancelled→cancelled.
- Retry/edit: (1) preview: PEP once (`retry_turn`/`edit_turn`), load turn by request_id
  (missing → 404 turn), `requester_user_id != caller` → 403 AUTHZ_DENIED, running → 400 turn_state
  STATE (checked before latest), not latest non-deleted by `(started_at, id)` → 409
  NOT_LATEST_TURN; (2) resolve chat model (no enabled filter; missing → 400 INVALID_MODEL) and run
  mutation preflight (kill switches, quota cascade, image guards for original message images, daily
  tool quotas using the stored `web_search_enabled`) — rejection changes nothing; (3) mutation
  commit txn: re-check latest+terminal (`SELECT … FOR UPDATE` on PG), soft-delete old turn
  (`deleted_at`, `replaced_by_request_id`) and its messages, insert new user message (retry: copy
  content; edit: new content) with copied non-deleted `message_attachments`, insert new running turn
  (server-generated request_id v4, preflight columns NULL, `web_search_enabled` copied), delete
  thread summary if its frontier >= old user message `(created_at,id)` and clear `is_compressed`
  for the chat, bump `chats.updated_at`, enqueue `turn_retry`/`turn_edit` audit. Running-index
  unique violation → 409 GENERATION_IN_PROGRESS. (4) context assembly, provider resolution, quota
  reserve + re-check + fill preflight columns (one txn); failure → mark new turn failed
  (`context_length_exceeded` | `quota_exceeded` | `turn_setup_failed`) with plain CAS, no
  settlement/outbox, return JSON error (400/429/500). (5) stream as send.
- Delete: preview as above (no preflight); txn: soft-delete turn + its messages, summary
  interaction rule, enqueue `turn_delete` audit; 204.

## 7. Quota & billing (D§3.2 quota service, D§3.7 quota_usage, D§5)

- Credits: `credits_micro(in, out, in_mult, out_mult) = ceil_div(in*in_mult, 1e6) +
  ceil_div(out*out_mult, 1e6)`, checked arithmetic with bounds (tokens <= 10_000_000; mult in
  1..=10_000_000_000); preflight substitutes `i64::MAX` on error (candidate unavailable).
- Estimation (per candidate's `estimation_budgets`): `estimated_text_tokens = ceil((ceil(bytes /
  bpt) + fixed_overhead) * (100 + margin) / 100)` (bpt 0 clamped to 1); `prior_context_tokens` =
  input+output of latest non-deleted assistant message with non-zero tokens; surcharges: images ×
  image_token_budget; tool_surcharge if chat has ready docs && model file_search && !disable_file_search;
  web_search_surcharge if enabled && model web_search; code_interpreter_surcharge if ready XLSX &&
  model code_interpreter && !disable_code_interpreter. `max_output_tokens_applied =
  min(model.max_output_tokens, streaming.max_output_tokens)`; `reserve_tokens = est_input +
  max_out`; `reserved_credits = credits(est_input, max_out)`; `minimal_generation_floor_applied =
  min(config floor, max_out)`.
- Cascade (D "Downgrade Decision Flow"): start tier from selected model (enabled → own tier; disabled
  → own tier + reason model_disabled; missing → premium + model_disabled); cascade premium→standard
  or standard only; skip premium on `force_standard_tier`/`disable_premium_tier` (reason unless
  set); candidate = selected model if in tier & enabled, else enabled `is_default` of tier, else
  first enabled of tier; availability: for daily and monthly, bucket `total` (and `tier:premium`
  for premium) `spent + reserved + this_reserve <= limit`; premium unavailable → reason
  `premium_quota_exhausted` unless set. Decision allow iff effective == selected and no reason.
  None → 429 tokens. Limits: `total` ↔ `user_limits.standard`, `tier:premium` ↔ `user_limits.premium`.
  Reads in one transaction (FOR UPDATE on PG).
- Daily tool quotas: `quota_usage[daily,total].web_search_calls >= web_search_daily_quota` → 429
  web_search (only when the tool is sent); code_interpreter likewise.
- Reserve (§6.1 step 11): upsert rows for (daily, monthly) × (total[, tier:premium]) incrementing
  `reserved_credits_micro`; then re-read and verify `spent + reserved <= limit` for every bucket
  of the decision; violation → rollback → 429 tokens.
- Settlement (in finalization txn; period_start values from preflight, watchdog derives from
  `started_at`): bucket total: `reserved -= turn_reserved; spent += committed; calls += 1;
  input/output_tokens += actual (actual method only); web_search_calls/code_interpreter_calls +=
  counts (actual & estimated)`; premium bucket (if effective tier premium, tier looked up via
  `policy_version_applied` snapshot): reserved/spent/calls. Methods: completed → actual (overshoot
  cap: if actual_tokens > reserve_tokens and ratio > tolerance → committed = reserved_credits);
  failed with non-zero usage → actual; failed without usage / cancelled / orphan → estimated
  `credits(est_input, floor)`; `released` only for the (unreachable) pre-provider-after-reserve case.
- Usage event fields per §2; `dedupe_key = "{tenant.simple}/{turn.simple}/{request.simple}"`;
  system task: `"{tenant}/thread_summary_update/{system_request_id}"`, `billing_outcome
  system_task`, `settlement_method none`, `actual_credits_micro 0`, `requester_type system`,
  no user_id/turn_id.
- Quota status endpoint per §5.1.

## 8. Context assembly (D§4 "Context Plan Assembly and Truncation")

Order: system prompt (model `system_prompt`) + guards (file_search guard if file_search sent,
web_search guard if web_search sent, knowledge guard if search_knowledge sent) → thread summary as a
`user` message with the B.5.5 preamble → recent messages (`request_id IS NOT NULL`, not deleted,
not compressed, `<= boundary`, `> frontier` when a summary exists, newest first limit
`recent_messages_limit`, reversed) → current user message (+ `input_image` items with provider file
ids for images in attachment_ids). Budget: `input_limit = min(max_input_tokens (0 = none),
context_window - max_out)`; `token_budget = input_limit - surcharges - fixed_overhead`; invalid →
CONTEXT_BUDGET_EXCEEDED. Truncation: mandatory items must fit; summary kept if fits; recent
messages newest→oldest while they fit, then drop leading assistant messages (whole turns);
`messages_truncated` flag; `assembled_context_tokens` for the summary trigger. Estimation per item
uses the effective model's budgets (`image_token_budget` per current image).

Tool list (built from tool_support of the effective model, attachments, kill switches, request):
file_search (`vector_store_ids:[vs]`, `max_num_results`) iff chat has ready docs (non-deleted) &&
model file_search && !disable_file_search && vector store exists; web_search iff request enabled &&
model web_search (`search_context_size` from catalog); code_interpreter iff ready XLSX && model
code_interpreter && !disable_code_interpreter (`container: {type:auto, file_ids}`, `include:
["code_interpreter_call.outputs"]`); search_knowledge iff knowledge search enabled and file_search
not included.

## 9. LLM provider layer (D§3.2 llm_provider, D§3.5, ADR-0005)

### 9.1 OpenAI Responses adapter (full fidelity; E2E-tested)

`POST /{alias}{api_path}` (with `{model}` replaced). Body:
`{model: provider_model_id, instructions: system+guards, input: [messages…], stream: true,
max_output_tokens, store: false, tools?: [...], max_tool_calls: model.max_tool_calls, include?,
user: "{tenant_hex}{user_hex}", metadata: {tenant_id, user_id, chat_id, request_type: "chat",
feature: "none"|"file_search"|"web_search"|"code_interpreter"|joined with "+"}, temperature?,
top_p?, frequency_penalty?, presence_penalty?, stop? (when non-empty), reasoning?: {effort}}` +
`extra_body` merged at top level except controlled keys (warn). Input messages: `{role, content}`
with string content for history; current user message `[{type:input_text,text},{type:input_image,
file_id}…]` when images, else string. SSE parsing: event name from `event:` line, else `type` in
data. Mapping per D "Provider Event Translation" table (delta, file_search/web_search/
code_interpreter tool start/done, `output_item.done` code_interpreter outputs capped 8192 chars,
annotations → citations (url_citation → web with span & snippet from text range; file_citation →
file with attachment_id lookup, empty snippet, no span), `response.completed`/`incomplete` → done
with usage (`input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`,
`output_tokens_details.reasoning_tokens`), `response.failed`/`error` → error). HTTP error statuses:
429 → `rate_limited` (with `retry in {N}s` when numeric Retry-After), gateway timeout / OAGW 504
deadline_exceeded → `provider_timeout`, others → `provider_error`. Non-streaming summary call:
same body with `stream: false`, `max_output_tokens = summary model catalog max_output_tokens`,
`metadata.request_type = "summary"`, `feature = "none"`, user = tenant hex + `11111111-6a88-4768-9dfc-6bcd5187d9ed` hex.

### 9.2 Other adapters (unit-tested, no E2E)

- `openai_chat_completions`: `messages` (system first), `stream: true`, `stream_options:
  {include_usage: true}`, `max_completion_tokens`, `user`; drops file_search/web_search/
  code_interpreter; keeps function tools; maps `choices[].delta.content`, `finish_reason=length` →
  incomplete `max_tokens`, usage `prompt_tokens/completion_tokens`, tool call events named
  `function_call`.
- `vllm_responses`: Responses format without metadata and with all tools dropped; `<think>…</think>`
  text → `delta type reasoning`.
- `anthropic_messages`: `system`, `messages`, `max_tokens`, `metadata.user_id`, `stream: true`;
  drops file_search; maps web_search → server web search tool, code_interpreter → code execution;
  parses `message_start`/`content_block_*`/`message_delta`/`message_stop`; usage `input_tokens`/
  `output_tokens`/cache fields; `stop_reason=max_tokens` → incomplete; images via Anthropic Files
  file ids (secondary copy); no citations.

### 9.3 Storage (files & vector stores)

Dispatch by the RAG provider's `storage_kind` (`rag_provider` entry if set, else the model's
provider). OpenAI prefix `/v1`; Azure prefix `/openai` + `?api-version={api_version}`.
Files: `POST {prefix}/files` multipart (`purpose=assistants`, `file` with filename
`{chat_id}_{attachment_id}.{ext}` and content type) → `id`. Delete: `DELETE {prefix}/files/{id}`
(2xx/404 success). Vector stores: `POST {prefix}/vector_stores` (`{name: "chat-{chat_id}"}`) → id;
`POST {prefix}/vector_stores/{vs}/files` `{file_id, attributes: {attachment_id}}` → status;
`GET {prefix}/vector_stores/{vs}/files/{file_id}` → status; `DELETE {prefix}/vector_stores/{vs}`.
OAGW routes for these are provisioned per entry with `storage_kind` (best effort).

### 9.4 Sanitization (D "Provider identifier non-exposure invariant")

Replace provider ids (`resp_`, `chatcmpl-`, `cmpl-`, `msg_` + alnum; `file-`, `file_`,
`assistant-`, `vs_` + ≥12 alnum) with `[provider_id]`, URLs with `[url]`, `sk-` + ≥10 alnum and
`Bearer <token>` with `[credential]`. Applied to every SSE `error.message` and any client-visible
text derived from provider output errors.

## 10. Background work

### 10.1 Outbox handlers (D§5.6 "Shared Outbox Processing Model")

- usage: deserialize `UsageEvent` (Reject on corrupt) → resolve policy plugin → `publish_usage`
  (Transient/resolution failure → Retry; Permanent → Reject).
- audit: deserialize first (Reject corrupt); resolve audit plugin each delivery (no negative
  caching; warn once per period); none registered → Ok + `dropped`; instance found but client
  missing → Retry (reset cached id); 30 s timeout → Retry; Permanent → Reject; Retry on the 120th
  attempt → Reject.
- attachment cleanup (`event_type` attachment_deleted | attachment_upload_abandoned |
  attachment_indexing_failed; payload per D "Attachment Deletion" Phase 1 table): chat soft-deleted
  → Ok without action; no provider_file_id → mark done; delete primary (2xx/404 success; else
  increment `cleanup_attempts`, `last_cleanup_error`, Retry; at `cleanup_worker.max_attempts` →
  `cleanup_status=failed`, Reject); delete secondary best effort; mark done.
- chat cleanup (payload `{tenant_id, chat_id, system_request_id, reason: "chat_soft_delete",
  chat_deleted_at}`): chat not soft-deleted → Reject; for each attachment `cleanup_status=pending`
  delete provider file (+secondary) with per-attachment attempt accounting (failed terminal at max);
  when none pending and `chat_vector_stores` row exists → (metric if any failed) delete vector store
  (2xx/404 → delete row; else Retry until delivery reaches max_attempts → Reject); DB errors Retry.
- thread summary (§10.2).

### 10.2 Thread summary (D§3.6 "Thread Summary Update", P§5.3)

Trigger in finalization of completed turns when enabled: urgent (`messages_truncated`) or
proactive (no summary and `assembled_context_tokens >= effective_budget * pct / 100`,
`effective_budget = min(max_input_tokens(0=none), context_window - max_out)`). Frozen target =
latest non-deleted message not belonging to the causing turn; none → nothing. Payload: tenant_id,
chat_id, system_request_id (v4, stable), base frontier (current summary frontier or null), frozen
target, system_task_type. Handler: resolve summary model with enabled filter (missing/disabled →
Reject `model_unavailable`); load messages in `(base, target]` not deleted/compressed; build prompt
per D B.5.5 (system prompt precedence: catalog `thread_summary_prompt` → config → built-in; user
prompt with existing summary block, `User:`/`Assistant:` entries, content limit with `...`,
analysis instruction); fit to budget (drop oldest ceil(n/5), keep ≥2); non-streaming call; parse
`<summary>`; empty → Retry (`empty_summary`); provider error → Retry (Reject at max_attempts);
commit txn: CAS on base frontier, target message not deleted, upsert `thread_summaries`, mark range
`is_compressed = true`, enqueue system usage event; frontier moved → Ok (cas conflict);
`token_estimate` = output − reasoning or ceil(bytes/4).

### 10.3 Orphan watchdog (D§4 "Orphan Turn Watchdog", B.9.1)

Every `scan_interval_secs` (leader only): ≤100 candidates `state=running AND deleted_at IS NULL AND
COALESCE(last_progress_at, started_at) <= now - timeout`; per candidate one txn: orphan CAS
(re-checks the predicate) → `failed`/`orphan_timeout`; estimated settlement (skip with warning when
reserve fields NULL); usage event (`aborted`/`estimated`, `selected_model = effective_model`) and
`turn_failed` audit (quota decision "unknown").

### 10.4 Upload reaper (B.9.5)

Every `scan_interval_secs` (leader only): ≤100 rows `status IN (pending, uploaded) AND deleted_at
IS NULL AND cleanup_status IS NULL AND updated_at < now - stale_after`; per row txn: CAS to
`failed`/`upload_abandoned`; if `provider_file_id` → `cleanup_status=pending` + attachment cleanup
event `attachment_upload_abandoned`.

## 11. Attachments (D§3.3 Upload, D§3.6 File Upload, ADR-0007)

Handler: PEP `upload_attachment` + scoped chat load (404 chat) → resolve chat model without enabled
filter (400 INVALID_MODEL / 500) → compute limits `min(rag kind limit, model max_file_size_mb)` →
acquire concurrency permit (none → 503 Retry-After 5) → parse multipart streaming (`multer`):
missing boundary → 400 BOUNDARY_REQUIRED; unreadable → MULTIPART_ERROR; no `file` field →
MISSING_FILE; part without content type → MISSING_CONTENT_TYPE. Filename default `upload`, truncate
to 255 keeping extension; `application/octet-stream` → infer from extension; `text/csv` → `text/plain`
when `allow_csv_upload`. MIME allowlist (P§13 resolved): pdf, docx, pptx, xlsx, text/plain,
text/markdown, text/html, application/json, source code (python, java, javascript, typescript,
rust, go, csharp, ruby, sql variants), png, jpeg, webp, gif; else 400 UNSUPPORTED_CONTENT_TYPE.
Kind/purpose per D table; `disable_images` → 400 images FEATURE_DISABLED; XLSX-only with code
interpreter unavailable (kill switch or chat model) → 400 `field_violations[file].reason =
CODE_INTERPRETER_UNAVAILABLE`. Byte counting while streaming; over limit → 400 FILE_TOO_LARGE
(`content_length`). Per-chat limits (non-deleted, non-failed; documents count; total size incl.
images) → 429 document_limit / storage_limit. Insert row `pending` → provider upload → `uploaded` +
provider_file_id → documents with file_search: vector store protocol (D§3.7 chat_vector_stores
creation protocol incl. placeholder/CAS/stale reclaim; backend mismatch → 409 provider_mismatch),
add file, poll 250 ms doubling to 2 s until 25 s after start; completed → ready; failed/other read
error → failed `indexing_failed`, delete provider file best effort, 503 Retry-After 10; still
in_progress → 201 `uploaded` + background task (20 s rounds, refresh `updated_at`, 250 ms doubling
to 5 s, 10 min; completed → ready (3 retries over 7 s); failed/timeout → failed `indexing_failed` +
`cleanup_status=pending` + cleanup event in one txn; stops when row/chat deleted or gear stops).
XLSX → ready. Image → thumbnail (WebP fit-inside, ≤ max_bytes, skip on size/pixel limits) → ready.
Provider failure → row failed `upload_failed` → 503 Retry-After 10. Anthropic chats: secondary copy
of images (≤ max_decode_bytes) to Anthropic Files.

GET attachment: scoped chat; attachment of this chat, not deleted, uploaded_by caller → else 404
attachment. DELETE: same 404 rules (uploader check before idempotency); already deleted → 204;
referenced by any `message_attachments` row → 409 already_exists `attachment_locked`; else txn:
soft-delete + enqueue attachment cleanup (`attachment_deleted`) → 204.

Chat delete: txn soft-delete chat (`deleted_at`, `updated_at`), set `cleanup_status=pending`,
`cleanup_updated_at=now` on every non-deleted attachment of the chat whose `cleanup_status IS NULL`,
enqueue chat cleanup; 204; second delete → 404.

## 12. Authorization (D§3.8)

Resource types: chat `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~` (properties
owner_tenant_id, owner_id, id), model `gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~`
(require_constraints=false), user quota `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~`.
Actions per the matrix. Every chat-scoped operation adds owner predicate (`user_id =
subject_id`) and tenant predicate to the compiled scope. `EnforcerError::Denied | CompileFailed`
→ 403 `AUTHZ_DENIED`; `EvaluationFailed` → 503 Retry-After 5. Child tables are queried with the
tenant from the scope and the chat id from the scoped chat query.

## 13. Observability

Register OTel instruments named in D "Metrics (OTLP)" (prefix from config) and record the
"Emitted" series at the obvious points; labels never include tenant/user/chat/request ids. Metric
correctness is not E2E-checkable; keep it light.

## 14. Testing strategy (every acceptance-criteria item implemented AND tested)

1. **Unit tests** (in-crate `#[cfg(test)]` / `_tests.rs`): config validation; credits arithmetic;
   estimation; cascade truth tables; billing outcome derivation (each D§5.8 row); settlement
   (actual/estimated/overshoot cap); context assembly & truncation; tool list & guards; sanitizer;
   SSE parser + OpenAI event translation; other adapters' request building/parsing; error mapping
   (status + reason for every ADR-0004 row); MIME/filename handling; thumbnail; quota status math;
   summary prompt/parse.
2. **Integration tests** (`gears/mini-chat/mini-chat/tests/`): in-memory SQLite with real
   migrations, fake `ServiceGatewayClientV1` (scripted SSE/file/vector-store responses, records
   requests), fake PDP (`AuthZResolverApi` allow/deny/fail), static-like policy & audit plugin
   fakes; drive the REST router (OperationBuilder routes + injected `SecurityContext`) to assert
   HTTP status, Problem fields, SSE event sequences, DB rows, outbox rows and recorded provider
   requests for every acceptance-criteria item.
3. **Black-box E2E** (`testing/e2e/suites/mini_chat_dev/`, Python + pytest + a local
   OpenAI-compatible mock server): start the real debug server with a config like the hidden one
   (provider pointed at the mock with `use_http`), and exercise CRUD, streaming, replay/409,
   attachments, quota downgrade/429 (by seeding `quota_usage` in the SQLite DB), mutations,
   reactions, models, quota status, cleanup outbox rows and provider DELETE calls.
4. A traceability table `gears/mini-chat/mini-chat/tests/ACCEPTANCE_TRACE.md` maps each
   acceptance-criteria bullet to its tests.

## 15. Risks and decisions log

- Hidden-suite config shape unknown beyond DESIGN keys → every section defaulted; strict
  unknown-key rejection only where DESIGN says so.
- OAGW alias resolution per tenant → S2S context for all proxy calls (§1.2).
- `messages:stream` axum path with `:` → verified by test.
- `ModelTier` casing differs between yaml and DESIGN → accept both.
- Only `openai_responses` gets E2E coverage (ADR-0005); other adapters unit-tested.
- Knowledge search: implement config + validation + tool/loop per D§4 "Knowledge Search" as the last
  feature (off by default; not in acceptance criteria beyond tool availability).
