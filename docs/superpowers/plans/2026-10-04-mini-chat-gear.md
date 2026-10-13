# Mini Chat Gear + SDK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the `mini-chat` gear (`gears/mini-chat/mini-chat`) and its SDK crate (`gears/mini-chat/mini-chat-sdk`) so the example server serves the full P1 contract of `gears/mini-chat/docs/` and every item of `acceptance-criteria.md` is implemented and tested.

**Architecture:** Layered gear (api → domain → infra) per DESIGN §1.3; in-process `llm_provider` adapters calling OAGW `proxy_request`; SQLite/PG persistence via Secure ORM; transactional outbox (5 queues) for usage, audit, cleanup and thread summaries; CAS-guarded turn finalization; bundled static policy/audit plugin gears inside the gear crate.

**Tech Stack:** Rust 1.98 (edition 2024), toolkit (gear macro, OperationBuilder, canonical errors, OData), toolkit-db (Secure ORM, outbox), sea-orm 2, axum 0.8 SSE, oagw-sdk, authz/authn SDKs, multer, image (webp), opentelemetry.

**Spec:** `docs/superpowers/specs/2026-10-04-mini-chat-gear-design.md` (and, as source of truth, `gears/mini-chat/docs/{PRD.md,DESIGN.md,ADR/*,acceptance-criteria.md}`). Platform API reference notes: `docs/superpowers/refs/01..05-*.md` (01 gear lifecycle & plugins, 02 DB/Secure ORM/outbox, 03 OAGW/authn/authz, 04 REST/errors/OData/SSE, 05 mini-chat OpenAPI dump of `docs/api/api.json`). **Every implementer reads the spec, the DESIGN sections named in their task, and the refs named in their task before coding.**

## Global Constraints

- Build must succeed: `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore` (offline; only crates already in `Cargo.lock`; adding workspace crates such as `toolkit-odata-macros`, `tower`, `http-body-util`, `tempfile`, `arc-swap`, `chrono` as deps/dev-deps is allowed).
- SDK package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`, dir `gears/mini-chat/mini-chat-sdk`, version `0.4.5`, workspace member + `[workspace.dependencies] mini-chat-sdk = { package = "cf-gears-mini-chat-sdk", version = "0.4.5", path = "gears/mini-chat/mini-chat-sdk" }`.
- Gear name `mini-chat`; plugin gear names `static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin`, in public modules `mini_chat::infra::plugins::static_model_policy` / `::static_audit`.
- REST base path `{url_prefix}/v1` with `url_prefix` default `/mini-chat`; 19 operations exactly as `docs/api/api.json` (operation ids `mini_chat.*`, tags, status sets, `Retry-After` header on 503).
- REST errors are canonical `Problem`s (ADR-0004); resource types `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`; SSE `error` data is `{code, message}`.
- DB table/column names exactly as DESIGN §3.7; SQLite + PostgreSQL migrations; timestamps `time::OffsetDateTime`; outbox tables from `toolkit_db::outbox::outbox_migrations()` (default prefix `toolkit_outbox`).
- Outbox queue names (config defaults): `mini-chat.usage_snapshot`, `mini-chat.attachment_cleanup`, `mini-chat.chat_cleanup`, `mini-chat.thread_summary`, `mini-chat.audit`; `outbox.num_partitions` default 4.
- No provider identifier (`file-…`, `file_…`, `assistant-…`, `vs_…`, `resp_…`, `chatcmpl-…`, `cmpl-…`, `msg_…`) ever reaches an API body, SSE payload or error message.
- All OAGW calls use the S2S `SecurityContext` obtained from authn-resolver client credentials (spec §4a.3); `user` field / metadata still carry the request's tenant/user.
- Tests live in-crate (`#[cfg(test)]` modules; dylint DE1101 is relaxed for `gears/mini-chat`); shared test helpers in `src/test_support/` (`#[cfg(test)]`). Never put tests in `testing/e2e/suites/mini_chat/`.
- Never stop processes with `pkill -f`/`pgrep -f`/`ps aux | grep`; record and kill by pid.
- Commit after every task (`git add` only files of the task).

## Review Focus

1. Client disconnects mid-stream (SSE receiver dropped) → turn `cancelled`, partial text persisted when non-empty, estimated settlement + `aborted` usage event, no SSE `error` written. Test owned by Task 15 (`disconnect_mid_stream_cancels_turn_and_settles_estimated`).
2. Two concurrent `messages:stream` requests for one chat → exactly one stream, the other 409 `turn_already_running`, and no leftover reserve in `quota_usage`. Test owned by Task 14 (`concurrent_sends_one_wins_no_reserve_leak`).
3. Provider answers the chat call with a non-SSE HTTP error (500 JSON body with a `file-…` id, 429 with `Retry-After: 7`, OAGW gateway 504) → SSE `stream_started` then terminal `error` (`provider_error` sanitized / `rate_limited` with `retry in 7s` / `provider_timeout`), turn `failed`, estimated settlement. Test owned by Task 15 (`provider_http_errors_map_to_sse_codes`).
4. Multibyte UTF-8 input: emoji/CJK content estimated by UTF-8 bytes for `INPUT_TOO_LONG`; upload filenames > 255 chars truncated on a char boundary keeping the extension. Tests owned by Task 14 (`input_too_long_counts_utf8_bytes`) and Task 18 (`long_multibyte_filename_truncated_keeping_extension`).
5. Restart recovery: a `running` turn with stale `last_progress_at` and a `pending`/`uploaded` attachment with stale `updated_at` (rows written directly to the DB, as a crashed process would leave them) are finalized/failed by the watchdog/reaper. Tests owned by Task 21 (`watchdog_finalizes_seeded_stale_turn`, `reaper_fails_seeded_stale_upload`).

---

## File Structure

```
gears/mini-chat/mini-chat-sdk/
  Cargo.toml, README.md
  src/lib.rs            re-exports
  src/gts.rs            plugin spec types (GTS), resource-type consts
  src/error.rs          PolicyPluginError, PublishError, AuditPluginError
  src/credits.rs        credits_micro_checked + CreditsError
  src/plugin_api.rs     MiniChatModelPolicyPluginClientV1, MiniChatAuditPluginClientV1
  src/models/{mod,policy,usage,audit}.rs
gears/mini-chat/mini-chat/src/
  lib.rs gear.rs wiring.rs metrics.rs
  config/{mod.rs,providers.rs}
  api/{mod.rs,routes.rs,license.rs,error.rs,sse.rs,state.rs}
  api/dto/{chats,messages,attachments,turns,models,quota,reactions,stream}.rs
  api/handlers/{chats,messages,stream,turns,attachments,models,quota,reactions}.rs
  domain/{mod.rs,error.rs,authz.rs,sanitize.rs,model_service.rs,chat_service.rs,message_service.rs,
          reaction_service.rs,turn_service.rs,context.rs,tools.rs,thread_summary.rs}
  domain/quota/{mod.rs,estimate.rs,periods.rs,preflight.rs,ledger.rs,billing.rs,status.rs}
  domain/stream/{mod.rs,events.rs,setup.rs,replay.rs,provider_task.rs,relay.rs,finalize.rs}
  domain/attachment/{mod.rs,mime.rs,upload.rs,indexing.rs,thumbnail.rs}
  infra/db/{mod.rs,entity/*.rs,migrations/{mod.rs,m0001_initial.rs},repo/*.rs}
  infra/outbox/{mod.rs,payloads.rs,usage.rs,audit.rs,attachment_cleanup.rs,chat_cleanup.rs,thread_summary.rs}
  infra/gateways/{mod.rs,policy.rs,audit.rs}
  infra/llm/{mod.rs,types.rs,resolver.rs,s2s.rs,provisioning.rs,errors.rs,openai_responses.rs,
             openai_chat.rs,vllm_responses.rs,anthropic.rs}
  infra/storage/{mod.rs,openai.rs,azure.rs,anthropic_files.rs,knowledge.rs}
  infra/workers/{mod.rs,leader.rs,orphan_watchdog.rs,upload_reaper.rs}
  infra/plugins/{mod.rs,static_model_policy/{mod,config,service}.rs,static_audit/{mod,config,service}.rs}
  test_support/{mod.rs,db.rs,pdp.rs,gateway.rs,catalog.rs,app.rs}     (#[cfg(test)])
testing/mini_chat_blackbox/   black-box Python suite (Task 23)
```

## Shared Interfaces (decided once; tasks reference these names)

**SDK (Task 1)** — all `serde` types:
- `ModelTier { Premium, Standard }` serialized lowercase, deserializes `premium|Premium|standard|Standard`.
- `ModelCatalogEntry` fields per DESIGN Appendix A.1 (required: `id, provider_model_id, display_name, provider_id, provider_display_name, tier, context_window: u32, max_output_tokens: u32, max_input_tokens: u32, input_tokens_credit_multiplier_micro: i64, output_tokens_credit_multiplier_micro: i64, max_num_results: u32, general_config`; defaulted: `description, icon, multiplier_display, system_prompt, thread_summary_prompt: String` (""), `enabled: bool` (false), `multimodal_capabilities: Vec<String>`, `estimation_budgets: EstimationBudgets`, `web_search_context_size: WebSearchContextSize` (low), `max_tool_calls: u32` (2), `preference: Option<ModelPreference>`). Helper methods: `fn is_premium(&self) -> bool`, `fn supports_vision(&self) -> bool` (`VISION_INPUT`), `fn tool_support(&self) -> &ModelToolSupport`.
- `EstimationBudgets { bytes_per_token_conservative: u32=4, fixed_overhead_tokens: u32=100, safety_margin_pct: u32=10, image_token_budget: u32=1000, tool_surcharge_tokens: u32=500, web_search_surcharge_tokens: u32=500, code_interpreter_surcharge_tokens: u32=1000, minimal_generation_floor: u32=50 }` (`#[serde(default)]`).
- `ModelGeneralConfig { r#type: String, available_from: String, max_file_size_mb: u32, api_params: ModelApiParams, features: ModelFeatures, tool_support: ModelToolSupport, supported_endpoints: ModelSupportedEndpoints }`; `ModelApiParams { temperature, top_p, frequency_penalty, presence_penalty: Option<f64>, stop: Vec<String>, extra_body: Option<serde_json::Map>, reasoning_effort: Option<String> }`; `ModelToolSupport { web_search, file_search, image_generation, code_interpreter, mcp: bool }`.
- `KillSwitches { disable_premium_tier, force_standard_tier, disable_web_search, disable_file_search, disable_images, disable_code_interpreter: bool }` (all required on deserialize).
- `PolicySnapshot { policy_version: i64, model_catalog: Vec<ModelCatalogEntry>, kill_switches: KillSwitches }`, `PolicyVersionInfo { policy_version: i64, generated_at: OffsetDateTime }`, `TierLimits { limit_daily_credits_micro: i64, limit_monthly_credits_micro: i64 }`, `UserLimits { user_id: Uuid, policy_version: i64, standard: TierLimits, premium: TierLimits }`, `UserLicenseStatus { active: bool }`.
- `UsageTokens { input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_tokens: i64 }`; `UsageEvent` exactly the DESIGN §3.2/A.3 field list (`user_id`, `turn_id`, `system_task_type` are `Option` with `skip_serializing_if`; `usage: Option<UsageTokens>` serialized as `null` when None).
- `AuditEvent` (`#[serde(tag = "kind", rename_all = "snake_case")]`): `Turn(TurnAuditEvent)`, `Mutation(TurnMutationAuditEvent)`. `TurnAuditEvent { event_type: "turn_completed"|"turn_failed", timestamp, tenant_id, requester_type, actor_user_id: Option<Uuid>, chat_id, turn_id, request_id, selected_model, effective_model, terminal_state, error_code: Option<String>, usage: Option<UsageTokens>, latency: TurnLatency { ttft_ms: Option<u64>, total_ms: u64 }, tool_calls: ToolCalls { web_search_calls: u32, file_search_calls: u32 }, policy_decisions: PolicyDecisions { quota: QuotaPolicyDecision { decision: String, downgrade_from: Option<String>, downgrade_reason: Option<String> }, license: Option<serde_json::Value> }, prompt: String, response: String, attachments: Vec<serde_json::Value>, quota_scope: Option<String>, trace_id: Option<String> }`. `TurnMutationAuditEvent { event_type: "turn_retry"|"turn_edit"|"turn_delete", tenant_id, actor_user_id, chat_id, original_request_id: Option<Uuid>, new_request_id: Option<Uuid>, request_id: Option<Uuid>, timestamp }`.
- `credits_micro_checked(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> Result<i64, CreditsError>` (DESIGN §5.3, per-component ceil_div, token bound 10_000_000, multiplier bound 1..=10_000_000_000, distinct errors `InvalidTokenCount`, `ZeroMultiplier`, `MultiplierTooLarge`, `Overflow`).
- Plugin traits (async_trait, Send+Sync): `MiniChatModelPolicyPluginClientV1 { get_current_policy_version(&self, user_id: Uuid) -> Result<PolicyVersionInfo, PolicyPluginError>; get_policy_snapshot(&self, user_id: Uuid, policy_version: i64) -> Result<PolicySnapshot, PolicyPluginError>; get_user_limits(&self, user_id: Uuid, policy_version: i64) -> Result<UserLimits, PolicyPluginError>; publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>; check_user_license(&self, user_id: Uuid) -> Result<UserLicenseStatus, PolicyPluginError> /* default Ok(active:false) */ }`; `MiniChatAuditPluginClientV1 { emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> }`. Errors: `PolicyPluginError::{NotFound(String), Unavailable(String), Internal(String)}`, `PublishError::{Transient(String), Permanent(String)}`, `AuditPluginError::{Transient(String), Permanent(String), PluginTimeout}`.
- GTS specs: `MiniChatModelPolicyPluginSpecV1` (`gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~")`), `MiniChatAuditPluginSpecV1` (`…~cf.core.mini_chat.audit_plugin.v1~`); consts `CHAT_RESOURCE_TYPE = gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~")`, `MODEL_RESOURCE_TYPE = gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~")`, `USER_QUOTA_RESOURCE_TYPE = gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~")`.

**Gear core (Tasks 2–6)**:
- `crate::config::MiniChatConfig` (Task 2) — sections and defaults listed in Task 2.
- `crate::domain::error::DomainError` (Task 5) — the single error enum; `impl From<DomainError> for CanonicalError` in `api/error.rs`.
- `crate::domain::authz::Authz` (Task 5): `chat_scope(&self, ctx: &SecurityContext, action: ChatAction, chat_id: Option<Uuid>) -> Result<AccessScope, DomainError>` (PDP scope `.ensure_owner(ctx.subject_id())`), `create_scope(&self, ctx) -> Result<AccessScope, DomainError>`, `model_permission(&self, ctx, action: &str) -> Result<(), DomainError>`, `quota_scope(&self, ctx) -> Result<AccessScope, DomainError>`. `ChatAction` enum with `as_str()` values `create, list, read, update, delete, list_messages, send_message, upload_attachment, read_attachment, delete_attachment, read_turn, retry_turn, edit_turn, delete_turn, set_reaction, delete_reaction`.
- `crate::infra::gateways::policy::PolicyGateway` trait (Task 6): `current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError>`, `snapshot(&self, user_id: Uuid, version: i64) -> Result<PolicySnapshot, DomainError>`, `user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError>`, `publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome>` (`PublishOutcome::{Retry, Reject(String)}`); impls `PluginPolicyGateway` (types-registry + ClientHub) and `DirectPolicyGateway(Arc<dyn MiniChatModelPolicyPluginClientV1>)` (tests).
- `crate::infra::gateways::audit::AuditGateway` trait (Task 6): `deliver(&self, ev: AuditEvent) -> AuditDelivery` (`Ok | Dropped | Retry | Reject(String)`); impls `PluginAuditGateway`, `DirectAuditGateway`.
- `crate::domain::model_service::ModelService` (Task 6): `list_visible(ctx) -> Vec<ModelCatalogEntry>`, `get_visible(ctx, id) -> Result<ModelCatalogEntry, DomainError::ModelNotFound>`, `resolve_for_create(ctx, requested: Option<&str>) -> Result<ModelCatalogEntry, DomainError::InvalidModel>`, `resolve_chat_model(ctx, model_id: &str) -> Result<(PolicySnapshot, ModelCatalogEntry), DomainError>` (no enabled filter; missing → `InvalidModel`).
- `crate::infra::outbox::OutboxEnqueuer` (Task 7): `enqueue(&self, tx: &DbTx<'_>, rec: OutboxRecord) -> Result<Wake, DomainError>`; `OutboxRecord::{usage(&UsageEvent), audit(&AuditEvent), attachment_cleanup(&AttachmentCleanupEvent), chat_cleanup(&ChatCleanupEvent), thread_summary(&ThreadSummaryTask)} -> Result<OutboxRecord, DomainError>` (payload > 64 KiB → `DomainError::OutboxPayloadTooLarge`); `partition_for(key: Uuid, n: u32) -> u32` (last two bytes big-endian `% n`).
- `crate::infra::llm::S2sContext` (Task 9): `get(&self) -> Result<SecurityContext, DomainError::ProviderUnavailable>`, `set(SecurityContext)`.
- `crate::infra::llm::resolver::ProviderResolver` (Task 9): `chat_target(&self, provider_id: &str, tenant_id: Uuid) -> Result<ChatTarget, DomainError>`, `storage_target(&self, provider_id: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError>` (follows `rag_provider`), `storage_by_backend(&self, backend: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError>`. `ChatTarget { provider_id, kind: ProviderKind, alias, api_path_template }` with `fn uri_for(&self, provider_model_id: &str) -> String` (`/{alias}{api_path}` with `{model}` replaced); `StorageTarget { provider_id, storage_kind: StorageKind, alias, api_version: Option<String>, storage_backend }` with `fn uri(&self, path: &str) -> String` (`/{alias}{prefix}{path}` + `?api-version=` for azure; prefix `/v1` openai, `/openai` azure).
- `crate::infra::llm::types` (Task 10): `ProviderRequest`, `InputItem`, `ContentPart`, `ToolSpec`, `ProviderEvent`, `ProviderUsage`, `ProviderError`, `ProviderErrorKind`, `RawCitation`, `CompletionResult`, trait `ChatAdapter` (all defined in Task 10).
- `crate::infra::storage::{FileStorage, VectorStores, IndexStatus, StorageError}` (Task 11).
- `crate::domain::quota::QuotaService` (Task 12), `crate::domain::context::{assemble, ContextInput, ContextPlan}` and `crate::domain::tools::build_tools` (Task 13; `ToolGates` lives in `crate::domain::quota`, Task 12), `crate::domain::stream::{StreamService, StreamEvent, StreamOutcome}` (Tasks 14–15).
- `crate::api::state::AppServices` (Task 6) — `Arc` holder of every service; extended by each later task; built only in `crate::wiring::build_services(deps: ServiceDeps) -> Result<Arc<AppServices>>` (used by `gear.rs` and by `test_support::app::TestApp`).
- `crate::test_support::app::TestApp` (Task 6, extended later): `TestApp::builder().config(MiniChatConfig).catalog(Vec<ModelCatalogEntry>).kill_switches(KillSwitches).limits(TierLimits std, TierLimits premium).pdp(PdpMode).build().await -> TestApp { router, services, db, gateway: Arc<FakeGateway>, audit: Arc<RecordingAudit>, usage: Arc<RecordingPolicy> }`; helpers `ctx(tenant: Uuid, user: Uuid) -> SecurityContext`, `TestApp::call(&self, method, uri, ctx, Option<serde_json::Value>) -> TestResponse { status, headers, json: serde_json::Value }`, `TestApp::call_raw(&self, http::Request<Body>) -> http::Response<Body>`, `TestApp::stream(&self, method, uri, ctx, body) -> Result<Vec<SseFrame{event, data: Value}>, TestResponse>`, `TestApp::wait_until(desc, async closure -> bool)` (polls 10 ms→100 ms up to 5 s).
- `crate::test_support::gateway::FakeGateway` (Task 6, extended Task 10/11): implements `ServiceGatewayClientV1`; provisioning CRUD stored in memory; `proxy_request` matches registered `Responder`s by `(Method, path substring)` in order, records `RecordedRequest { method, uri, headers, body: Bytes, json: Option<Value> }`; responders: `Json(status, Value)`, `JsonWithHeaders(status, Vec<(name,value)>, Value)`, `Sse(Vec<SseScript>)` where `SseScript::{Event(name, Value), Raw(String), Delay(Duration), Hang}`, `Err(CanonicalError)`, `GatewayStatus(u16)` (sets `ErrorSource::Gateway`), sequence responders consumed in order.

---
## Task Map

| # | Task | Depends on |
|---|---|---|
| 1 | SDK crate | — |
| 2 | Gear skeleton, config, metrics module | 1 |
| 3 | Bundled static plugins | 1, 2 |
| 4 | DB entities + migrations | 2 |
| 5 | DomainError → Problem mapping, Authz PEP, sanitizer | 2 |
| 6 | Policy/audit gateways, ModelService, REST scaffolding, Models API, test harness | 3, 4, 5 |
| 7 | Outbox infrastructure | 4, 6 |
| 8 | Chat CRUD API | 6, 7 |
| 9 | Provider resolver, S2S context, OAGW provisioning | 2, 6 |
| 10 | LLM adapter types + `openai_responses` adapter | 9 |
| 11 | File storage + vector stores | 9 |
| 12 | Quota service + quota status API | 6, 7 |
| 13 | Context assembly + tool builder | 10, 12 |
| 14 | Stream setup, replay, basic provider task + finalization, `messages:stream` | 8, 10, 12, 13 |
| 15 | Full provider task / relay / finalization semantics | 14 |
| 16 | Messages list, reactions, turn status APIs | 14 |
| 17 | Turn mutations (retry/edit/delete) | 15, 16 |
| 18 | Attachments (upload/get/delete, indexing, thumbnails) | 11, 14 |
| 19 | Outbox handlers: usage, audit, attachment cleanup, chat cleanup | 7, 11, 18 |
| 20 | Thread summary trigger + handler | 15, 19 |
| 21 | Orphan watchdog, upload reaper, leader election | 15, 18, 19 |
| 22 | Other adapters (chat completions, vLLM, Anthropic) + knowledge search | 10, 15 |
| 23 | Final gear wiring, server build, black-box suite | all |
| 24 | Acceptance-criteria traceability audit | all |

Test command for the gear: `cargo test -p cf-gears-mini-chat --lib <filter>`; SDK: `cargo test -p cf-gears-mini-chat-sdk`.

---

### Task 1: SDK crate `cf-gears-mini-chat-sdk`

**Files:**
- Create: `gears/mini-chat/mini-chat-sdk/{Cargo.toml,README.md,src/lib.rs,src/gts.rs,src/error.rs,src/credits.rs,src/plugin_api.rs,src/models/{mod,policy,usage,audit}.rs}`
- Modify: `Cargo.toml` (workspace members + `[workspace.dependencies]`), `gears/mini-chat/mini-chat/Cargo.toml` (add `mini-chat-sdk = { workspace = true }`)

**Interfaces:**
- Produces: everything under "Shared Interfaces → SDK".

- [ ] **Step 1: Read** ref 01 §2–3 (GTS spec + SDK Cargo conventions), DESIGN §3.2 "System Task Attribution Rules" (lines 481–580), §5.3 (4494–4632), Appendix A (6485–6800).
- [ ] **Step 2: Write failing tests** (`#[cfg(test)]` in the respective files):
  - `credits::tests::per_component_ceil_div` — `credits_micro_checked(1, 1, 1_500_000, 1_500_000) == Ok(4)` (ceil(1.5)+ceil(1.5)); `(1000, 500, 2_500_000_000, 2_500_000_000) == Ok(3_750_000)`.
  - `credits::tests::bounds` — tokens `10_000_001` → `Err(InvalidTokenCount)`; mult `0` → `Err(ZeroMultiplier)`; mult `10_000_000_001` → `Err(MultiplierTooLarge)`; negative tokens → `Err(InvalidTokenCount)`.
  - `models::policy::tests::catalog_entry_defaults` — deserialize the minimal required entry from YAML-equivalent JSON (copy one `model_catalog` item from `config/mini-chat.yaml`, `tier: Premium`) → `enabled` true as given, `max_tool_calls == 2` when omitted, `web_search_context_size == Low` when omitted, `estimation_budgets == EstimationBudgets::default()` when omitted, `tier` serializes back as `"premium"`.
  - `models::policy::tests::kill_switches_require_all_fields` — JSON missing `disable_images` fails to deserialize.
  - `models::usage::tests::system_task_event_omits_user_and_turn` — serialized system-task `UsageEvent` has no `user_id`/`turn_id` keys, has `"system_task_type":"thread_summary_update"`; a user-turn event has no `system_task_type` key and `"usage": null` when usage is None.
  - `models::audit::tests::audit_event_round_trip` — `AuditEvent::Turn(..)` and `AuditEvent::Mutation(..)` round-trip through JSON with `kind` tag.
- [ ] **Step 3: Run** `cargo test -p cf-gears-mini-chat-sdk` → FAIL (crate missing / not compiling).
- [ ] **Step 4: Implement** the crate per Shared Interfaces. Cargo deps: `async-trait, thiserror, serde, serde_json, uuid, time, schemars, gts, toolkit-gts, toolkit` (all `workspace = true`). `README.md` one paragraph. `#![doc = include_str!("../README.md")]`.
- [ ] **Step 5: Run** `cargo test -p cf-gears-mini-chat-sdk` → PASS; `cargo check -p cf-gears-mini-chat` → OK.
- [ ] **Step 6: Commit** `feat(mini-chat-sdk): add SDK crate with policy, usage, audit models and plugin APIs`.

---

### Task 2: Gear skeleton, configuration, metrics module

**Files:**
- Create: `src/config/{mod.rs,providers.rs}`, `src/metrics.rs`, `src/gear.rs`, `src/wiring.rs` (empty `ServiceDeps`/`build_services` stub), module stubs `src/{api,domain,infra}/mod.rs`, `src/infra/plugins/{mod.rs,static_audit/mod.rs,static_model_policy/mod.rs}` (public empty modules kept compiling)
- Modify: `src/lib.rs`, `gears/mini-chat/mini-chat/Cargo.toml` (dev-deps `tower`, `http-body-util`, `tempfile`, `tokio` with `test-util`; add `arc-swap`, `chrono` (runtime, for outbox `created_at`), `toolkit-odata-macros` if used)

**Interfaces:**
- Produces: `MiniChatConfig` (+ section structs below), `MiniChatConfig::validate(&self) -> Result<(), String>`, `MiniChatConfig::normalize(&mut self)` (fills `upstream_alias` per spec §4a.4, see Task 9), `Metrics::new(prefix: &str) -> Metrics` with one field per DESIGN "Emitted" + "Declared, deferred" instrument (`metrics.rs`); `MiniChatGear` declared with `#[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver, authz_resolver, oagw], capabilities = [db, rest, stateful], lifecycle(entry = "serve", stop_timeout = "30s", await_ready))]` and `OnceLock` state; `init` loads `ctx.config_expanded_or_default::<MiniChatConfig>()`, validates, logs deprecated-field warnings; `register_rest` registers nothing yet; `migrations()` returns empty until Task 4; `serve` notifies ready and waits for cancel.

Config sections and defaults (B.1–B.9; names exact):
- top: `url_prefix: "/mini-chat"`, `vendor: "constructorfabric"`, `client_credentials { client_id, client_secret: String }` (required non-empty; secret redacted in Debug), `metrics { prefix: "" }`, `providers: BTreeMap<String, ProviderEntry>` default `{"openai": {kind: openai_responses, host: "api.openai.com", api_path: "/v1/responses", auth_plugin_type: APIKEY plugin id, auth_config: {header: "Authorization", prefix: "Bearer ", secret_ref: "cred://openai-key"}, storage_kind: openai}}`.
- `ProviderEntry { kind: ProviderKind (openai_responses|openai_chat_completions|vllm_responses|anthropic_messages, snake_case), host, port: Option<u16>, use_http: bool, upstream_alias: Option<String>, api_path: String="/v1/responses", auth_plugin_type: Option<String>, auth_config: Option<BTreeMap<String,String>>, storage_kind: Option<StorageKind (openai|azure)>, storage_backend: Option<String>, api_version: Option<String>, rag_provider: Option<String>, tenant_overrides: BTreeMap<String, TenantOverride{host, upstream_alias, auth_plugin_type, auth_config}> }` — `deny_unknown_fields`; `${VAR}` on `host`, `auth_config` (entry and overrides). Validation: host non-empty and chars `[A-Za-z0-9._:\[\]-]`; port ≠ 0; `storage_kind = azure` requires non-blank `api_version` matching `[A-Za-z0-9.-]+`; `rag_provider` must name an entry; an override must set host or upstream_alias. Note: DESIGN marks `storage_kind` required — accept it as `Option` but require it for entries that are used for storage (an entry without own storage needs `rag_provider`; `anthropic_messages` entries require `rag_provider`), so `kind: anthropic_messages` configs without `storage_kind` still validate.
- `streaming { sse_ping_interval_seconds: u16=15 (5..=60), sse_channel_capacity: u16=32 (16..=64), max_output_tokens: u32=32768 }` deny_unknown.
- `estimation_budgets` (all 8 fields, defaults as SDK `EstimationBudgets`): only `minimal_generation_floor` validated (`>0`, `<= streaming.max_output_tokens`); warn per other field ≠ default. deny_unknown.
- `quota { overshoot_tolerance_factor: f64=1.10 (1.0..=1.5), warning_threshold_pct: u8=80 (1..=99), web_search_max_calls_per_message: u32=2 (>0), web_search_daily_quota: u32=75 (>0), code_interpreter_max_calls_per_message: u32=10 (>0), code_interpreter_daily_quota: u32=50 (>0) }` deny_unknown.
- `outbox { queue_name, cleanup_queue_name, chat_cleanup_queue_name, thread_summary_queue_name, audit_queue_name (defaults above, non-empty), num_partitions: u32=4 (power of 2, 1..=64) }` deny_unknown.
- `context { recent_messages_limit: u32=10 (0..=100), web_search_guard: String = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.", file_search_guard: String = "Use file_search to find relevant excerpts in the documents uploaded to this chat when the user asks about them. Cite the documents you use." }` deny_unknown.
- `rag { max_documents_per_chat: u32=50, max_total_upload_mb_per_chat: u32=100, allow_csv_upload: bool=true, max_concurrent_uploads: u16=10 (1..=256), uploaded_file_max_size_kb: u32=25600, uploaded_image_max_size_kb: u32=5120, max_images_per_message: u32=4 }` (all >0) deny_unknown.
- `thumbnail { width: u32=128, height: u32=128, max_bytes: usize=131072, max_pixels: u64=100_000_000, max_decode_bytes: usize=33_554_432 }` (>0) deny_unknown.
- `orphan_watchdog { enabled=true, timeout_secs: u64=300 (90..=3600), scan_interval_secs: u64=60 (1..=3600) }` (unknown keys accepted).
- `upload_reaper { enabled=true, scan_interval_secs: u64=60 (1..=3600), stale_after_secs: u64=300 (60..=86400) }` (unknown accepted).
- `thread_summary_worker { enabled=true, claim_timeout_secs: u64=300 (30..=3600), max_attempts: u32=3 (>0), compression_threshold_pct: u32=80 (1..=99), summary_model_id: String="" (empty ⇒ "gpt-4.1-mini"), summary_system_prompt: String = <DESIGN B.5.5 built-in default>, message_content_limit: usize=4000, reconcile_interval_secs: u64=60 (deprecated) }` (unknown accepted).
- `cleanup_worker { max_attempts: u32=5 (>0), enabled=true, poll_interval_secs=60, reconcile_interval_secs=300, stale_in_progress_timeout_secs=900, batch_size=32 }` (deprecated fields warn when non-default; unknown accepted).
- `knowledge_search { enabled=false, vector_store_id: Option<String>, provider_id: Option<String>, max_calls_per_message: u32=3, top_k: usize=5, max_chunk_chars: usize=2000, guard: String = "Use search_knowledge to look up organization knowledge when the question needs it." }` (vector_store_id + provider_id required when enabled) deny_unknown.

- [ ] **Step 1: Read** ref 01 §1, §1.5–1.6, §4.1; DESIGN Appendix B (6804–7349); `config/mini-chat.yaml`, `config/e2e-local.yaml` mini-chat sections.
- [ ] **Step 2: Write failing tests** in `config/mod.rs`:
  - `config_mini_chat_yaml_parses` — the `gears.mini-chat.config` block of `config/mini-chat.yaml` (with `AZURE_OPENAI_API_HOST` set via `temp_env`) deserializes and validates.
  - `config_e2e_local_minimal_parses` — `{"client_credentials": {...}}` only → defaults: `url_prefix == "/mini-chat"`, one provider `openai`, `quota.web_search_daily_quota == 75`, `outbox.num_partitions == 4`.
  - `config_rejects_unknown_top_level_and_provider_keys` — `{"mcp": {}}` fails; provider with `supports_file_search_filters: true` fails; `streaming.web_search_context_size` fails.
  - `config_worker_sections_accept_unknown_keys` — `orphan_watchdog: {foo: 1}` parses.
  - `config_validation_ranges` — `orphan_watchdog.timeout_secs: 60` → Err; `streaming.sse_ping_interval_seconds: 4` → Err; `azure` without `api_version` → Err; `estimation_budgets.minimal_generation_floor: 0` → Err; `outbox.num_partitions: 3` → Err; `quota.overshoot_tolerance_factor: 1.6` → Err; `rag_provider: "missing"` → Err; host `"a/b"` → Err; empty `client_credentials.client_id` → Err.
- [ ] **Step 3: Run** `cargo test -p cf-gears-mini-chat --lib config` → FAIL.
- [ ] **Step 4: Implement** config, metrics (OpenTelemetry meter from `opentelemetry::global::meter("mini_chat")`; names `{prefix}_{name}` where prefix defaults to `mini_chat`; counters/histograms/up-down counters per DESIGN §4 "Metric series" tables), gear struct + lifecycle stubs. `lib.rs`: `pub mod config; pub mod gear; pub mod api; pub mod domain; pub mod infra; pub mod metrics; pub mod wiring; #[cfg(test)] mod test_support;` and keep `pub mod infra::plugins::{static_audit, static_model_policy}` public.
- [ ] **Step 5: Run** tests → PASS; `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore` → OK.
- [ ] **Step 6: Commit** `feat(mini-chat): gear skeleton, configuration and metrics`.

---

### Task 3: Bundled static plugins

**Files:**
- Create: `src/infra/plugins/static_model_policy/{mod.rs,config.rs,service.rs}`, `src/infra/plugins/static_audit/{mod.rs,config.rs,service.rs}`

**Interfaces:**
- Consumes: SDK traits/specs (Task 1).
- Produces: `StaticModelPolicyService::from_config(cfg: StaticModelPolicyConfig) -> Result<Self, String>` implementing `MiniChatModelPolicyPluginClientV1` (version always 1; `get_user_limits` returns `default_standard_limits`/`default_premium_limits`; `publish_usage` logs at info and returns Ok); `StaticAuditService::new(enabled: bool)` implementing `MiniChatAuditPluginClientV1` (logs event JSON at info when enabled). Gear structs `StaticModelPolicyPlugin` (`name = "static-mini-chat-model-policy-plugin"`, `deps = [types_registry]`) and `StaticAuditPlugin` (`name = "static-mini-chat-audit-plugin"`, `deps = [types_registry]`), each registering `PluginV1::<Spec>::build_registration("cf.core._.static_mini_chat_model_policy.v1" | "cf.core._.static_mini_chat_audit.v1", vendor, priority)` and a scoped client.
- Config: `StaticModelPolicyConfig { vendor="constructorfabric", priority: i16=100, model_catalog: Vec<ModelCatalogEntry> (required when section present — model as `#[serde(deny_unknown_fields)]` struct without `default` on that field, and the gear uses `config_or_default` so an absent section yields an empty catalog), kill_switches: StaticKillSwitches (deny_unknown, each field default false), default_standard_limits: TierLimits {100_000_000, 1_000_000_000}, default_premium_limits: TierLimits {50_000_000, 500_000_000} }` deny_unknown; `StaticAuditConfig { enabled=true, vendor, priority }` deny_unknown.

- [ ] **Step 1: Read** ref 01 §2.4–2.5, §4.1, §7; DESIGN B.1 "Bundled plugins" (6844–6864).
- [ ] **Step 2: Write failing tests:**
  - `static_model_policy::service::tests::serves_catalog_as_version_1` — snapshot `policy_version == 1`, catalog equals config, limits equal defaults, `check_user_license` → `active: false`.
  - `…::rejects_zero_or_huge_multiplier` — `from_config` with `input_tokens_credit_multiplier_micro: 0` → Err; `10_000_000_001` → Err; `estimation_budgets.bytes_per_token_conservative: 0` → Err.
  - `…::config::tests::catalog_required_when_section_present` — `{"vendor":"x"}` fails; `{}`-section absent path handled by `config_or_default` (test `Default` gives empty catalog); unknown kill switch key fails.
  - `static_audit::tests::registers_instance_and_scoped_client` and `static_model_policy::tests::registers_instance_and_scoped_client` — using `types_registry_sdk` `test-util` `MockTypesRegistryClient` (enable feature in dev-deps) in a `ClientHub`, run gear `init` with a `GearCtx` built as in ref 01 §7; assert `hub.try_get_scoped::<dyn ...>(&ClientScope::gts_id(id))` is Some.
- [ ] **Step 3: Run** `cargo test -p cf-gears-mini-chat --lib plugins` → FAIL.
- [ ] **Step 4: Implement.**
- [ ] **Step 5: Run** → PASS; build the server (command in Global Constraints) → OK.
- [ ] **Step 6: Commit** `feat(mini-chat): bundled static model-policy and audit plugins`.

---

### Task 4: Database entities and migrations

**Files:**
- Create: `src/infra/db/{mod.rs,entity/{mod,chats,messages,chat_turns,attachments,message_attachments,thread_summaries,chat_vector_stores,quota_usage,message_reactions}.rs,migrations/{mod.rs,m0001_initial.rs}}`, `src/test_support/{mod.rs,db.rs}`
- Modify: `src/gear.rs` (`migrations()` returns gear migrations + `toolkit_db::outbox::outbox_migrations()`)

**Interfaces:**
- Produces: SeaORM entities with `#[derive(Scopable)]`: `chats` `#[secure(tenant_col="tenant_id", resource_col="id", owner_col="user_id", no_type)]`; `message_reactions`, `quota_usage` `#[secure(tenant_col="tenant_id", resource_col="id", owner_col="user_id", no_type)]`; `messages`, `chat_turns`, `attachments`, `thread_summaries` `#[secure(tenant_col="tenant_id", resource_col="id", no_owner, no_type)]`; `message_attachments`, `chat_vector_stores` `#[secure(tenant_col="tenant_id", no_resource, no_owner, no_type)]` (`chat_vector_stores` has `id` PK but no resource col needed). Column types: UUID → `Uuid`, TIMESTAMPTZ → `time::OffsetDateTime`, DATE → `time::Date`, JSONB → `serde_json::Value`, BYTEA → `Vec<u8>`, BIGINT → `i64`, INTEGER → `i32`, BOOLEAN → `bool`. String-valued enums stored as text with Rust enums in `infra/db/mod.rs`: `TurnState {Running, Completed, Failed, Cancelled}` (`running|completed|failed|cancelled`), `AttachmentStatus {Pending, Uploaded, Ready, Failed}`, `AttachmentKind {Document, Image}`, `MessageRole {User, Assistant, System}`, `CleanupStatus {Pending, Done, Failed}`, with `as_str()`/`parse()`.
- `test_support::db::test_db() -> toolkit_db::Db` (unique `sqlite:file:mc-{uuid}?mode=memory&cache=shared`, `max_conns 4`, runs `run_migrations_for_testing` with gear + outbox migrations).

Schema (one migration, both dialects; SQLite: UUID → `BLOB`, timestamps/dates → `TEXT`, booleans → `INTEGER CHECK (x IN (0,1))`, JSON → `TEXT`; PG native types): every column, NOT NULL, default, FK (`ON DELETE CASCADE`), CHECK and index of DESIGN §3.7 (2051–2473), including: `uq_messages_chat_request_role` UNIQUE `(chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL`; `messages (chat_id, created_at) WHERE deleted_at IS NULL`; UNIQUE `messages(id, chat_id)`; UNIQUE `chat_turns(chat_id, request_id)`; `uq_chat_turns_running` UNIQUE `chat_turns(chat_id) WHERE state = 'running' AND deleted_at IS NULL`; `chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL`; `chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL`; `attachments` indexes incl. plain `idx_attachments_stale_upload (status, cleanup_status, deleted_at, updated_at)` and UNIQUE `(id, chat_id)`; `message_attachments` composite PK + composite FKs; `thread_summaries` UNIQUE `chat_id`; `chat_vector_stores` UNIQUE `(tenant_id, chat_id)`, CHECK `file_count >= 0`; `quota_usage` UNIQUE `(tenant_id, user_id, period_type, period_start, bucket)`; `message_reactions` UNIQUE `(message_id, user_id)`, CHECK reaction. Simple value CHECKs on `state`, `requester_type`, `attachment_kind`, `status`, `secondary_status`, `secondary_provider_kind`, non-negative counters. `chats` partial index `(tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL`.

- [ ] **Step 1: Read** ref 02 §1–3, §5.1; DESIGN §3.7.
- [ ] **Step 2: Write failing tests** (`infra::db::tests`):
  - `migrations_apply_on_sqlite` — `test_db()` succeeds; inserting one row per table through the entities and reading it back works (UUID/time/date/json/blob round-trip).
  - `running_turn_unique_per_chat` — inserting two `running` non-deleted turns for one chat fails with `is_unique_violation`; a second running turn after the first is set `completed` succeeds; a soft-deleted running row does not block.
  - `turn_request_id_unique_per_chat` — duplicate `(chat_id, request_id)` fails even if the first is deleted.
  - `message_request_role_unique` — duplicate `(chat_id, request_id, 'user')` fails, `'assistant'` with same request_id succeeds.
  - `quota_usage_bucket_unique` — duplicate bucket row fails.
  - `outbox_tables_exist` — `toolkit_db::outbox::Outbox::builder(db).queue("mini-chat.audit", Partitions::of(4))…start()` succeeds against the migrated DB.
- [ ] **Step 3: Run** `cargo test -p cf-gears-mini-chat --lib infra::db` → FAIL.
- [ ] **Step 4: Implement** (raw SQL per dialect as in ref 02 §3.2; MySQL → `DbErr::Custom` error; `down` drops tables).
- [ ] **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): database schema and entities`.

---

### Task 5: Domain errors, Problem mapping, authorization PEP, sanitizer

**Files:**
- Create: `src/domain/{error.rs,authz.rs,sanitize.rs}`, `src/api/error.rs`, `src/test_support/pdp.rs`

**Interfaces:**
- Produces `DomainError` variants and their exact canonical mapping (field / reason / resource type), implemented in `impl From<DomainError> for CanonicalError` (`api/error.rs`), resource markers `ChatResource`, `MessageResource`, `TurnResource`, `AttachmentResource`, `ModelResource` via `#[resource_error(gts_id!("cf.core.mini_chat.<x>.v1~"))]`:

| Variant | Category / HTTP | Machine-readable |
|---|---|---|
| `ChatNotFound`, `MessageNotFound`, `TurnNotFound`, `AttachmentNotFound`, `ModelNotFound` (each `{ id: String }`) | not_found 404 | resource type of the entity, `resource_name = id` |
| `InvalidModel` | invalid_argument 400 | field `model`, reason `INVALID_MODEL` (chat resource) |
| `EmptyContent` | invalid_argument | `content` / `EMPTY_CONTENT` |
| `InvalidTitle` | invalid_argument | `title` / `INVALID_TITLE` |
| `InvalidReaction` | invalid_argument | `reaction` / `INVALID_REACTION` (message resource) |
| `InvalidAttachment(String)` | invalid_argument | `attachment` / `invalid_attachment` |
| `UnsupportedContentType` | invalid_argument | `content_type` / `UNSUPPORTED_CONTENT_TYPE` (attachment) |
| `CodeInterpreterUnavailable` | invalid_argument | `file` / `CODE_INTERPRETER_UNAVAILABLE` (attachment) |
| `MultipartBoundaryRequired` / `MultipartError(String)` / `MissingFile` / `MissingContentType` | invalid_argument | `content_type`/`BOUNDARY_REQUIRED`, `multipart`/`MULTIPART_ERROR`, `file`/`MISSING_FILE`, `content_type`/`MISSING_CONTENT_TYPE` (attachment) |
| `VisionNotSupported` | invalid_argument | `content_type` / `VISION_NOT_SUPPORTED` |
| `FileTooLarge` | out_of_range 400 | `content_length` / `FILE_TOO_LARGE` (attachment) |
| `TooManyImages` | out_of_range | `image_count` / `TOO_MANY_IMAGES` |
| `InputTooLong` | out_of_range | `content` / `INPUT_TOO_LONG` |
| `ContextBudgetExceeded` | out_of_range | `content` / `CONTEXT_BUDGET_EXCEEDED` |
| `FeatureDisabled { subject }` (`web_search` \| `images`) | failed_precondition 400 | violation `{type: FEATURE_DISABLED, subject}` |
| `TurnNotTerminal` | failed_precondition | `{type: STATE, subject: turn_state}` (turn) |
| `ReactionTargetNotAssistant` | failed_precondition | `{type: STATE, subject: reaction_target}` (message) |
| `AccessDenied` | permission_denied 403 | reason `AUTHZ_DENIED` |
| `AuthzUnavailable` | service_unavailable 503 | `retry_after_seconds = 5` |
| `TurnAlreadyRunning` / `RequestIdConflict` / `NotLatestTurn` / `GenerationInProgress` / `Replay` | aborted 409 | reason `turn_already_running` / `request_id_conflict` / `NOT_LATEST_TURN` / `GENERATION_IN_PROGRESS` / `REPLAY` (chat or turn resource) |
| `AttachmentLocked` / `ProviderMismatch` / `Conflict { code }` | already_exists 409 | `resource_name` `attachment_locked` / `provider_mismatch` / `code` |
| `QuotaExceeded { scope }` (`tokens`\|`web_search`\|`code_interpreter`) | resource_exhausted 429 | violation `{subject: scope, description: "quota_exceeded"}` |
| `DocumentLimit` / `StorageLimit` | resource_exhausted 429 | subject `document_limit` / `storage_limit` |
| `StorageUnavailable(String)` | service_unavailable 503 | retry 10 |
| `UploadConcurrencyLimit` | service_unavailable 503 | retry 5 |
| `ChatCleanupPayloadTooLarge(String)` | invalid_argument 400 | `with_format(msg)` |
| `OutboxPayloadTooLarge(String)`, `ProviderUnavailable(String)`, `Internal(String)` | internal 500 | — |

  Also `impl From<toolkit_db::DbError>` and `fn map_scope_err(ScopeError) -> DomainError` (unique violation → `Conflict { code: "unique_violation" }`), `impl From<toolkit_odata::Error>` kept as a separate `DomainError::OData(toolkit_odata::Error)` mapped via toolkit's own `From`.
- `Authz::new(enforcer: PolicyEnforcer)`; resource types from SDK consts with supported props `[owner_tenant_id, owner_id, id]` (chat), `[]` (model), `[owner_tenant_id, owner_id]` (quota). `chat_scope` sends `AccessRequest::new().require_constraints(true)` plus `resource_property(owner_tenant_id, ctx.subject_tenant_id())` for `create`; returns `scope.ensure_owner(ctx.subject_id())`. Error map: `Denied | CompileFailed → AccessDenied`, `EvaluationFailed → AuthzUnavailable` (log cause at error).
- `sanitize::sanitize_provider_message(&str) -> String` per DESIGN line 1070: provider ids `resp_|chatcmpl-|cmpl-|msg_` + `[A-Za-z0-9]+` → `[provider_id]`; `file-|file_|assistant-|vs_` + ≥12 `[A-Za-z0-9]` → `[provider_id]`; URLs (`https?://\S+`) → `[url]`; `sk-` + ≥10 alnum and `Bearer \S+` → `[credential]`.
- `test_support::pdp::FakePdp` implementing `AuthZResolverApi` with `PdpMode::{TenantConstraint, Deny, Fail}` (TenantConstraint returns `In(owner_tenant_id, [subject tenant])` like static-authz).

- [ ] **Step 1: Read** ref 04 §2, ref 03 §5, ADR-0004, DESIGN lines 1196–1257, §3.8.
- [ ] **Step 2: Write failing tests:**
  - `api::error::tests::maps_every_variant` — table-driven: for each variant assert `status`, `problem.type` suffix (category), and the reason location (`context.field_violations[0].{field,reason}`, `context.violations[0].{subject,type|description}`, `context.reason`, `context.resource_name`, `context.resource_type`), e.g. `ChatNotFound` → 404 + `context.resource_type == "gts.cf.core.mini_chat.chat.v1~"`; `AuthzUnavailable` → 503 + `Retry-After: 5` header + `context.retry_after_seconds == 5`; `QuotaExceeded{scope:"web_search"}` → 429 + `violations[0] == {subject:"web_search", description:"quota_exceeded"}`; `Internal` → no top-level `code`, detail generic.
  - `domain::sanitize::tests::scrubs_ids_urls_credentials` — `"file-AbCdEf123456789 failed at https://x.y/z with sk-abcdefghijk1 and vs_abcdefghijkl12"` → `"[provider_id] failed at [url] with [credential] and [provider_id]"`; `"file-based and file_search"` unchanged; `"resp_abc123 done"` → `"[provider_id] done"`.
  - `domain::authz::tests::{owner_constraint_added, deny_is_403, pdp_failure_is_503}` — with `FakePdp`: scope contains `owner_id == subject`; Deny → `AccessDenied`; Fail → `AuthzUnavailable`.
- [ ] **Step 3: Run** `cargo test -p cf-gears-mini-chat --lib -- error authz sanitize` → FAIL.
- [ ] **Step 4: Implement.**
- [ ] **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): domain errors, canonical mapping, authz PEP, sanitizer`.

---

### Task 6: Gateways, ModelService, REST scaffolding, Models API, test harness

**Files:**
- Create: `src/infra/gateways/{mod.rs,policy.rs,audit.rs}`, `src/domain/model_service.rs`, `src/api/{mod.rs,routes.rs,license.rs,state.rs,sse.rs(empty stub)}`, `src/api/dto/models.rs`, `src/api/handlers/models.rs`, `src/test_support/{gateway.rs,catalog.rs,app.rs}`
- Modify: `src/wiring.rs`, `src/gear.rs` (`register_rest` calls `api::routes::register_routes(router, openapi, services, &cfg.url_prefix)`)

**Interfaces:**
- Consumes: Task 1 SDK, Task 3 plugin services, Task 4 DB, Task 5 errors/authz.
- Produces: `PolicyGateway`/`AuditGateway` (Shared Interfaces); `PluginPolicyGateway::new(hub, vendor)` (lazy `GtsPluginSelector`, plugin missing → `DomainError::Internal("model policy plugin unavailable")`); `PluginAuditGateway::new(hub, vendor)` (no plugin → `Dropped`, not cached; found id cached; scoped client missing → reset cache + `Retry`; warning throttled via `toolkit::telemetry::ThrottledLog`); `ModelService` (Shared Interfaces; default-model algorithm DESIGN §3.3 "Create Chat" + PRD model-selection: first enabled `is_default`, else first enabled, else `InvalidModel`); `AppServices { cfg: Arc<MiniChatConfig>, db: Arc<DBProvider<DomainError>>, authz: Arc<Authz>, models: Arc<ModelService>, metrics: Arc<Metrics>, … }`; `ServiceDeps { cfg, db: toolkit_db::Db, hub: Arc<ClientHub>, authz_client: Arc<dyn AuthZResolverApi>, gateway: Arc<dyn ServiceGatewayClientV1>, policy: Arc<dyn PolicyGateway>, audit: Arc<dyn AuditGateway> }`; `register_routes(router, openapi, services, url_prefix) -> Router` with the two model routes; `api::license::License` (base feature, `require_license_features([&License])`); `ModelDto { model_id, display_name, tier ("standard"|"premium"), multiplier_display, description (omitted when empty), multimodal_capabilities, context_window }`, `ModelListDto { items }`.
- Test harness per Shared Interfaces (`TestApp`, `FakeGateway`, `catalog::{premium_model(id), standard_model(id), no_vision_model(id), test_catalog()}`; `test_catalog()` = `[premium "gpt-premium" is_default (VISION, web_search, file_search, code_interpreter), standard "gpt-standard" (VISION, file_search, web_search, code_interpreter), standard "gpt-mini-novision" (no VISION, no tools), disabled premium "gpt-disabled"]`, all on provider `openai` (`kind openai_responses`, host `127.0.0.1`, port 9, `use_http`, storage `openai`), `context_window 128000`, `max_output_tokens 4096`, `max_input_tokens 120000`, multipliers 1_000_000 standard / 3_000_000 premium). `RecordingAudit`/`RecordingPolicy` implement the SDK traits and record calls.

- [ ] **Step 1: Read** ref 01 §2.3, ref 04 §1, §6, §7, ref 05 operations 17–18, DESIGN §3.3 Models API (1278–1346).
- [ ] **Step 2: Write failing tests:**
  - `api::handlers::models::tests::list_shows_only_enabled_without_internal_fields` — `GET /mini-chat/v1/models` → 200, ids `["gpt-premium","gpt-standard","gpt-mini-novision"]` (catalog order), no key among `provider_id, provider_model_id, input_tokens_credit_multiplier_micro, is_default, max_output_tokens`, `tier == "premium"` for gpt-premium.
  - `…::get_disabled_or_unknown_is_404_model` — `GET …/models/gpt-disabled` and `/models/nope` → 404, `context.resource_type == "gts.cf.core.mini_chat.model.v1~"`.
  - `…::pdp_deny_is_403_and_failure_503` — with `PdpMode::Deny` → 403 `context.reason == "AUTHZ_DENIED"`; `PdpMode::Fail` → 503 with `Retry-After: 5`.
  - `domain::model_service::tests::default_model_algorithm` — no `is_default` enabled → first enabled; no enabled → `InvalidModel`; disabled requested → `InvalidModel`; `resolve_chat_model` for disabled id returns the entry, for removed id `InvalidModel`.
  - `infra::gateways::audit::tests::no_plugin_is_dropped_and_not_cached` — empty `MockTypesRegistryClient` → `Dropped` twice (list called twice); after adding an instance + scoped client → `Ok`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS; build server.
- [ ] **Step 6: Commit** `feat(mini-chat): plugin gateways, model service, models API and test harness`.

---

### Task 7: Outbox infrastructure

**Files:**
- Create: `src/infra/outbox/{mod.rs,payloads.rs}`
- Modify: `src/wiring.rs`, `src/gear.rs` (start pipeline in `init` after services are built; keep `parking_lot::Mutex<Option<OutboxHandle>>`; stop it in `serve` after cancel), `src/test_support/app.rs` (TestApp starts the pipeline with `idle_interval(50ms)`)

**Interfaces:**
- Produces: `OutboxEnqueuer` + `OutboxRecord` + `partition_for` (Shared Interfaces); payload structs in `payloads.rs`: `AttachmentCleanupEvent { event_type: String /*attachment_deleted|attachment_upload_abandoned|attachment_indexing_failed*/, tenant_id, chat_id, attachment_id, provider_file_id: Option<String>, vector_store_id: Option<String> /*always None*/, storage_backend: String, attachment_kind: String, deleted_at: OffsetDateTime, secondary_ref: Option<SecondaryRef{file_id, provider_kind, upstream_alias}> }`, `ChatCleanupEvent { tenant_id, chat_id, system_request_id: Uuid, reason: "chat_soft_delete", chat_deleted_at }`, `ThreadSummaryTask { tenant_id, chat_id, system_request_id, base_frontier_created_at: Option<OffsetDateTime>, base_frontier_message_id: Option<Uuid>, frozen_target_created_at, frozen_target_message_id, system_task_type: "thread_summary_update" }`. Payload types: `mini-chat.usage.v1`, `mini-chat.audit.v1`, `mini-chat.attachment_cleanup.v1`, `mini-chat.chat_cleanup.v1`, `mini-chat.thread_summary.v1`. Partition keys: usage/audit/attachment cleanup → `tenant_id`; chat cleanup/thread summary → `chat_id`.
- `start_outbox(db: Db, cfg: &MiniChatConfig, handlers: OutboxHandlers) -> Result<OutboxHandle, OutboxError>`: `outbox_migrations` default prefix, `OutboxProfile::low_latency()`, `processor_tuning(WorkerTuning::processor_low_latency().batch_size(1).retry_max(30s))`, five queues with `Partitions::of(num_partitions)`; thread-summary lease `claim_timeout_secs` (headroom 2 s); audit lease 60 s. `OutboxHandlers { usage, attachment_cleanup, chat_cleanup, thread_summary, audit: Arc<dyn LeasedMessageHandler> }` wrapped by a delegating newtype; until Tasks 19–20 replace them, wiring installs `LoggingAckHandler` (logs payload type, returns `Ok`).
- `fn with_wakes<T>(db, f) -> Result<T, DomainError>` helper: runs `DBProvider::transaction` whose closure returns `(T, Wake)` and fires the wake after commit.

- [ ] **Step 1: Read** ref 02 §4 entirely; DESIGN §5.6 (5430–5534), B.9.3.
- [ ] **Step 2: Write failing tests:** `infra::outbox::tests::partition_is_stable_and_in_range` (`partition_for(u, 4) < 4`, same uuid same result); `…::oversized_payload_rejected` (65 KiB payload → `OutboxPayloadTooLarge`); `…::enqueued_message_is_delivered_after_commit` (TestApp with a recording handler on the audit queue receives the payload; a rolled-back transaction delivers nothing).
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): outbox enqueuer, payloads and pipeline`.

---

### Task 8: Chat CRUD API

**Files:**
- Create: `src/domain/chat_service.rs`, `src/infra/db/repo/{mod.rs,chats.rs}`, `src/api/dto/chats.rs`, `src/api/handlers/chats.rs`
- Modify: `src/api/routes.rs`, `src/api/state.rs`, `src/wiring.rs`

**Interfaces:**
- Produces: `ChatService { create(ctx, CreateChat{title: Option<String>, model: Option<String>}) -> ChatDetail, list(ctx, ODataQuery) -> Page<ChatDetail>, get(ctx, id) -> ChatDetail, rename(ctx, id, title: String) -> ChatDetail, delete(ctx, id) -> () }`; `ChatDetail { id, model, title: Option<String>, is_temporary, message_count: i64, created_at, updated_at }`; `repo::chats::{load_scoped(conn, scope, id) -> Option<chats::Model>, message_count(conn, scope_tenant, chat_id) -> i64, touch_updated_at(tx, chat_id, now)}` (other tasks reuse `load_scoped` + `touch_updated_at`).
- DTOs: `CreateChatReq { title: Option<String>, model: Option<String> }`, `UpdateChatReq { title: String }` (unknown fields ignored), `ChatDetailDto` (title omitted when None, RFC 3339 timestamps). OData field enum `ChatField { UpdatedAt, Id, Title }` (FIELDS order `updated_at, id, title`; kinds DateTimeUtc, Uuid, String) with a mapper that normalises `ChronoDateTimeUtc` filter values into `TimeDateTimeWithTimeZone` so SQLite text comparison matches stored `OffsetDateTime` values; default order `updated_at desc` (+ tiebreaker `id desc`) when no `$orderby` and no cursor; `LimitCfg { default: 20, max: 100 }`.
- Rules: title trimmed, 1–255 chars (`chars().count()`), validated before authz and model lookup; create: authz `create`, model via `ModelService::resolve_for_create`, `updated_at = created_at = now`, `Location: {request path}/{id}` from `OriginalUri`, 201; get/patch/delete 404 for missing, soft-deleted, foreign; patch sets `updated_at = now`; delete in one transaction: set `chats.deleted_at = now, updated_at = now`, set `cleanup_status = 'pending'` (and `cleanup_updated_at`) on the chat's non-deleted attachments whose `cleanup_status IS NULL`, enqueue `ChatCleanupEvent` (payload too large → `ChatCleanupPayloadTooLarge`); 204. `message_count` counts non-deleted messages. Create route carries the dry-run `ThrottlingSpec` (spec §4a.7).

- [ ] **Step 1: Read** DESIGN §3.3 (641–748), PRD chat CRUD (194–199), ref 04 §1.2, §3, §5, ref 05 operations 1–5.
- [ ] **Step 2: Write failing tests** (`api::handlers::chats::tests`):
  - `create_defaults_model_and_returns_location` — POST `{}` → 201, `model == "gpt-premium"`, `message_count == 0`, `is_temporary == false`, no `title` key, `Location == "/mini-chat/v1/chats/{id}"`.
  - `create_validates_title_and_model` — `"   "` → 400 `title/INVALID_TITLE`; 256×`"é"` → 400; 255×`"é"` → 201 (title stored trimmed); `model: "gpt-disabled"` → 400 `model/INVALID_MODEL`; `model: "nope"` → 400.
  - `create_json_extractor_errors` — malformed JSON → 400 `body/json_syntax_error`; `{"title": 5}` → 422 `invalid_json_body`; missing content type → 415 `missing_json_content_type`.
  - `get_patch_delete_lifecycle` — PATCH `{"title":" New ","model":"gpt-standard"}` → 200 title `"New"`, model unchanged, `updated_at` greater; DELETE → 204; GET → 404 chat; second DELETE → 404.
  - `foreign_and_cross_tenant_chats_are_404` — chat of user A: GET/PATCH/DELETE by user B (same tenant) and by tenant B → 404; non-UUID id → 400 `invalid_path_params`.
  - `delete_marks_attachments_and_enqueues_chat_cleanup` — seed an attachment row; after DELETE `cleanup_status == "pending"` and the chat-cleanup handler (recording stub) receives `reason == "chat_soft_delete"`.
  - `list_orders_by_activity_and_paginates` — create 3 chats, rename the first → list order `[first, third, second]`; `limit=2` → 2 items + `next_cursor`; following the cursor → remaining 1 item and `prev_cursor` set; `limit=500` → `page_info.limit == 100`; `limit=0` → 400 `INVALID_LIMIT`; `$filter=contains(title,'x')` filters; `$filter=updated_at gt <rfc3339 of second chat>` returns only newer ones; `$orderby=title asc` sorts; `$filter=foo eq 1` → 400 `context.resource_type == "gts.cf.core.odata.query.v1~"`; `cursor=garbage` → 400 `INVALID_CURSOR`; `$skip=1` → 400 `UNSUPPORTED_QUERY_PARAM`; other users' chats never listed.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): chat CRUD and list API`.

---

### Task 9: Provider resolver, S2S context, OAGW provisioning

**Files:**
- Create: `src/infra/llm/{mod.rs,resolver.rs,s2s.rs,provisioning.rs}`
- Modify: `src/config/providers.rs` (alias normalisation), `src/gear.rs` (`serve`: S2S exchange → provisioning → reconcile task; all with the cancel token), `src/wiring.rs`, `src/test_support/gateway.rs` (record created upstreams/routes; scripted create errors)

**Interfaces:**
- Produces: `S2sContext`, `ProviderResolver`, `ChatTarget`, `StorageTarget`, `ProviderKind`, `StorageKind` (Shared Interfaces). `obtain_s2s_context(authn: &dyn AuthNResolverClient, creds: &ClientCredentialsConfig, cancel) -> anyhow::Result<SecurityContext>` (retries `NoPluginAvailable`/`ServiceUnavailable` every 1 s until cancel; `TokenAcquisitionFailed` → error, gear startup fails). `provision_all(gw: &dyn ServiceGatewayClientV1, s2s: &SecurityContext, providers: &BTreeMap<String, ProviderEntry>) -> ProvisionReport { deferred: Vec<String> }` (Err on fatal); `spawn_reconcile(…, deferred, cancel)` — first retry 2 s, doubling to 60 s cap, one `warn!` naming still-pending providers after 2 min.
- Alias rule (spec §4a.4): `upstream_alias` if set; else host when host is an IP literal; else host when port is standard for the scheme (443 https / 80 http) or unset; else `host:port`. Same rule per tenant override (override host). Resolver and provisioning use the same computed alias.
- Upstream: `Endpoint { scheme: Http if use_http else Https, host, port: port.unwrap_or(80|443) }`, protocol `HTTP_PROTOCOL_ID`, `.alias(alias)`, auth `AuthConfig { plugin_type: auth_plugin_type, sharing: SharingMode::Inherit, config: auth_config }` when `auth_plugin_type` is set, headers `RequestHeaderRules { passthrough: Allowlist, passthrough_allowlist: ["accept", "anthropic-version", "anthropic-beta"] }`, tags `["mini-chat", "mini-chat-provider:{id}"]`. `AlreadyExists` → reuse by paging `list_upstreams` for the alias.
- Routes: chat — `POST`, path = `api_path` without query, cut before `/{model}` when present, `PathSuffixMode::Append`, `query_allowlist` = query keys of `api_path`. Storage entries (`storage_kind` set): `{prefix}/files` methods `[Post, Delete]`, `{prefix}/vector_stores` methods `[Get, Post, Delete]`, prefix `/v1` (openai) or `/openai` (azure, allowlist `["api-version"]`). `AlreadyExists` → reuse. Classification: `FailedPrecondition` on `auth.config.secret_ref`, `Unavailable`, `Timeout`, `Internal` → deferred; everything else fatal (route creation errors for storage routes are logged and skipped — "a missing RAG route only degrades RAG").

- [ ] **Step 1: Read** ref 03 §0–4; DESIGN component `llm_provider` + "OAGW provisioning" (399–412), §3.5 (1417–1448), ADR-0005.
- [ ] **Step 2: Write failing tests** (`infra::llm::provisioning::tests`, `…::resolver::tests`):
  - `alias_rules` — `("127.0.0.1", Some(8080), http)` → `"127.0.0.1"`; `("api.openai.com", None, https)` → `"api.openai.com"`; `("localhost", Some(8080), http)` → `"localhost:8080"`; explicit `upstream_alias: "x"` wins.
  - `provisions_upstream_and_routes_openai` — FakeGateway records one upstream (alias, endpoint, auth sharing Inherit) and routes `POST /v1/responses`, `/v1/files [POST,DELETE]`, `/v1/vector_stores [GET,POST,DELETE]`.
  - `azure_routes_allowlist_api_version` — `api_path "/openai/v1/responses?api-version=2025-03-01-preview"` → chat route path `/openai/v1/responses` allowlist `["api-version"]`; storage routes `/openai/files`, `/openai/vector_stores` with `["api-version"]`.
  - `model_placeholder_route_prefix` — `api_path "/openai/deployments/{model}/chat/completions"` → route path `/openai/deployments`.
  - `already_exists_reuses` — create_upstream returns `AlreadyExists` → reuse listed upstream; no error.
  - `secret_not_readable_is_deferred_then_reconciled` — first create returns `FailedPrecondition(auth.config.secret_ref)` → report `deferred == ["openai"]`; reconcile loop (paused tokio time) provisions on a later attempt.
  - `validation_error_is_fatal` — `Validation` error → `Err`.
  - `resolver_uris` — chat `uri_for("gpt-4.1")` → `/127.0.0.1/v1/responses`; azure storage `uri("/files")` → `/{alias}/openai/files?api-version=2025-03-01-preview`; tenant override selects override alias for that tenant only; `rag_provider` followed by `storage_target`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): provider resolver, S2S context and OAGW provisioning`.

---

### Task 10: LLM adapter types and the `openai_responses` adapter

**Files:**
- Create: `src/infra/llm/{types.rs,errors.rs,openai_responses.rs}`
- Modify: `src/infra/llm/mod.rs` (`LlmClient`), `src/test_support/gateway.rs` (SSE responders), `src/wiring.rs`

**Interfaces:**
- Produces (in `types.rs`):
  - `ProviderRequest { model: String /*provider_model_id*/, instructions: String, input: Vec<InputItem>, tools: Vec<ToolSpec>, max_output_tokens: u32, max_tool_calls: u32, api_params: ModelApiParams, user: String, metadata: RequestMetadata { tenant_id: Uuid, user_id: Uuid, chat_id: Option<Uuid>, request_type: &'static str /*chat|summary*/, feature: String }, stream: bool }`
  - `InputItem::{Message { role: Role(User|Assistant), parts: Vec<ContentPart> }, FunctionCall { call_id, name, arguments: String }, FunctionCallOutput { call_id, output: String }}`, `ContentPart::{Text(String), Image { file_id: String, secondary_file_id: Option<String> }}`
  - `ToolSpec::{FileSearch { vector_store_ids: Vec<String>, max_num_results: u32 }, WebSearch { search_context_size: WebSearchContextSize }, CodeInterpreter { file_ids: Vec<String> }, Function { name: String, description: String, parameters: serde_json::Value }}`
  - `ProviderUsage { input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_tokens: i64 }`
  - `RawCitation::{Web { url: String, title: String, snippet: String, span: Option<(usize, usize)> }, File { provider_file_id: String, filename: String }}`
  - `ProviderEvent::{TextDelta(String), ReasoningDelta(String), ToolStart { name: String, details: Value }, ToolDone { name: String, details: Value }, FunctionCall { call_id, name, arguments }, Completed { response_id: Option<String>, usage: Option<ProviderUsage>, citations: Vec<RawCitation> }, Incomplete { response_id: Option<String>, usage: Option<ProviderUsage>, reason: String }, Failed(ProviderError)}`
  - `ProviderErrorKind::{Provider, Timeout, RateLimited { retry_after_secs: Option<u64> }, ContextLengthExceeded}`; `ProviderError { kind, message: String /*sanitized*/, usage: Option<ProviderUsage> }` with `fn sse_code(&self) -> &'static str` (`provider_error` | `provider_timeout` | `rate_limited`; context-length → `provider_error`) and `fn client_message(&self) -> String` (rate limited: `"Provider rate limit exceeded, retry in {N}s"` when known).
  - `type ProviderEventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>`; `#[async_trait] trait ChatAdapter { async fn stream(&self, target: &ChatTarget, req: ProviderRequest) -> Result<ProviderEventStream, ProviderError>; async fn complete(&self, target: &ChatTarget, req: ProviderRequest) -> Result<CompletionResult { text: String, usage: Option<ProviderUsage> }, ProviderError>; }` — dropping the returned stream drops the OAGW body (hard cancel).
  - `LlmClient { fn adapter(&self, kind: ProviderKind) -> Arc<dyn ChatAdapter> }` holding `Arc<dyn ServiceGatewayClientV1>` + `S2sContext`; every proxy call uses `s2s.get()?`.
- Request JSON (spec §4.2): `model, instructions, input, stream, store:false, max_output_tokens, max_tool_calls, user, metadata{tenant_id,user_id,chat_id,request_type,feature}`, `tools` (only when non-empty), `include:["code_interpreter_call.outputs"]` only with code interpreter; api_params when set (`stop` only non-empty; `reasoning:{effort}`), `extra_body` merged except controlled keys (`model, input, messages, instructions, system, stream, stream_options, max_output_tokens, max_completion_tokens, max_tokens, max_tool_calls, tools, tool_choice, include, store, previous_response_id, user, metadata` → `warn!`). Input items: user `{"role":"user","content":[{"type":"input_text","text":…},{"type":"input_image","file_id":…}]}`; assistant `{"role":"assistant","content":[{"type":"output_text","text":…}]}`; function call items `{"type":"function_call","call_id","name","arguments"}` / `{"type":"function_call_output","call_id","output"}`. Tools: `{"type":"file_search","vector_store_ids":[…],"max_num_results":N}`, `{"type":"web_search","search_context_size":"low"}`, `{"type":"code_interpreter","container":{"type":"auto","file_ids":[…]}}`, `{"type":"function","name","description","parameters"}`.
- SSE translation per DESIGN "Provider Event Translation" (1170–1194): event name from `event:` line, else JSON `type`; `response.output_text.delta`→TextDelta; `…file_search_call.searching`/`.completed` (details `{files_searched: len(results)}`); `…web_search_call.searching`/`.completed`; `…code_interpreter_call.in_progress`→ToolStart; `response.output_item.done` with `code_interpreter_call` → ToolDone `{output: logs joined "\n", capped 8192 chars + "...[truncated]"}`; `response.output_item.done` with `function_call` → FunctionCall; `response.output_text.annotation.added` collected; `response.completed`/`response.incomplete` (usage from `response.usage`, `input_tokens_details.cached_tokens`, `output_tokens_details.reasoning_tokens`; reason from `response.incomplete_details.reason`); `response.failed` (error from `response.error`, fallback top-level `error`; usage kept); `error` event (parsed as failed, then flat `{code,message}`, else raw data as message). Web citations: `url_citation` annotations (`url`, `title`, `start_index`, `end_index`), snippet = annotation `text` if present else the char range of that output_text part (empty when out of range); file citations: `file_citation` (`file_id`, `filename`). Collect from streamed annotation events, falling back to `response.completed.response.output[*].content[*].annotations`. Stream end without terminal event → `Failed(Provider, "provider stream ended without a terminal event")`.
- HTTP/channel errors: `Err(CanonicalError::DeadlineExceeded)`→Timeout; `Err(ResourceExhausted)`→RateLimited; other `Err`→Provider; `Ok` + `ErrorSource::Gateway` status 504 → Timeout, other gateway statuses → Provider; upstream 429 → RateLimited (numeric `Retry-After`); upstream other non-2xx → Provider with sanitized `error.message` (code `context_length_exceeded` → ContextLengthExceeded).
- `complete` sends `stream:false`, parses `output[*].content[*].text` of type `output_text` and `usage`.

- [ ] **Step 1: Read** ref 03 §4, DESIGN 1170–1194, 3668–3725, 3455 (`api_params`, `extra_body`).
- [ ] **Step 2: Write failing tests** (`infra::llm::openai_responses::tests`) using `FakeGateway`:
  - `request_body_shape` — assert the recorded JSON: `stream == true`, `store == false`, `max_tool_calls == 2`, `user == tenant_hex + user_hex` (64 chars), `metadata.request_type == "chat"`, input item roles/content types as above, tools JSON exactly as above, `include` present only with code interpreter, `temperature` absent when unset, `extra_body {"foo":1,"model":"x"}` → `foo` merged, `model` unchanged.
  - `translates_text_tools_and_completion` — scripted SSE (`response.created`, two deltas, file_search searching/completed with 2 results, web_search searching/completed, `response.completed` with usage `{input_tokens:12, output_tokens:5, input_tokens_details:{cached_tokens:3}, output_tokens_details:{reasoning_tokens:1}}` and `id "resp_1"`) → events in order with `files_searched == 2`, `Completed{usage.cache_read_input_tokens == 3, reasoning_tokens == 1, response_id == Some("resp_1")}`.
  - `event_name_from_type_field` — data-only SSE lines (no `event:`) are translated by `type`.
  - `citations_from_annotations` — `url_citation` with start/end → `Web{span: Some((s,e)), snippet == text[s..e]}`; `file_citation` → `File{provider_file_id, filename}`.
  - `code_interpreter_output_truncated` — logs of 9000 chars → details.output ends with `"...[truncated]"` and length 8192 + suffix.
  - `failures_map_to_kinds` — `response.failed` with `{error:{message:"bad file-abcdefghijklmnop"}}` and usage → `Failed{kind Provider, message contains "[provider_id]", usage Some}`; upstream 429 `Retry-After: 7` → `RateLimited{Some(7)}` and `client_message` contains `retry in 7s`; `Err(DeadlineExceeded)` → Timeout; gateway 504 → Timeout; upstream 504 JSON → Provider; truncated stream → Failed Provider.
  - `complete_parses_text_and_usage` — non-streaming JSON → `CompletionResult`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): OpenAI Responses adapter`.

---

### Task 11: File storage and vector stores (OpenAI / Azure)

**Files:**
- Create: `src/infra/storage/{mod.rs,openai.rs}` (one OpenAI-compatible implementation used for both `openai` and `azure` storage kinds through `StorageTarget::uri`), `src/infra/storage/azure.rs` (thin re-export / kind-specific bits)
- Modify: `src/wiring.rs`

**Interfaces:**
- Produces: `#[async_trait] trait FileStorage { async fn upload(&self, t: &StorageTarget, f: FileUpload) -> Result<String, StorageError>; async fn delete(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError>; }` with `FileUpload { filename: String, content_type: String, body: oagw_sdk::body::BodyStream }`; `#[async_trait] trait VectorStores { async fn create(&self, t, name: &str) -> Result<String, StorageError>; async fn add_file(&self, t, vs_id: &str, file_id: &str, attachment_id: Uuid) -> Result<IndexStatus, StorageError>; async fn file_status(&self, t, vs_id: &str, file_id: &str) -> Result<IndexStatus, StorageError>; async fn delete(&self, t, vs_id: &str) -> Result<(), StorageError>; }`; `IndexStatus::{InProgress, Completed, Failed(String)}` (missing `status` → InProgress; `failed`/`cancelled`/unknown → Failed); `StorageError::{Transient(String) /*5xx, gateway errors*/, Permanent { status: u16, message: String } /*other 4xx*/}`; delete treats 2xx and 404 as success.
- Wire calls: `POST {prefix}/files` multipart (`purpose=assistants`, `file` part with filename and content type) → `id`; `DELETE {prefix}/files/{id}`; `POST {prefix}/vector_stores` `{"name":"chat-{chat_id}"}` → `id`; `POST {prefix}/vector_stores/{vs}/files` `{"file_id":…,"attributes":{"attachment_id":"<uuid>"}}` → `status`; `GET {prefix}/vector_stores/{vs}/files/{file_id}` → `status`; `DELETE {prefix}/vector_stores/{vs}`. All via `proxy_request` with the S2S context.

- [ ] **Step 1: Read** ref 03 §4 (multipart), DESIGN §3.6 File Upload (1550–1666), cleanup (1907–2042), B.8.
- [ ] **Step 2: Write failing tests** (`infra::storage::tests`): `upload_sends_multipart_purpose_assistants` (recorded body contains `name="purpose"` / `assistants` and the filename; returns provider id); `azure_uses_openai_prefix_and_api_version` (URIs `/{alias}/openai/files?api-version=…`); `vector_store_add_file_sends_attachment_attribute`; `status_mapping` (`{}`→InProgress, `completed`, `failed`, `cancelled`, `"weird"`→Failed); `delete_404_is_success_500_is_transient_400_is_permanent`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): OpenAI/Azure file storage and vector store clients`.

---

### Task 12: Quota service and quota status API

**Files:**
- Create: `src/domain/quota/{mod.rs,estimate.rs,periods.rs,preflight.rs,ledger.rs,billing.rs,status.rs}`, `src/infra/db/repo/quota_usage.rs`, `src/api/dto/quota.rs`, `src/api/handlers/quota.rs`
- Modify: `src/api/routes.rs`, `src/api/state.rs`, `src/wiring.rs`

**Interfaces:**
- Produces:
  - `estimate::estimate_text_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64` = `ceil((ceil(bytes / max(bpt,1)) + fixed_overhead) * (100 + margin) / 100)`.
  - `periods::PeriodStarts { daily: time::Date, monthly: time::Date }`, `period_starts(now: OffsetDateTime) -> PeriodStarts`, `next_reset(period: Period, now) -> OffsetDateTime`; `Period::{Daily, Monthly}` (`"daily"|"monthly"`), `Bucket::{Total, Premium}` (`"total"|"tier:premium"`).
  - `PreflightInput { tenant_id, user_id, selected_model: String, snapshot: PolicySnapshot, limits: UserLimits, message_bytes: usize, prior_context_tokens: i64, image_count: u32, chat_has_ready_documents: bool, chat_has_ready_ci_files: bool, web_search_requested: bool, streaming_max_output_tokens: u32, minimal_generation_floor: u32, now: OffsetDateTime }`.
  - `PreflightDecision { effective_model: ModelCatalogEntry, effective_is_premium: bool, quota_decision: QuotaDecision(Allow|Downgrade), downgrade_reason: Option<&'static str>, reserve: ReserveAmounts { estimated_input_tokens: i64, max_output_tokens_applied: i32, reserve_tokens: i64, reserved_credits_micro: i64, minimal_generation_floor_applied: i32 }, policy_version: i64, periods: PeriodStarts, limits: UserLimits, tools: ToolGates { file_search: bool, web_search: bool, code_interpreter: bool }, vision_supported: bool }`.
  - `QuotaService::preflight(&self, input) -> Result<PreflightDecision, DomainError>` — order: `web_search_requested && disable_web_search` → `FeatureDisabled{web_search}`; cascade per DESIGN "Downgrade Decision Flow" (3590–3641) with kill switches and candidate selection (selected model if in tier & enabled → `preference.is_default` enabled model of tier → first enabled of tier), per-candidate surcharges by its `tool_support` + kill switches, reserve via `credits_micro_checked` (error → `i64::MAX`, candidate unavailable, `warn!`), availability `spent + reserved + reserve <= limit` for both periods on `total` (+ `tier:premium` for premium; the premium-bucket limit from `limits.premium`, total from `limits.standard`); no candidate → `QuotaExceeded{tokens}`; then daily tool quotas (daily `total` row): `tools.web_search && web_search_calls >= web_search_daily_quota` → `QuotaExceeded{web_search}`; `tools.code_interpreter && code_interpreter_calls >= code_interpreter_daily_quota` → `QuotaExceeded{code_interpreter}`. `ToolGates`: file_search = ready docs ∧ model.file_search ∧ ¬disable_file_search; web_search = requested ∧ model.web_search; code_interpreter = ready CI files ∧ model.code_interpreter ∧ ¬disable_code_interpreter. `downgrade_reason` values `premium_quota_exhausted | force_standard_tier | disable_premium_tier | model_disabled`. Rows read in one transaction (`lock_exclusive()` on PG). Records `mini_chat_quota_preflight{decision,model,tier}` and `quota_estimated_tokens`.
  - `QuotaService::reserve(&self, tx: &DbTx<'_>, tenant_id, user_id, d: &PreflightDecision) -> Result<(), DomainError>` — upsert-increment `reserved_credits_micro` on `total` (+`tier:premium`) daily+monthly rows, then re-read and check `spent + reserved <= limit` for each → else `QuotaExceeded{tokens}` (caller rolls back).
  - `SettleInput { tenant_id, user_id, is_premium: bool, periods: PeriodStarts, turn_reserved_credits_micro: i64, reserve_tokens: i64, max_output_tokens_applied: i64, minimal_generation_floor_applied: i64, in_mult: i64, out_mult: i64, method: SettlementMethod, usage: Option<ProviderUsage>, web_search_calls: i64, code_interpreter_calls: i64 }`; `QuotaService::settle(&self, tx, SettleInput) -> Result<SettleResult { committed_credits_micro: i64, overshoot_capped: bool }, DomainError>` per DESIGN §3.7 "Commit semantics" (2390–2400) + §5.4.5 (overshoot with `quota.overshoot_tolerance_factor`) + estimated formula `credits(est_input, floor)`; released → 0 credits; credit errors → `Internal`.
  - `billing::derive_billing(state: TurnState, error_code: Option<&str>, usage: Option<&ProviderUsage>) -> (BillingOutcome, SettlementMethod)` implementing the DESIGN table (5978–6014): completed→(completed, actual); failed+provider/tool codes→(failed, actual if usage has a non-zero count else estimated); `context_length_exceeded|validation_error|input_too_long|turn_setup_failed`→(failed, released); `orphan_timeout`→(aborted, estimated); cancelled→(aborted, estimated); unknown code→(failed, estimated) with `error!`. `BillingOutcome`/`SettlementMethod` serialize as the lowercase strings.
  - `QuotaService::status(&self, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError>` and `QuotaService::warnings(&self, conn, tenant_id, user_id, limits: &UserLimits, now) -> Result<Vec<QuotaWarning>, DomainError>`; `QuotaWarning { tier: "premium"|"total", period, remaining_percentage: u8, warning, exhausted, next_reset: Option<OffsetDateTime> /*only when warning||exhausted*/ }`; periods with limit ≤ 0 skipped; `remaining_percentage = floor(max(limit - spent - reserved, 0) * 100 / limit)`; `warning = pct <= 100 - warning_threshold_pct`; `exhausted = pct == 0`; tiers ordered `premium`, `total`.
  - Route `GET {prefix}/v1/quota/status` (authz `quota_scope`, action `read` on UserQuota) → `QuotaStatusResponse { tiers: [{tier, periods: [{period, limit_credits_micro, used_credits_micro, remaining_credits_micro, remaining_percentage, next_reset, warning, exhausted}]}], warning_threshold_pct }`.

- [ ] **Step 1: Read** DESIGN §3.2 quota service (419–467), §3.7 quota_usage (2357–2447), "Two-Tier Rate Limiting" + "Downgrade Decision Flow" (3474–3666), §5.3–5.5 (4494–5430), §5.8 billing table (5946–6037), ADR-0008, ref 02 §1.4, §1.8.
- [ ] **Step 2: Write failing tests** (`domain::quota::tests`, seeding `quota_usage` rows through the repo):
  - `estimate_examples` — 0 bytes with defaults (4,100,10) → 110; 10 bytes → ceil((3+100)*1.10)=114 (integer math).
  - `design_example_downgrades_premium` — DESIGN §5.10 numbers (premium P mult 2.5e9, standard S 1e9, limits/spent as in §5.10.1, est input 1000, max_output 500 via streaming cap) → effective S, `Downgrade`, reason `premium_quota_exhausted`, `reserved_credits_micro == 1_500_000`.
  - `cascade_rules` — standard selected never upgrades; disabled selected → `model_disabled` downgrade; missing selected → treated premium + `model_disabled`; `force_standard_tier` → reason `force_standard_tier`; `disable_premium_tier` → `disable_premium_tier`; all exhausted → `QuotaExceeded{tokens}`; monthly-only exhaustion makes tier unavailable.
  - `web_search_kill_switch_before_quota` — `disable_web_search` + requested → `FeatureDisabled{web_search}` even when quota is exhausted.
  - `daily_tool_quotas` — daily `total.web_search_calls = 75` + requested on web-search model → `QuotaExceeded{web_search}`; same request on `gpt-mini-novision` → allowed (tool not sent); CI quota only checked with ready CI files.
  - `reserve_rechecks_limits` — two reserves for the same user each fitting alone but not together: second `reserve` → `QuotaExceeded{tokens}`.
  - `settle_actual_estimated_released_and_overshoot` — actual: reserved decremented, spent += credits, calls += 1, tokens added on `total` only; estimated: spent == `credits(est_input, floor)`, tokens unchanged, web/ci calls added; released: spent unchanged, calls += 1; overshoot 1.15 > 1.10 → spent += reserved credits (`overshoot_capped`); premium turn also updates `tier:premium` reserved/spent/calls.
  - `billing_derivation_table` — every row of the DESIGN mapping table.
  - `status_and_warnings` — limits 100/1000, spent 85 → daily `remaining_percentage == 15`, `warning == true`, `next_reset` tomorrow 00:00Z; spent 99.5% → `exhausted == true`; limit 0 period omitted; `GET /mini-chat/v1/quota/status` returns tiers `["premium","total"]` and `warning_threshold_pct == 80`; another user's usage not visible.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): quota preflight, reserve, settlement, billing derivation and status API`.

---

### Task 13: Context assembly and tool builder

**Files:**
- Create: `src/domain/{context.rs,tools.rs}`

**Interfaces:**
- Produces:
  - `HistoryMessage { id: Uuid, role: Role, content: String, created_at: OffsetDateTime }`.
  - `ContextInput<'a> { model: &'a ModelCatalogEntry, max_output_tokens_applied: u32, guards: Vec<String>, tool_surcharge_tokens: i64 /*sum for tools in the request*/, summary: Option<(String, i64 /*token_estimate*/)>, history: Vec<HistoryMessage> /*chronological, ≤ recent_messages_limit*/, current_text: String, current_images: Vec<ContentPart> }`.
  - `ContextPlan { instructions: String, input: Vec<InputItem>, summary_applied: Option<i64>, assembled_tokens: i64, effective_budget: i64, messages_truncated: bool }`.
  - `assemble(input: ContextInput) -> Result<ContextPlan, DomainError>` per DESIGN "Context Plan Assembly and Truncation" (2986–3088): `instructions = system_prompt + "\n\n" + guards.join("\n\n")` (no separator when no guards); `input_limit = min(max_input_tokens (0 ⇒ ∞), context_window - max_output_tokens_applied)`; `token_budget = input_limit - tool_surcharge_tokens - fixed_overhead_tokens`; `max_output_tokens_applied >= context_window` or `token_budget <= 0` → `ContextBudgetExceeded`; item estimate = `estimate_text_tokens(bytes)` (+ `image_token_budget` per current image); mandatory (instructions + current message + images) over budget → `ContextBudgetExceeded`; summary item (role user, text `"This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.\n\n" + summary`) kept if it fits; history walked newest→oldest while it fits, then leading assistant messages dropped; `messages_truncated` when any history message dropped; `effective_budget = input_limit` (no surcharge/overhead deduction) for the summary trigger; `assembled_tokens` = estimate of everything sent.
  - `tools::build_tools(gates: &ToolGates, model: &ModelCatalogEntry, vector_store_id: Option<&str>, ci_file_ids: &[String], cfg: &ContextConfig) -> ToolSet { tools: Vec<ToolSpec>, guards: Vec<String>, surcharge_tokens: i64, feature: String }` — file_search only with a vector store id; guards: `context.file_search_guard` with file_search, `context.web_search_guard` with web_search; `feature` = `none` or `+`-joined `file_search|web_search|code_interpreter` in that order; surcharge = model budgets for included tools.

- [ ] **Step 1: Read** DESIGN 233–253, 2986–3123, 3124–3150, B.5.5.
- [ ] **Step 2: Write failing tests** (`domain::context::tests`, `domain::tools::tests`):
  - `keeps_everything_when_it_fits` — order: summary item first, then history, then current; `messages_truncated == false`; `summary_applied == Some(token_estimate)`.
  - `drops_oldest_whole_turns_first` — tiny `context_window` so only the last 3 messages fit → kept range starts with a user message (a leading assistant dropped), `messages_truncated == true`, deterministic for repeated calls.
  - `summary_dropped_when_it_alone_does_not_fit`.
  - `mandatory_over_budget_is_context_budget_exceeded` and `max_output_ge_context_window_rejected`.
  - `images_cost_image_token_budget`.
  - `tools_and_guards` — web_search gate → tool `WebSearch{low}` + exact web guard text; file_search without vs id → no tool; feature `"file_search+web_search"`; no tools → `feature == "none"`, no guards.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): context assembly and tool builder`.

---

### Task 14: Stream setup, replay, basic provider task and finalization, `messages:stream`

**Files:**
- Create: `src/domain/stream/{mod.rs,events.rs,setup.rs,replay.rs,provider_task.rs,relay.rs,finalize.rs}`, `src/infra/db/repo/{messages.rs,turns.rs,attachments.rs,thread_summaries.rs,vector_stores.rs}` (functions this task needs; later tasks extend), `src/api/dto/stream.rs`, `src/api/handlers/stream.rs`, `src/api/sse.rs`
- Modify: `src/api/routes.rs`, `src/api/state.rs`, `src/wiring.rs`, `src/test_support/app.rs` (`stream()` helper; `outbox_payloads(queue) -> Vec<Value>` — TestApp wraps every outbox handler with a recorder)

**Interfaces:**
- Produces:
  - `StreamEvent::{StreamStarted { request_id, message_id, is_new_turn: bool, thread_summary_applied: Option<i64> }, Ping, Delta { kind: DeltaKind(Text|Reasoning), content: String }, Tool { phase: ToolPhase(Start|Done), name: String, details: Value }, Citations(Vec<CitationDto>), Done(DonePayload), Error { code: String, message: String }}`; `DonePayload { usage: UsageDto { input_tokens: i64, output_tokens: i64 }, effective_model, selected_model, quota_decision: "allow"|"downgrade", downgrade_from: Option<String>, downgrade_reason: Option<String>, quota_warnings: Option<Vec<QuotaWarning>> }`; `CitationDto { source: "file"|"web", title, url: Option, attachment_id: Option<Uuid>, snippet: String, span: Option<TextSpan{start,end}> }` (optional fields skipped when None). `fn event_name(&self) -> &'static str` = `stream_started|ping|delta|tool|citations|done|error`; ping data `{}`; `thread_summary_applied` serialized as `{"token_estimate": n}`.
  - `pub type EventStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>`.
  - `SendMessage { content: String, request_id: Option<Uuid>, attachment_ids: Vec<Uuid>, web_search_enabled: bool }`; `StreamService::send(&self, ctx: &SecurityContext, chat_id: Uuid, req: SendMessage) -> Result<EventStream, DomainError>` (the handler runs it in `tokio::spawn` and awaits the join handle, DESIGN 1163).
  - `TurnContext` (setup output carried into the provider task and finalization): `{ turn_id, chat_id, tenant_id, user_id, request_id, user_message_id, assistant_message_id, selected_model: String, decision: PreflightDecision, target: ChatTarget, provider_id: String, web_search_enabled: bool, file_citation_map: HashMap<String, (Uuid, String)>, summary: SummaryTriggerInfo { assembled_tokens: i64, effective_budget: i64, messages_truncated: bool, summary_exists: bool }, started_at: std::time::Instant }`.
  - `TerminalOutcome::{Completed { incomplete_reason: Option<String> }, Failed { error_code: String, client_message: String }, Cancelled}`; `finalize::finalize_turn(deps: &StreamService /*db, quota, policy, outbox, metrics*/, turn: &TurnContext, outcome: TerminalOutcome, text: &str, usage: Option<ProviderUsage>, response_id: Option<String>, counts: ToolCounts { web_search: u32, code_interpreter: u32, file_search: u32 }, citations: Vec<CitationDto>) -> FinalizeResult` with `FinalizeResult::{Done(DonePayload, Vec<CitationDto>), Error { code, message }, CasLost, NoEvent /*cancelled*/}`. One transaction: CAS `UPDATE chat_turns SET state, error_code, error_detail, completed_at, updated_at, provider_response_id, assistant_message_id WHERE id = ? AND state = 'running'` (0 rows → `CasLost`); assistant message insert (`role assistant`, `request_id`, `model = effective_model.id`, token columns from usage, `provider_response_id`) for completed (and cancelled with non-empty text, best-effort: on insert failure finalize again without the message); settlement via `QuotaService::settle` with `derive_billing`; enqueue `UsageEvent` (`dedupe_key = "{tenant}/{turn}/{request}"` simple hex, `requester_type "user"`, `terminal_state`, `billing_outcome`, `settlement_method`, `actual_credits_micro = committed`, `policy_version_applied`, counts, `usage` = provider usage or null) and `AuditEvent::Turn` (`turn_completed` for completed else `turn_failed`, quota decision `allow|downgrade`, `trace_id` from the current OTel span when present); fire wakes after commit; compute `quota_warnings` after commit for `Done`. Multipliers from `PolicyGateway::snapshot(user, policy_version)` fetched before the transaction.
- Setup order (each failure is a JSON error, no stream opened): (1) `content.trim()` empty → `EmptyContent`; duplicate `attachment_ids` or more than `max_documents_per_chat + max_images_per_message` → `InvalidAttachment`; (2) `authz.chat_scope(send_message, chat_id)` and scoped chat load → `ChatNotFound`; (3) `ModelService::resolve_chat_model(chat.model)` (removed → `InvalidModel`; plugin failure → `Internal`); (4) `request_id = req.request_id.unwrap_or_else(Uuid::new_v4)`; lookup `(chat_id, request_id)` in `chat_turns` **including deleted**: completed ∧ not deleted → replay; any other existing row → `RequestIdConflict`; (5) any running turn in the chat → `TurnAlreadyRunning`; (6) snapshot boundary (latest message `(created_at,id)`), `prior_context_tokens` (latest non-deleted assistant message with non-zero tokens), chat attachment facts (ready non-deleted documents with `for_file_search`, ready CI files), referenced attachments loaded by ids within the chat (images counted; > `max_images_per_message` → `TooManyImages`); (7) `QuotaService::preflight`; (8) images present ∧ `disable_images` → `FeatureDisabled{images}`; images ∧ ¬`decision.vision_supported` → `VisionNotSupported`; (9) `estimate_text_tokens(content.len())` > `max_input_tokens` (if > 0) → `InputTooLong`; (10) tools via `build_tools` (vector store id from `chat_vector_stores`, CI file ids), history (DESIGN 3071–3088 query, `recent_messages_limit`), thread summary row, `assemble`; (11) `ProviderResolver::chat_target(effective provider, tenant)` (error → `Internal`); (12) reserve transaction: `QuotaService::reserve`, insert user message (`content` as sent, `request_id`, role user), `touch_updated_at(chat)`, validate every `attachment_id` (same tenant, uploader = caller, same chat, `status = ready`, not deleted) → `InvalidAttachment`, insert `message_attachments`, insert `chat_turns` (`running`, `requester_type user`, `requester_user_id`, all preflight columns, `web_search_enabled`, `started_at = last_progress_at = now`); unique violation → re-read: a row with this `request_id` → `RequestIdConflict`, else `TurnAlreadyRunning`. Then build the `ProviderRequest` (`user` = tenant_hex+user_hex, metadata `request_type "chat"`, `feature`) and return the stream.
- Live stream: `relay::spawn_turn(turn, adapter, request) -> EventStream` — emits `StreamStarted{is_new_turn:true, message_id: assistant_message_id, thread_summary_applied}` first; spawns the provider task writing into a bounded `mpsc` of `streaming.sse_channel_capacity`; the returned stream owns a drop guard that cancels a `CancellationToken`. Basic provider task (this task): forward `TextDelta` as `Delta{Text}` immediately (no buffering), accumulate text, on `Completed`/`Incomplete` call `finalize_turn(Completed)` and send `Done` (+ preceding `Citations` when non-empty), on `Failed` finalize `Failed{error_code: err.sse_code()}` and send `Error`. (Task 15 adds the rest.)
- Replay (`replay.rs`): `StreamStarted{is_new_turn:false, message_id: turn.assistant_message_id}`, one `Delta{Text, full content}`, `Done{usage from the assistant message, effective_model = message.model, selected_model = chat.model, quota_decision = downgrade iff they differ, downgrade_from = selected when downgrade, no downgrade_reason, no quota_warnings}`; no writes of any kind.
- SSE writer (`api/sse.rs`): `fn into_sse_response(stream: EventStream) -> Response` using `axum::response::sse::{Sse, Event, KeepAlive}` with `KeepAlive::new().interval(30s)` (comment line), `event: <name>` + JSON `data`, header `X-Accel-Buffering: no`; the stream ends right after the terminal event.
- Route `POST {prefix}/v1/chats/{id}/messages:stream`, body `StreamMessageRequest { content: String, request_id: Option<Uuid>, attachment_ids: Vec<Uuid> (default []), web_search: Option<WebSearchConfig { enabled: bool }> }` via `extract::Json`; responses per ref 05 operation 12.

- [ ] **Step 1: Read** DESIGN 812–926, 990–1168, 1462–1548, 3945–4180, 5535–5843, PRD chat-streaming (212–226), ADR-0010, ref 04 §4.
- [ ] **Step 2: Write failing tests** (`domain::stream::tests` / `api::handlers::stream::tests`, FakeGateway scripted Responses SSE):
  - `send_streams_contract_in_order` — frames `stream_started` (`request_id` echoes the client's, `is_new_turn == true`, no `thread_summary_applied`), `delta`(s) with `type "text"`, `done` with `usage == {input_tokens: 12, output_tokens: 5}`, `effective_model == selected_model == "gpt-premium"`, `quota_decision == "allow"`, no `downgrade_from`; `done` lacks `message_id`/`request_id`; nothing after `done`.
  - `send_persists_turn_messages_and_settlement` — DB: turn `completed`, `assistant_message_id == stream_started.message_id`, `provider_response_id == "resp_1"`; user + assistant messages share `request_id`, assistant `model == "gpt-premium"`, tokens 12/5; `chats.updated_at` bumped; `quota_usage` total daily row `reserved_credits_micro == 0`, `spent_credits_micro == credits(12,5)`, `calls == 1`; exactly one usage payload (`billing_outcome "completed"`, `settlement_method "actual"`, `dedupe_key` format) and one audit payload (`turn_completed`).
  - `server_generates_request_id_when_omitted` — `stream_started.request_id` is a v4 UUID, turn row carries it.
  - `provider_request_contains_history_and_message` — second turn's recorded request `input` contains the first user+assistant messages then the new message; `instructions` starts with the catalog `system_prompt`.
  - `preflight_rejections_open_no_stream` — whitespace content → 400 `content/EMPTY_CONTENT`; duplicate attachment ids → 400 `attachment/invalid_attachment`; foreign chat → 404; removed chat model → 400 `INVALID_MODEL`; `web_search.enabled` with `disable_web_search` → 400 `violations[0] == {type: FEATURE_DISABLED, subject: web_search}`; 5 seeded ready image attachments → 400 `image_count/TOO_MANY_IMAGES`; image on `gpt-mini-novision` chat → 400 `VISION_NOT_SUPPORTED`; image with `disable_images` → 400 `FEATURE_DISABLED images`; not-ready / other-chat / other-uploader attachment → 400 `invalid_attachment` and **no** `quota_usage` reserve left and no turn row; quota exhausted (seeded spent ≥ limits) → 429 `violations[0] == {subject: tokens, description: quota_exceeded}`; in every case `FakeGateway` saw no `/responses` call.
  - `input_too_long_counts_utf8_bytes` — model `max_input_tokens 50` (budgets 4/100/10 → set `fixed_overhead_tokens 0`, `safety_margin_pct 0`): 200 ASCII bytes passes (50 tokens), 52 × "😀" (208 bytes) → 400 `INPUT_TOO_LONG`.
  - `downgrade_reported_in_done_and_message_model` — seed premium bucket exhausted → `done.effective_model == "gpt-standard"`, `selected_model == "gpt-premium"`, `quota_decision "downgrade"`, `downgrade_from "gpt-premium"`, `downgrade_reason "premium_quota_exhausted"`; assistant message `model == "gpt-standard"`; provider request `model` is gpt-standard's `provider_model_id`.
  - `vision_rejected_after_downgrade` — premium exhausted, standard candidate without VISION_INPUT → 400 `VISION_NOT_SUPPORTED`, no provider call.
  - `replay_is_side_effect_free` — resend the completed `request_id` → `stream_started.is_new_turn == false`, same `message_id`, one delta with full text, `done` with stored usage; `quota_usage` and outbox payload counts unchanged; no new provider call.
  - `replay_checked_before_parallel_guard` — while another turn is running (hanging provider), replaying a completed request_id still replays (200), while a new request_id gets 409 `turn_already_running`.
  - `request_id_conflicts` — reuse of a failed / running / soft-deleted turn's request_id → 409 `context.reason == "request_id_conflict"`.
  - `concurrent_sends_one_wins_no_reserve_leak` — two concurrent sends with different request_ids against a hanging provider: exactly one 200 stream, the other 409 `turn_already_running`; after releasing/finishing, `reserved_credits_micro == 0` in all rows.
  - `deltas_are_not_buffered` — provider script `delta "a"`, `Delay(800ms)`, `delta "b"`, `completed`: the first `delta` frame is received by the client < 500 ms after `stream_started`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS; build server.
- [ ] **Step 6: Commit** `feat(mini-chat): send-message streaming pipeline with replay and finalization`.

---

### Task 15: Full provider-task, relay and finalization semantics

**Files:**
- Modify: `src/domain/stream/{provider_task.rs,relay.rs,finalize.rs,events.rs}`, `src/infra/db/repo/turns.rs`

**Interfaces:**
- Consumes/extends Task 14 types. Adds `relay::with_pings(rx: mpsc::Receiver<StreamEvent>, ping_every: Duration) -> impl Stream<Item = StreamEvent>` (emits `Ping` while idle **only** before the first `Delta`/`Tool`; timer reset by every event; after a terminal event the stream ends; if the channel closes with no terminal event it emits `Error{code:"stream_interrupted"}`), and provider-task behaviour:
  - Tool events → `Tool{phase,name,details}` immediately; `ToolStart web_search` count > `quota.web_search_max_calls_per_message` → drop provider stream (cancel), finalize `Failed{web_search_calls_exceeded}`, send `Error`; same for `code_interpreter` with `code_interpreter_max_calls_per_message`; `ToolDone` increments completed counters (`file_search` also bumps `chat_turns.file_search_completed_count`, `web_search`/`code_interpreter` their `*_completed_count`).
  - `last_progress_at` updated on deltas/tool events at most every 30 s (CAS-free update on the running row).
  - `FunctionCall` when knowledge search is not active → finalize `Failed{unexpected_tool_use}`.
  - Client disconnect = cancel token fired (drop guard) or a failed channel send → stop reading (drop provider stream), finalize `Cancelled` (partial text persisted when non-empty, else no assistant message), send nothing. Metrics `cancel_requested`, `cancel_effective`, `time_to_abort_ms`, `streams_aborted{client_disconnect}`, `stream_disconnected{stage}`.
  - Completed: citations mapped — web as is (`source "web"`, `url`, `title`, `snippet`, `span`); file: lookup `turn.file_citation_map[provider_file_id]` (built before the provider call when file_search is in the request: chat's ready non-deleted attachments with `provider_file_id`) → `source "file"`, `attachment_id`, `title = filename`, `snippet ""`, no span; unknown → omitted. One `Citations` event before `Done` only when non-empty and the outcome is not `incomplete`.
  - Incomplete: finalize `Completed{incomplete_reason}` (error_code stays NULL), `Done`, metric `stream_incomplete{reason}` + `stream_completed`.
  - Finalization failure outcomes: completed + assistant insert failure → second finalization as `Failed{message_persistence_failed}` → SSE `error{message_persistence_failed}`; completed/incomplete finalization transaction failure → turn stays running, SSE `error{finalization_failed}`; failed-path finalization failure → original error code still sent; `CasLost` → task ends without terminal → relay emits `stream_interrupted`.
  - Failed provider errors settle actual when usage has a non-zero count, else estimated (`derive_billing`).
- `done.quota_warnings` = `QuotaService::warnings(...)` (every tier × period with limit > 0) on CAS-winning completed/incomplete turns; absent on error, cancelled, replay.

- [ ] **Step 1: Read** DESIGN 1031–1072, 1259–1277, 1667–1700, 3151–3224 (tool limits), 3727–3742, 5535–5843, 6095–6232, ADR-0010.
- [ ] **Step 2: Write failing tests** (`domain::stream::tests`):
  - `tool_events_and_citations_contract` — script: `file_search` searching/completed, `web_search` searching/completed, text, url and file annotations (file id of a seeded ready attachment and an unknown one), completed → frames `stream_started, (tool|delta)*, citations, done`; citations: web item with url/title/snippet/span, file item with `attachment_id` + filename title + `snippet ""`, unknown file omitted, no `file-…` string anywhere in the SSE body; turn `web_search_completed_count == 1`, `file_search_completed_count == 1`; usage payload `web_search_calls == 1`, `file_search_calls == 1`; quota `total` daily `web_search_calls == 1`.
  - `web_search_limit_exceeded_mid_turn` — 3 `web_search_call.searching` events (limit 2) → terminal `error{code:"web_search_calls_exceeded"}`, turn `failed` with that error_code, usage payload `billing_outcome "failed"`, `settlement_method "estimated"`, `spent == credits(est_input, floor)`.
  - `code_interpreter_limit_exceeded_mid_turn` — limit set to 1, two `code_interpreter_call.in_progress` → `code_interpreter_calls_exceeded`.
  - `provider_http_errors_map_to_sse_codes` — chat call answered with 500 JSON containing `file-AbCdEfGhIjKlMn` → frames `stream_started`, `error{code:"provider_error"}` whose message has no `file-`; 429 with `Retry-After: 7` → `rate_limited` and message contains `retry in 7s`; `GatewayStatus(504)` → `provider_timeout`; turn `failed` with matching `error_code`; settlement estimated.
  - `response_failed_with_usage_settles_actual` — `response.failed` carrying usage 10/2 → `settlement_method "actual"`, spent `credits(10,2)`.
  - `incomplete_is_completed_without_error_code` — `response.incomplete` with reason `max_output_tokens` → `done`, turn `completed`, `error_code` NULL, no citations event.
  - `disconnect_mid_stream_cancels_turn_and_settles_estimated` — script delta "partial" then `Hang`; client drops the body after the first delta → eventually turn `cancelled`, assistant message content `"partial"` and `assistant_message_id` set, usage payload `billing_outcome "aborted"`, `settlement_method "estimated"`, audit `turn_failed`; provider stream dropped (FakeGateway observes body drop).
  - `disconnect_before_any_text_persists_no_message` — `Hang` immediately, client drops → turn `cancelled`, `assistant_message_id` NULL, no assistant message row.
  - `message_persistence_failure_reports_error` — while the provider hangs, the test inserts an assistant message with the same `(chat_id, request_id)`; release → SSE `error{message_persistence_failed}`, turn `failed`/`message_persistence_failed`, settlement actual.
  - `finalization_failure_reports_finalization_failed` — completed usage `input_tokens 20_000_000` → SSE `error{finalization_failed}`, turn still `running`.
  - `cas_lost_emits_stream_interrupted` — while hanging, the test sets the turn `failed`/`orphan_timeout` directly; release with completed → SSE `error{stream_interrupted}`, no second usage payload.
  - `unexpected_function_call_fails_turn` — `function_call` output item → `error{unexpected_tool_use}`.
  - `relay_pings_only_before_content` (unit, `tokio::time::pause`) — idle 31 s before first delta yields `Ping`s every 15 s; after a delta no `Ping`; channel closed without terminal → `stream_interrupted`.
  - `null_content_completion_persists_empty_message` — completed with no deltas → `done`, assistant message with empty content exists (replay sends a single empty delta).
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): tool limits, citations, cancellation and finalization edge cases`.

---

### Task 16: Messages list, reactions, turn status APIs

**Files:**
- Create: `src/domain/{message_service.rs,reaction_service.rs}`, `src/infra/db/repo/reactions.rs`, `src/api/dto/{messages,reactions,turns}.rs`, `src/api/handlers/{messages,reactions,turns}.rs` (turn status only), `src/domain/turn_service.rs` (status only)
- Modify: routes/state/wiring

**Interfaces:**
- Produces: `MessageService::list(ctx, chat_id, ODataQuery) -> Page<MessageView>` (authz `list_messages`; messages of the chat with `deleted_at IS NULL`; OData fields `created_at` (DateTimeUtc), `id` (Uuid), `role` (String); default order `created_at asc` + tiebreaker `id asc`; `LimitCfg{20,100}`; a row with NULL `request_id` → `Internal`); batch enrichment: attachments = inner join `message_attachments`→`attachments` with `attachments.deleted_at IS NULL` projecting `AttachmentSummaryDto { attachment_id, kind, filename, status, img_thumbnail: Option<ImgThumbnailDto{content_type:"image/webp", width, height, data_base64}> }` (thumbnail only when image ∧ ready ∧ present); `my_reaction` from `message_reactions` of the caller. `MiniChatMessageDto { id, request_id, role, content, attachments, my_reaction (always present, null allowed), created_at, model?, input_tokens?, output_tokens? }` (`model` omitted when None, token counts omitted when 0).
- `ReactionService::{set(ctx, chat_id, msg_id, reaction: &str) -> ReactionView, remove(ctx, chat_id, msg_id)}`: reaction value validated **before** authz (`like|dislike` else `InvalidReaction`); authz `set_reaction`/`delete_reaction`; chat 404; message in chat (non-deleted) else `MessageNotFound`; non-assistant → `ReactionTargetNotAssistant` (both PUT and DELETE); PUT upserts on `(message_id, user_id)` (tenant/user from ctx/chat) → 200 `{message_id, reaction, created_at}`; DELETE → 204 idempotent. Body `SetReactionReq { reaction: String }` (missing field → 422 via extractor).
- `TurnService::status(ctx, chat_id, request_id) -> TurnStatusView` (authz `read_turn`; non-deleted turn by `(chat_id, request_id)` else `TurnNotFound`): `{ request_id, state: running|done|error|cancelled, error_code? (omitted when null), assistant_message_id? (omitted when null), updated_at }`.
- Routes per ref 05 operations 9, 10, 11, 13.

- [ ] **Step 1: Read** DESIGN 749–778, 928–989, 1347–1395, 2075–2111, 2227–2262, 2449–2473; PRD conversation history (229–236), reactions (452–461), turn status (1078–1103).
- [ ] **Step 2: Write failing tests:**
  - `messages_list_contract_and_counts` — after two completed turns: 4 items in chronological order (user, assistant, user, assistant), each with `request_id`, `attachments: []`, `my_reaction: null`; assistant items have `model`, `input_tokens`, `output_tokens`; user items lack those keys; `GET /chats/{id}` → `message_count == 4`.
  - `messages_list_odata` — `$filter=role eq 'assistant'` → 2; `$filter=id eq '<assistant id>'` → 1; `$orderby=created_at desc` reverses; `limit=1` + cursor walk returns all 4; `$select=id` accepted and ignored (full items); `$orderby=foo` → 400 odata resource type; `cursor=bad` → 400; foreign chat → 404.
  - `message_attachments_listed_without_deleted` — user message referencing a ready image (with thumbnail) and a document: summaries contain `img_thumbnail` only for the image; after soft-deleting the document row the summary list omits it.
  - `reactions_lifecycle` — PUT like → 200; PUT dislike replaces (single row); list shows `my_reaction "dislike"`; DELETE → 204, DELETE again → 204; PUT on user message → 400 `violations[0] == {subject: reaction_target, type: STATE}`; DELETE on user message → same 400; `{"reaction":"love"}` → 400 `reaction/INVALID_REACTION` even for a foreign chat (validated before authz); `{}` → 422; unknown message → 404 message resource type; another user's chat → 404.
  - `turn_status_states` — completed turn → `state "done"`, `assistant_message_id` present, no `error_code`; failed → `state "error"`, `error_code`; running (hanging provider) → `state "running"`, no `assistant_message_id`; unknown request_id → 404 turn resource type; non-UUID → 400 `invalid_path_params`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): messages list, reactions and turn status APIs`.

---

### Task 17: Turn mutations (retry, edit, delete)

**Files:**
- Modify: `src/domain/turn_service.rs`, `src/domain/stream/{mod.rs,setup.rs}` (mutation preflight + post-commit setup reuse the send pipeline), `src/api/handlers/turns.rs`, `src/api/dto/turns.rs`, routes/state/wiring

**Interfaces:**
- Produces: `TurnService::{retry(ctx, chat_id, request_id) -> EventStream, edit(ctx, chat_id, request_id, content: String) -> EventStream, delete(ctx, chat_id, request_id) -> ()}` and `StreamService::run_mutation(ctx, chat, MutationPlan) -> Result<EventStream, DomainError>`.
- Order (DESIGN 1540–1548, §3.9 2802–2909): edit content empty → `EmptyContent`; authz once (`retry_turn`/`edit_turn`/`delete_turn`), chat load (404); **preview** (read-only): target turn by `(chat_id, request_id)` — missing → `TurnNotFound`; target `running` → `TurnNotTerminal` (checked before latest); target deleted or not the latest non-deleted turn by `(started_at, id)` → `NotLatestTurn`; `requester_user_id != subject` → `AccessDenied`. Retry/edit **preflight** (no writes): resolve chat model (removed → `InvalidModel`), original user message + its non-deleted attachments (images re-sent; image count, `disable_images`, vision checks), `web_search_enabled` from the old turn (kill switch check), `QuotaService::preflight`. **Mutation commit** (one tx): re-check latest/terminal on the target, soft-delete target turn (`deleted_at`, `replaced_by_request_id = new` for retry/edit) and its messages (`deleted_at`), insert new user message (retry: original content; edit: new content) + copied `message_attachments` (non-deleted only), insert new `running` turn with NULL preflight columns (unique violation → `GenerationInProgress`), delete `thread_summaries` row and clear `is_compressed` if its frontier ≥ the target turn's user message `(created_at, id)`, `touch_updated_at`, enqueue `TurnMutationAuditEvent` (`turn_retry`/`turn_edit` with original/new request ids; `turn_delete` with `request_id`). **Post-commit setup**: context assembly, provider target, reserve tx that also fills the preflight columns; failure → plain CAS to `failed` with `context_length_exceeded` (budget), `quota_exceeded` (reserve re-check → 429), or `turn_setup_failed` (other), no settlement/usage/audit, return the JSON error. Then stream like send (`stream_started.request_id` = new server UUID v4). Delete: commit only (soft-delete turn + messages, summary rule, audit) → 204.
- Routes: `POST …/turns/{request_id}/retry` (no body), `PATCH …/turns/{request_id}` (`EditTurnRequest { content }`), `DELETE …/turns/{request_id}`; metrics `turn_mutation{op,result}`, `turn_mutation_latency_ms{op}`.

- [ ] **Step 1: Read** DESIGN §3.9 (2802–2909), 1540–1548, 4108–4178; PRD turn mutations (419–451).
- [ ] **Step 2: Write failing tests** (`domain::turn_service::tests`):
  - `retry_replaces_latest_turn` — retry → SSE with new `request_id` ≠ old; old turn `deleted_at` set, `replaced_by_request_id == new`; messages list shows only the new pair; turn status of old → 404; `chats.updated_at` bumped; audit payload `turn_retry` with both ids; the provider request's last user message equals the original content.
  - `edit_uses_new_content_and_keeps_attachments` — old user message referenced an image + document; edit `"new text"` → new user message content `"new text"`, `message_attachments` copied (deleted attachment excluded), provider input contains `input_image` with the image's provider file id.
  - `delete_removes_latest_turn` — 204, messages gone from list, `message_count` decremented by 2, audit `turn_delete`; previous turn becomes latest and can be deleted next.
  - `mutation_guards` — non-latest → 409 `NOT_LATEST_TURN`; already-deleted target → 409 `NOT_LATEST_TURN`; running target → 400 `turn_state/STATE`; foreign chat → 404; empty edit content → 400 `EMPTY_CONTENT`; unknown request_id → 404 turn.
  - `quota_rejection_leaves_previous_turn_intact` — exhaust quota → retry → 429, previous turn not deleted, still latest.
  - `concurrent_mutations_one_wins` — two concurrent retries of the same turn against a hanging provider: one stream; the other 409 (`GENERATION_IN_PROGRESS` or `NOT_LATEST_TURN`); exactly one new non-deleted turn.
  - `post_commit_setup_failure_marks_turn_failed` — catalog with tiny `context_window` where the original message no longer fits → 400 `CONTEXT_BUDGET_EXCEEDED`; new turn `failed`/`context_length_exceeded`; no usage payload; chat not blocked (a new send works).
  - `mutation_of_summarized_turn_drops_summary` — seed a `thread_summaries` row whose frontier covers the target turn's user message and `is_compressed = 1` rows → after delete the summary row is gone and `is_compressed` cleared.
  - `retry_reapplies_image_guards` — `disable_images` turned on after the original image turn → retry 400 `FEATURE_DISABLED images`, previous turn intact.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): retry, edit and delete of the last turn`.

---

### Task 18: Attachments — upload, get, delete, indexing, thumbnails

**Files:**
- Create: `src/domain/attachment/{mod.rs,mime.rs,upload.rs,indexing.rs,thumbnail.rs}`, `src/api/dto/attachments.rs`, `src/api/handlers/attachments.rs`
- Modify: `src/infra/db/repo/{attachments.rs,vector_stores.rs}`, routes/state/wiring

**Interfaces:**
- Produces: `AttachmentService::{upload(ctx, chat_id, headers: &HeaderMap, body: axum::body::Body) -> Result<AttachmentView, DomainError>, get(ctx, chat_id, attachment_id) -> AttachmentView, delete(ctx, chat_id, attachment_id) -> ()}`; `AttachmentDetailDto { id, filename, content_type, size_bytes, status, kind, error_code?, doc_summary? (always omitted), img_thumbnail?, summary_updated_at? (omitted), created_at }` (optional fields omitted when null; `img_thumbnail` only for `image` ∧ `ready`). `IndexingTimings { request_deadline: Duration /*25 s*/, request_poll_initial: 250 ms, request_poll_max: 2 s, background_total: 10 min, background_round: 20 s, background_poll_max: 5 s }` (`Default` = DESIGN values; tests inject small values via `AttachmentService::with_timings`).
- `mime::resolve(part_content_type: Option<&str>, filename: &str, allow_csv: bool) -> Result<ResolvedMime { content_type: String, kind: AttachmentKind, for_file_search: bool, for_code_interpreter: bool }, DomainError>`: allowlist per PRD §13 (pdf `application/pdf`; docx `application/vnd.openxmlformats-officedocument.wordprocessingml.document`; pptx `…presentationml.presentation`; xlsx `…spreadsheetml.sheet` (CI only); `text/plain`; `text/markdown`; `text/html`; `application/json`; code: `text/x-python`, `text/x-java`, `text/x-java-source`, `text/javascript`, `application/javascript`, `application/typescript`, `text/x-typescript`, `text/x-rust`, `text/x-go`, `text/x-csharp`, `text/x-ruby`, `application/sql`, `text/x-sql`; images `image/png|jpeg|webp|gif`; `text/csv` → `text/plain` when `allow_csv`); parameters (`; charset=…`) ignored; `application/octet-stream` inferred from extension (`pdf docx pptx xlsx txt md html htm json py java js ts rs go cs rb sql csv png jpg jpeg webp gif`), unknown → `UnsupportedContentType`.
- `upload::normalize_filename(Option<&str>) -> String` — missing/empty → `"upload"`; > 255 chars → truncated on a char boundary keeping the extension.
- Upload sequence: authz `upload_attachment` + chat load (404 chat) + `resolve_chat_model` (removed → `InvalidModel`) **before reading the body**; `Content-Type` boundary (`multer::parse_boundary`) → `MultipartBoundaryRequired`; parse with `multer`; no `file` field → `MissingFile`; `file` part without content type → `MissingContentType`; `mime::resolve`; image ∧ `disable_images` → `FeatureDisabled{images}`; purposes filtered by `disable_code_interpreter` / model `tool_support.code_interpreter` (no purpose left for a document → `CodeInterpreterUnavailable`); upload semaphore `try_acquire` (`rag.max_concurrent_uploads`) → `UploadConcurrencyLimit`; read the part with a byte counter into memory, limit `min(rag.uploaded_{file|image}_max_size_kb*1024, model.max_file_size_mb*1MiB)` → `FileTooLarge`; per-chat limits on non-deleted non-failed rows: documents count ≥ `max_documents_per_chat` (document uploads) → `DocumentLimit`; total size + this size > `max_total_upload_mb_per_chat` MiB → `StorageLimit`; insert row `pending` (`storage_backend` = storage target backend label, `uploaded_by_user_id`, purposes, `attachment_kind`); `FileStorage::upload` (filename `{chat_id}_{attachment_id}.{ext}`) → failure: row `failed`/`upload_failed`, `StorageUnavailable`; row `uploaded` + `provider_file_id`; then by purpose: document ∧ `for_file_search` → vector-store creation protocol (DESIGN 2339–2355: fast path, placeholder insert, loser polls ≤ 5 times with backoff then `StorageUnavailable`, winner creates + CAS, stale placeholder > 120 s reclaimed; existing row with other `provider` label → `ProviderMismatch`), `add_file` (failure → `failed`/`indexing_failed`), poll status until the request deadline (`completed` → `ready`; failed/cancelled/non-transient error → `failed`/`indexing_failed`, provider file delete best-effort, `StorageUnavailable`; transient error keeps polling; deadline → respond `uploaded` and spawn the background task of `indexing.rs`); CI-only document → `ready`; image → thumbnail (`thumbnail.rs`) then `ready` with `img_thumbnail*` columns. Background task: rounds refreshing `updated_at` (only while `status = uploaded ∧ cleanup_status IS NULL ∧ deleted_at IS NULL`, else stop), `completed` → `ready` (4 attempts 1/2/4 s), failure/timeout → one tx `failed`/`indexing_failed`/`cleanup_status pending` + `AttachmentCleanupEvent{event_type: "attachment_indexing_failed"}`; cancelled on gear stop. Metrics `attachment_upload{kind,result}`, `attachment_upload_bytes{kind}`, `attachments_pending`, `attachment_background_indexing{result}`.
- `thumbnail::make_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail { bytes: Vec<u8>, width: u32, height: u32 }>` — skip when `bytes.len() > max_decode_bytes`, header `w*h > max_pixels` or `w*h*4 > max_decode_bytes`; decode with `image::Limits { max_alloc: Some(max_decode_bytes) }`; fit inside `width×height` preserving aspect (no upscaling); encode WebP lossless; `None` when encoded > `max_bytes` or on any error.
- GET: authz `read_attachment`; row must be in the chat, not deleted, `uploaded_by_user_id == subject` → else `AttachmentNotFound`. DELETE: same lookup but deleted rows of the caller → 204 without changes; uploader mismatch → 404 first; referenced by any `message_attachments` row → `AttachmentLocked`; else one tx: `deleted_at`, `cleanup_status pending`, enqueue `AttachmentCleanupEvent{event_type:"attachment_deleted", …}` → 204.
- Routes per ref 05 operations 6–8; upload declared with `.multipart_file_request("file", Some("File to upload"))`, handler takes `HeaderMap` + raw `Body`.

- [ ] **Step 1: Read** DESIGN 779–811, 1550–1666, 2182–2226, 2301–2356, 3300–3407, B.8, B.9.5; PRD file/image upload (251–286), per-chat limits (360–373), attachment deletion (375–391), ADR-0007.
- [ ] **Step 2: Write failing tests** (FakeGateway scripts files / vector stores):
  - `upload_document_ready` — PDF → 201 `status "ready"`, `kind "document"`, no `doc_summary`/`img_thumbnail` keys; FakeGateway saw `POST /v1/files` (purpose assistants, filename `{chat}_{att}.pdf`), `POST /v1/vector_stores`, `POST /v1/vector_stores/{vs}/files` with `attributes.attachment_id`; DB row `for_file_search = 1`, `provider_file_id` set, `chat_vector_stores` row with `vector_store_id`; a second document reuses the same vector store.
  - `upload_document_indexing_in_progress_then_ready` — status stays `in_progress` past a 200 ms test deadline → 201 `status "uploaded"`; background task later flips to `ready` (GET polling).
  - `upload_indexing_failed_returns_503` — `status: failed` → 503 `Retry-After: 10`; GET shows `status "failed"`, `error_code "indexing_failed"`; provider file DELETE recorded.
  - `background_indexing_timeout_fails_and_enqueues_cleanup` — always `in_progress` with tiny timings → row `failed`/`indexing_failed`/`cleanup_status pending`, attachment-cleanup payload `event_type "attachment_indexing_failed"`.
  - `upload_image_with_thumbnail` — 300×150 PNG → `kind "image"`, `img_thumbnail.content_type "image/webp"`, `width 128`, `height 64`, base64 decodes to WebP; no vector-store call.
  - `upload_xlsx_routes_to_code_interpreter` — `for_code_interpreter = 1`, no vector-store call, `ready`; with `disable_code_interpreter` → 400 `file/CODE_INTERPRETER_UNAVAILABLE`; on `gpt-mini-novision` (no CI) → same 400.
  - `upload_validation_errors` — no boundary → 400 `BOUNDARY_REQUIRED`; no `file` field → `MISSING_FILE`; part without content type → `MISSING_CONTENT_TYPE`; `application/x-msdownload` → `UNSUPPORTED_CONTENT_TYPE`; `application/octet-stream` + `report.pdf` → accepted as pdf; image with `disable_images` → 400 `FEATURE_DISABLED images`; image > `uploaded_image_max_size_kb` → 400 `content_length/FILE_TOO_LARGE`; foreign chat → 404 chat resource type; removed chat model → 400 `INVALID_MODEL` and FakeGateway saw nothing.
  - `per_chat_limits` — `max_documents_per_chat 1`: second document → 429 `violations[0].subject == "document_limit"`; a failed row does not count; `max_total_upload_mb_per_chat 1` with two 600 KiB images → second 429 `storage_limit`.
  - `upload_concurrency_limit` — `max_concurrent_uploads 1`, first upload blocked on a hanging files call, second → 503 `Retry-After: 5`.
  - `provider_mismatch_conflict` — seeded `chat_vector_stores.provider = "other"` → 409 `resource_name "provider_mismatch"`.
  - `long_multibyte_filename_truncated_keeping_extension` — 300 × "й" + ".pdf" → stored filename has 255 chars, ends with `.pdf`, valid UTF-8; missing filename → `"upload"`.
  - `get_and_delete_rules` — another user's attachment in the caller's chat: GET/DELETE → 404 attachment resource type; DELETE own unreferenced → 204, payload `attachment_deleted` with `provider_file_id`, row `cleanup_status pending`; DELETE again → 204 and no second payload; attachment referenced by a sent message → 409 `resource_name "attachment_locked"`; GET of a deleted one → 404.
  - `vector_store_creation_race_creates_one_store` — two concurrent document uploads into a new chat → exactly one `POST /vector_stores`.
  - `ready_attachments_reach_provider_tools` — before any upload the send request has no `tools`; after a ready PDF the next request has `{"type":"file_search","vector_store_ids":[<vs>],"max_num_results":5}` and `instructions` contains `context.file_search_guard`; after a ready XLSX the request has `code_interpreter` with `container.file_ids == [<xlsx file id>]` and `include == ["code_interpreter_call.outputs"]`; a message with an image `attachment_id` has an `input_image` part with the image's provider file id while a later message without it has none; `web_search.enabled = true` adds `{"type":"web_search","search_context_size":"low"}` and the exact web guard text; a deleted (unreferenced) PDF that was the only document removes `file_search` from the next request.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): attachment upload, indexing, thumbnails, get and delete`.

---

### Task 19: Outbox handlers — usage, audit, attachment cleanup, chat cleanup

**Files:**
- Create: `src/infra/outbox/{usage.rs,audit.rs,attachment_cleanup.rs,chat_cleanup.rs}`
- Modify: `src/wiring.rs` (replace `LoggingAckHandler`s)

**Interfaces:**
- Produces `LeasedMessageHandler` impls:
  - `UsageHandler { policy: Arc<dyn PolicyGateway> }` — corrupt JSON → `Reject`; `publish_usage` Ok → `Ok`; `PublishOutcome::Retry` → `Retry`; `Reject(r)` → `Reject(r)`.
  - `AuditHandler { audit: Arc<dyn AuditGateway>, metrics }` — deserialize first (corrupt → `Reject`, metric `reject`); `deliver` with 30 s timeout (timeout = transient); `Dropped` → `Ok` (metric `dropped`); transient/resolution failure → `Retry` unless `msg.attempts >= 119` → `Reject` (metric `reject`); permanent → `Reject`.
  - `AttachmentCleanupHandler { db, storage, resolver, s2s, cfg }` — DESIGN 3357–3371 and 1962–1984: parent chat soft-deleted → `Ok` (no action); `provider_file_id` null → `cleanup_status done`; delete primary via `storage_by_backend(storage_backend)`; success/404 → `done` (+ `cleanup_updated_at`), secondary best-effort; failure → `cleanup_attempts += 1`, `last_cleanup_error`, stays `pending`, `Retry` — when attempts reach `cleanup_worker.max_attempts` → `failed` + `Reject`; DB error → `Retry` without increment; malformed payload → `Reject`. Metrics `cleanup_completed/failed/retry{resource_type=file}`.
  - `ChatCleanupHandler` — DESIGN 1907–2042, 4189–4207: chat not soft-deleted → `Reject("chat is not soft-deleted")`; each `pending` attachment: delete file (+ secondary) → `done` / attempt bookkeeping (`failed` at max, continue); any still `pending` → `Retry`; then `chat_vector_stores` row: any `failed` attachment → metric `cleanup_vector_store_with_failed_attachments`; delete VS (2xx/404) → delete the row → `Ok`; failure → `Retry`, or `Reject("vector store delete: max attempts (N) reached")` when `msg.attempts + 1 >= max_attempts`; DB errors → `Retry`.

- [ ] **Step 1: Read** DESIGN 1907–2042, 3321–3377, 5500–5534, B.9.2–B.9.3, ADR-0009.
- [ ] **Step 2: Write failing tests** (handlers called directly with constructed `OutboxMessage`s, plus one end-to-end through TestApp):
  - `usage_handler_publishes_and_classifies` — RecordingPolicy receives the event; transient → `Retry`; permanent → `Reject`; garbage payload → `Reject`.
  - `audit_handler_outcomes` — delivered → `Ok`; no plugin → `Ok` (dropped); transient at attempts 0 → `Retry`, at attempts 119 → `Reject`; garbage payload with no plugin → `Reject`.
  - `attachment_cleanup_deletes_file_and_marks_done` — FakeGateway `DELETE /v1/files/{id}` 200 → row `cleanup_status "done"`; 404 → done; 500 ×5 with `max_attempts 5` → attempts increment, final `failed` + `Reject`; chat already deleted → `Ok` with no call.
  - `chat_cleanup_end_to_end` — chat with a ready document (vector store) and an image: `DELETE /chats/{id}` → eventually FakeGateway saw both file DELETEs and `DELETE /v1/vector_stores/{vs}`, all attachment rows `done`, `chat_vector_stores` row removed.
  - `chat_cleanup_waits_for_attachments_before_vector_store` — file delete fails once → no VS delete in that delivery (`Retry`); next delivery succeeds and deletes the VS.
  - `chat_cleanup_rejects_active_chat` — payload for a non-deleted chat → `Reject`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): usage, audit and cleanup outbox handlers`.

---

### Task 20: Thread summary trigger and handler

**Files:**
- Create: `src/domain/thread_summary.rs`, `src/infra/outbox/thread_summary.rs`
- Modify: `src/domain/stream/finalize.rs` (trigger inside the completed-path finalization transaction), `src/infra/db/repo/{thread_summaries.rs,messages.rs}`, wiring

**Interfaces:**
- Produces: `thread_summary::evaluate_trigger(info: &SummaryTriggerInfo, threshold_pct: u32) -> bool` (`messages_truncated || (!summary_exists && assembled_tokens >= effective_budget * pct / 100)`); `thread_summary::enqueue_if_needed(tx, enq, turn) -> Result<(bool /*scheduled*/, Option<Wake>), DomainError>` — target = latest non-deleted message created before the turn's user message `(created_at, id)`; base = current frontier; none or target ≤ base → not scheduled; else enqueue `ThreadSummaryTask` (new `system_request_id`). Metric `thread_summary_trigger{scheduled|not_needed}` after commit (only when the trigger evaluated true).
- `ThreadSummaryHandler::handle` per DESIGN 1780–1835 + B.5.5: summary model = `summary_model_id` or `"gpt-4.1-mini"`, resolved **with** enabled filter (missing/disabled → `Reject`, metric `model_unavailable`); base frontier must still match (stored summary missing while task has a base → `Ok`, metric `base_missing`; frontier already advanced → `Ok`, metric `cas_conflicts`); load messages in `(base, target]` non-deleted, non-compressed; prompt: system = catalog `thread_summary_prompt` or config `summary_system_prompt`; user prompt = opening (`"Summarize the following conversation:"` or the existing-summary text + `<existing_summary>…</existing_summary>` + `"New messages to incorporate:"`), entries `User: …` / `Assistant: …` separated by blank lines, contents cut to `message_content_limit` chars + `...`, then the analysis instruction (B.5.5, verbatim); fit to the summary model's budget by dropping the oldest `ceil(n/5)` messages (keep ≥ 2); `complete` with `max_output_tokens = model.max_output_tokens`, `user` = `{tenant_hex}{11111111-6a88-4768-9dfc-6bcd5187d9ed hex}`, metadata `request_type "summary"`, `feature "none"`; context-length error → drop ~20 % and retry (≤ 2 times); provider failure → `Retry` (metrics `provider_error`, `summary_fallback`); parse: strip `<analysis>…</analysis>`, take `<summary>…</summary>` (else whole text unless it still contains `<analysis`/`<summary` markup → empty), collapse blank-line runs; empty → `Retry` (`empty_summary`); commit tx: target message not deleted (`lock_exclusive` on PG) else `Ok` (`frontier_deleted`); upsert summary only if the stored frontier equals base (CAS), set frontier = target, `token_estimate` = `output_tokens - reasoning_tokens` or `ceil(bytes/4)`, mark the range `is_compressed = true`, enqueue system `UsageEvent` (`billing_outcome "system_task"`, `settlement_method "none"`, `actual_credits_micro 0`, `requester_type "system"`, `dedupe_key "{tenant_hex}/thread_summary_update/{system_request_id_hex}"`, `system_task_type "thread_summary_update"`, `policy_version_applied 0`); `Retry` on the `max_attempts`-th delivery becomes `Reject`.
- Context assembly (Task 14 setup) already excludes `is_compressed` messages and messages ≤ frontier and sets `thread_summary_applied`.

- [ ] **Step 1: Read** DESIGN 1702–1906, 2263–2299, 2884–2895, B.5.5, B.9.4; PRD thread summary (395–408).
- [ ] **Step 2: Write failing tests:**
  - `trigger_rules` — truncated → true; no summary and 80 % of budget → true; existing summary without truncation → false.
  - `long_chat_gets_summary_and_next_turn_uses_it` — add to the test catalog a standard model `gpt-tiny-ctx` (`context_window 4096`, `max_output_tokens 1024`, `max_input_tokens 3072`) and four turns with long answers → a summary request (`metadata.request_type == "summary"`, system prompt = configured text) is sent; `thread_summaries` row exists with frontier = last message before the triggering turn; summarized messages `is_compressed = 1`; next turn's `stream_started.thread_summary_applied.token_estimate > 0`; provider input starts with the preamble text; system usage payload with `billing_outcome "system_task"` and no `user_id`.
  - `summary_provider_failure_keeps_previous_state` — summary call 500 → no row, no `is_compressed` change, handler `Retry`; at `max_attempts` → `Reject`.
  - `summary_parse_rules` — `"<analysis>x</analysis>\n<summary>A\n\n\nB</summary>"` → `"A\n\nB"`; no tags → whole text; only `<analysis>` → empty.
  - `frontier_deleted_skips_commit` — delete the target message before the handler runs → `Ok`, no row.
  - `disabled_summary_model_rejected` — `summary_model_id` disabled → `Reject`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): thread summary trigger and outbox handler`.

---

### Task 21: Orphan watchdog, upload reaper, leader election

**Files:**
- Create: `src/infra/workers/{mod.rs,leader.rs,orphan_watchdog.rs,upload_reaper.rs}`
- Modify: `src/gear.rs` (`serve` spawns enabled workers; joins them on cancel with a bounded timeout), wiring

**Interfaces:**
- Produces: `#[async_trait] trait LeaderElector: Send + Sync { async fn is_leader(&self, role: &str) -> bool; }`, `NoopElector` (always true), `#[cfg(feature = "k8s")] LeaseElector` (kube `Api<Lease>` in `POD_NAMESPACE`, holder `POD_NAME`, lease name `mini-chat-{role}`, 15 s duration, 2 s renew, creates a missing Lease). `OrphanWatchdog::scan_once(&self, now: OffsetDateTime) -> Result<u32 /*finalized*/, DomainError>` and `UploadReaper::scan_once(&self, now) -> Result<u32, DomainError>`; `run(self, elector, interval, cancel)` loops.
- Watchdog (DESIGN 4022–4107, B.9.1, 5567–5586): candidates `state running ∧ deleted_at IS NULL ∧ COALESCE(last_progress_at, started_at) <= now - timeout`, ≤ 100, `AccessScope::allow_all()`; per candidate one tx: CAS (same predicate) to `failed`/`orphan_timeout`/`completed_at`; if won: settle estimated from persisted columns (multipliers from `PolicyGateway::snapshot(requester_user_id, policy_version_applied)`; periods from `started_at`; skipped with `warn!` when reserve fields or `requester_user_id` are NULL), enqueue `UsageEvent` (`terminal_state "failed"`, `billing_outcome "aborted"`, `settlement_method "estimated"`, `usage null`, `selected_model = effective_model`, counts from the `*_completed_count` columns; NULL reserve → `actual_credits_micro 0`, models `""`, `policy_version_applied 0`) and `AuditEvent::Turn` (`turn_failed`, quota decision `"unknown"`). Metrics `orphan_detected/finalized{stale_progress}`, `orphan_scan_duration_seconds`, `streams_aborted{orphan_timeout}`.
- Reaper (B.9.5): rows `status IN (pending, uploaded) ∧ deleted_at IS NULL ∧ cleanup_status IS NULL ∧ updated_at < now - stale_after`, oldest first, ≤ 100; per row CAS to `failed`/`upload_abandoned`/`updated_at = now`; with `provider_file_id`: `cleanup_status pending` + `AttachmentCleanupEvent{event_type: "attachment_upload_abandoned", secondary_ref: None}` in the same tx (warn when `secondary_file_id` set). Metrics `attachment_upload_abandoned{from_status}`, `upload_reaper_scan_duration_seconds`.

- [ ] **Step 1: Read** DESIGN 593–598, 4011–4107, B.9.1, B.9.5, ADR-0010.
- [ ] **Step 2: Write failing tests:**
  - `watchdog_finalizes_seeded_stale_turn` — seed (direct DB writes) a running turn with full reserve columns, `last_progress_at = now - 400 s`, and matching `reserved_credits_micro` on quota rows → `scan_once` → turn `failed`/`orphan_timeout`, `completed_at` set, reserve released and `spent == credits(est_input, floor)`, usage payload `aborted`/`estimated`, audit `turn_failed`; a second `scan_once` finalizes nothing.
  - `watchdog_uses_started_at_when_progress_null` and `watchdog_skips_recent_progress` — NULL `last_progress_at` with old `started_at` → finalized; recent progress → untouched.
  - `watchdog_null_reserve_skips_settlement` — retry/edit-style row with NULL reserve fields → finalized, no `quota_usage` change, usage payload `actual_credits_micro 0`.
  - `watchdog_unblocks_chat` — after finalization a new send to the chat succeeds and turn status reports `error`/`orphan_timeout`.
  - `reaper_fails_seeded_stale_upload` — seeded `pending` row (no file id) and `uploaded` row (with file id), `updated_at` 10 min old → both `failed`/`upload_abandoned`; the `uploaded` one `cleanup_status pending` + payload `attachment_upload_abandoned`; a row with `cleanup_status` set and a fresh row are untouched.
  - `noop_elector_is_leader`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement** (also `cargo check -p cf-gears-mini-chat --features k8s`). **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): orphan watchdog, upload reaper and leader election`.

---

### Task 22: Chat Completions, vLLM and Anthropic adapters; Anthropic files; knowledge search

**Files:**
- Create: `src/infra/llm/{openai_chat.rs,vllm_responses.rs,anthropic.rs}`, `src/infra/storage/{anthropic_files.rs,knowledge.rs}`
- Modify: `src/infra/llm/mod.rs` (adapter by kind), `src/domain/stream/{setup.rs,provider_task.rs}` (knowledge-search loop), `src/domain/attachment/upload.rs` (secondary copy), cleanup handlers (secondary delete), wiring

**Interfaces:**
- `OpenAiChatAdapter` (`/chat/completions` style body: `messages` with system first, `stream: true`, `stream_options: {include_usage: true}`, `max_completion_tokens`, `user`; drops `file_search`/`web_search`/`code_interpreter`, keeps function tools; parses `choices[0].delta.content`, `tool_calls` (tool events `name "function_call"` start/done), `finish_reason` (`length` → incomplete `max_tokens`), usage `prompt_tokens`/`completion_tokens`/`prompt_tokens_details.cached_tokens`/`completion_tokens_details.reasoning_tokens`, `[DONE]`).
- `VllmResponsesAdapter` (Responses body without tools and without `metadata`; `<think>…</think>` text → `ReasoningDelta`).
- `AnthropicAdapter` (Messages API: `system`, `messages` with `image` blocks from `secondary_file_id` (`source: {type: file, file_id}`; images without one dropped), `max_tokens`, `stream: true`, `metadata.user_id`, header `anthropic-version: 2023-06-01`; tools: drop file_search, `web_search` → `{type: "web_search_20250305", name: "web_search"}`, code_interpreter → `{type: "code_execution_20250522", name: "code_execution"}` (tool events renamed `code_interpreter`), functions → `{name, description, input_schema}`; events `message_start` (usage), `content_block_start/delta/stop`, `message_delta` (`stop_reason max_tokens` → incomplete), `message_stop`, `error`).
- `AnthropicFiles { upload(alias, filename, content_type, bytes) -> Result<String, StorageError>, delete(alias, file_id) }` (`POST /v1/files` multipart `file`, header `anthropic-beta: files-api-2025-04-14`); upload sets `secondary_status`, `secondary_file_id`, `secondary_provider_kind "anthropic"` for images ≤ `thumbnail.max_decode_bytes` in chats whose model provider is `anthropic_messages`.
- `KnowledgeRetriever` (Azure): `POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version={ver}` `{query, max_num_results}` → chunks trimmed to `max_chunk_chars`; agentic loop per DESIGN 3409–3420 (`search_knowledge` function tool with `knowledge_search.guard`; ≤ `max_calls_per_message` retrievals then "search limit reached" output; hard cap `max_calls_per_message + 2` iterations → `agentic_iterations_exceeded`; never together with `file_search`).

- [ ] **Step 1: Read** ADR-0005, DESIGN 399–413, 1031–1050, 3409–3420, 3668–3707.
- [ ] **Step 2: Write failing tests:** `openai_chat_translation` (text, usage, `[DONE]`, length → incomplete, built-in tools dropped), `vllm_reasoning_split` (`<think>a</think>b` → Reasoning "a", Text "b"; no tools in body), `anthropic_translation` (system prompt, image block from secondary id, web search tool mapping, `max_tokens` stop → incomplete, usage), `knowledge_search_loop` (enabled config + function call `search_knowledge` → retrieval request sent, function output appended, final answer streamed; exceeding the cap → `agentic_iterations_exceeded`), `anthropic_secondary_copy_on_image_upload`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): chat completions, vLLM and Anthropic adapters, knowledge search`.

---

### Task 23: Final gear wiring, server build, black-box suite

**Files:**
- Modify: `src/gear.rs`, `src/wiring.rs`
- Create: `testing/mini_chat_blackbox/{README.md,conftest.py,mock_provider.py,test_blackbox.py,run.sh}`

**Interfaces:**
- `init`: config (+ normalise + validate + deprecated warnings), DB provider, clients from ClientHub (`AuthZResolverApi`, `ServiceGatewayClientV1`, `AuthNResolverClient`), `PluginPolicyGateway`/`PluginAuditGateway`, `build_services`, outbox start with real handlers. `serve`: S2S exchange (retry until cancel), `provision_all` (fatal → return Err), reconcile task for deferred, summary-model presence check (log error only), spawn watchdog + reaper with `NoopElector` (or `LeaseElector` with `k8s`), `ready.notify()`, wait for cancel, cancel workers (join ≤ 10 s), stop background indexing tasks, `outbox.stop()`.
- Black-box suite: `conftest.py` starts `mock_provider.py` (aiohttp; implements `POST /v1/responses` streaming scripted Responses SSE, `POST /v1/files`, `DELETE /v1/files/{id}`, `POST /v1/vector_stores`, `POST /v1/vector_stores/{id}/files`, `GET …/files/{fid}`, `DELETE /v1/vector_stores/{id}`, and records requests), writes a server config (api-gateway on a free port with `auth_disabled: true`, `defaults.body_limit_bytes: 64000000`, both throttling zones; `oagw` with `allow_http_upstream: true`, `ssrf_policy.enabled: false`; `mini-chat` with `providers.openai {kind: openai_responses, storage_kind: openai, host: 127.0.0.1, port: <mock>, use_http: true}` and no auth plugin; static-authn `accept_all` + `s2s_credentials`; static-authz; single-tenant; credstore + static-credstore; static policy plugin with a premium + standard catalog), launches `./target/debug/cf-gears-example-server --config <file> run` recording its pid (kill by pid only), waits for `GET /mini-chat/v1/models` 200.
- Tests (`test_blackbox.py`): create/list/get/rename/delete chat; stream a message and assert SSE order + DB rows via `sqlite3` on `mini_chat.db` (turn completed, messages, `quota_usage`); replay; turn status; quota status; models; upload a PDF and an image; delete chat → mock eventually receives `DELETE /v1/vector_stores/{id}`; 409 parallel turn; reaction lifecycle.

- [ ] **Step 1: Write** the black-box tests and mock (failing until wiring is complete).
- [ ] **Step 2: Implement** the final wiring.
- [ ] **Step 3: Build** `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore` → OK.
- [ ] **Step 4: Run** `testing/mini_chat_blackbox/run.sh` → all PASS; also start the server with `config/mini-chat.yaml` (env vars set to dummy values) and confirm it starts, `GET /mini-chat/v1/models` → 200, then stop it by pid.
- [ ] **Step 5: Run** `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` → PASS.
- [ ] **Step 6: Commit** `feat(mini-chat): final wiring and black-box suite`.

---

### Task 24: Acceptance-criteria traceability audit

**Files:**
- Create: `gears/mini-chat/docs/acceptance-traceability.md`
- Modify: test modules where gaps are found

- [ ] **Step 1:** For each of the 48 items of `gears/mini-chat/docs/acceptance-criteria.md`, list the implementing code location and the test names (Rust and black-box) that cover it, in a table.
- [ ] **Step 2:** For every item without a test, write the missing test (TDD), implement any missing behaviour, run it.
- [ ] **Step 3: Run** `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` and the black-box suite → PASS.
- [ ] **Step 4: Commit** `test(mini-chat): acceptance-criteria traceability`.
