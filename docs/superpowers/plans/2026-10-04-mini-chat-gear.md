# Mini Chat Gear + SDK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the `mini-chat` gear and the new `cf-gears-mini-chat-sdk` crate so the example server serves the full Mini Chat REST/SSE API defined by `gears/mini-chat/docs/` and every `acceptance-criteria.md` item is implemented and tested.

**Architecture:** DDD-light ToolKit gear (`api/rest` → `domain` services → `infra` adapters) with SecureORM persistence (SQLite/PG), ToolKit transactional outbox (5 leased queues), OAGW in-process proxy for all provider traffic, model-policy and audit plugins resolved through types-registry (bundled static plugin gears live inside the gear crate). Domain services depend on small port traits (policy, provider, storage, outbox, clock) so integration tests inject fakes.

**Tech Stack:** Rust 2024 (toolchain pinned by `rust-toolchain.toml`), axum, SeaORM + `toolkit-db` (SecureConn, outbox, OData), `toolkit` (`#[toolkit::gear]`, OperationBuilder), `toolkit-canonical-errors`, `authz-resolver-sdk` (PolicyEnforcer), `oagw-sdk`, `types-registry-sdk`, `multer`, `image`, tokio; Python 3 + pytest + requests for the black-box suite.

**Spec:** `docs/superpowers/specs/2026-10-04-mini-chat-gear-design.md` (read it first; it maps the contract in `gears/mini-chat/docs/{PRD,DESIGN}.md`, `ADR/*`, `acceptance-criteria.md`, and `docs/api/api.json`). Section refs: "S§n" = spec, "D§n" = DESIGN.md, "P§n" = PRD.md.

## Global Constraints

- Build must pass: `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`.
- No new crates outside `Cargo.lock` (offline registry). Gear `Cargo.toml` deps are already complete; SDK uses only workspace deps.
- SDK: path `gears/mini-chat/mini-chat-sdk`, package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`; add to workspace `members` and as a dependency of `gears/mini-chat/mini-chat/Cargo.toml` (`mini-chat-sdk = { package = "cf-gears-mini-chat-sdk", path = "../mini-chat-sdk" }`).
- Keep `mini_chat::infra::plugins::static_audit` and `mini_chat::infra::plugins::static_model_policy` module paths (imported by `apps/cf-gears-example-server/src/registered_gears.rs`).
- Gear name `mini-chat`; plugin gear names `static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin`; REST prefix default `/mini-chat`, all routes under `/mini-chat/v1`.
- Tests: never under `testing/e2e/suites/mini_chat/`. Own black-box suite goes in `testing/e2e/suites/mini_chat_dev/`.
- Manually started servers: `./target/debug/cf-gears-example-server --config <file> run >/tmp/server.log 2>&1 & echo $! >/tmp/server.pid`; stop with `kill "$(cat /tmp/server.pid)"` only (never `pkill -f`/`pgrep -f`).
- Docs win: on any conflict `gears/mini-chat/docs/` (accepted ADRs override older DESIGN/PRD text) beats this plan and the spec.
- Gear unit-test command: `cargo test -p cf-gears-mini-chat` (SDK: `cargo test -p cf-gears-mini-chat-sdk`).
- Commit after each task with a `feat(mini-chat): …` message.

## Review Focus

1. Non-ASCII titles and titles of exactly 255/256 characters — length is counted in Unicode chars after trim (256 → 400 INVALID_TITLE; 255 multibyte chars → 201). Test in Task 4.
2. Client disconnects mid-stream — the turn must end `cancelled` (not stuck `running`), the next send to the chat must be accepted, partial text persisted. Test in Task 9.
3. Provider answers with a non-SSE HTTP 500 JSON error containing `file-abc123def456ghi` / `resp_…` ids — SSE `error{provider_error}` with the ids replaced by `[provider_id]`, turn `failed`, chat not blocked. Test in Task 9.
4. Two concurrent sends to the same chat — exactly one stream, the other 409 `turn_already_running`, even when the race reaches the insert. Test in Task 9.
5. Upload filenames with path separators, no extension, or > 255 chars — stored filename ≤ 255 chars, extension kept, no directory part, provider filename `{chat_id}_{attachment_id}.{ext}`. Test in Task 10.

---

## File Structure (locked)

```
gears/mini-chat/mini-chat-sdk/{Cargo.toml, src/lib.rs, src/models.rs, src/plugin_api.rs, src/gts.rs, src/errors.rs}
gears/mini-chat/mini-chat/src/
  lib.rs, gear.rs, config.rs
  api/rest/{mod.rs, routes.rs, dto.rs, error.rs, sse.rs, handlers/{mod.rs, chats.rs, messages.rs, stream.rs, attachments.rs, turns.rs, reactions.rs, models.rs, quota.rs}}
  domain/{mod.rs, error.rs, authz.rs, models.rs, ports.rs, clock.rs, billing.rs, estimation.rs, context.rs, tools.rs,
          services/{mod.rs, model_resolver.rs, chat_service.rs, message_service.rs, quota_service.rs, stream_service.rs,
                    finalization.rs, replay.rs, turn_service.rs, attachment_service.rs, reaction_service.rs,
                    thread_summary.rs, cleanup.rs}}
  infra/{mod.rs, metrics.rs, mime.rs, thumbnail.rs, oagw_provisioning.rs,
         db/{mod.rs, entities/*.rs, migrations/{mod.rs, m20261004_000001_initial.rs}, repos/*.rs},
         llm/{mod.rs, types.rs, provider_resolver.rs, sse_parser.rs, sanitize.rs, gateway.rs,
              providers/{mod.rs, openai_responses.rs, openai_chat_completions.rs, vllm_responses.rs, anthropic_messages.rs},
              storage/{mod.rs, files.rs, vector_stores.rs, anthropic_files.rs}},
         outbox/{mod.rs, enqueuer.rs, payloads.rs, handlers/{usage.rs, audit.rs, attachment_cleanup.rs, chat_cleanup.rs, thread_summary.rs}},
         gateways/{policy_gateway.rs, audit_gateway.rs},
         plugins/{mod.rs, static_audit/{mod.rs, config.rs}, static_model_policy/{mod.rs, config.rs}},
         workers/{mod.rs, leader.rs, orphan_watchdog.rs, upload_reaper.rs, background_indexing.rs}}
gears/mini-chat/mini-chat/tests/
  common/mod.rs (test app harness), fake_oagw.rs, *.rs per area, ACCEPTANCE_TRACE.md
testing/e2e/suites/mini_chat_dev/{e2e.yaml?, conftest.py, mock_llm.py, config.yaml.tmpl, test_*.py}
```

Unit tests live next to code in `*_tests.rs` files included with `#[cfg(test)] #[path = "x_tests.rs"] mod tests;` (repo convention, see `docs/adrs/tests/0001-tests-in-separate-files.md`).

---

### Task 1: SDK crate, workspace wiring, gear skeleton, config, bundled plugins

Covers: S§1.1 (init skeleton), S§2, S§3. AC: foundation for all.

**Files:**
- Create: `gears/mini-chat/mini-chat-sdk/**` (all SDK files above)
- Modify: `Cargo.toml` (workspace members + `[workspace.dependencies]` entry `mini-chat-sdk = { package = "cf-gears-mini-chat-sdk", path = "gears/mini-chat/mini-chat-sdk" }` if other SDKs are declared there), `gears/mini-chat/mini-chat/Cargo.toml`, `gears/mini-chat/mini-chat/src/lib.rs`
- Create: `src/gear.rs`, `src/config.rs`, `src/config_tests.rs`, `src/infra/plugins/**`, `src/infra/gateways/{policy_gateway.rs,audit_gateway.rs}`, `src/domain/ports.rs` (policy/audit port traits only)

**Interfaces:**
- Produces (SDK, exact names): `ModelTier`, `ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `ApiParams`, `ModelFeatures`, `ModelToolSupport`, `SupportedEndpoints`, `ModelPreference`, `KillSwitches`, `PolicySnapshot`, `PolicyVersionInfo`, `TierLimits`, `UserLimits`, `UsageTokens`, `UsageEvent`, `TurnAuditEvent`, `TurnMutationAuditEvent`, `MiniChatAuditEvent`, traits `MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`, errors `PolicyPluginError`, `PublishError`, `AuditPluginError`, GTS specs `MiniChatModelPolicyPluginSpecV1`, `MiniChatAuditPluginSpecV1` (fields per S§2).
- Produces (gear): `config::MiniChatConfig` with sub-structs `StreamingConfig, EstimationBudgetsConfig, QuotaConfig, OutboxConfig, ContextConfig, RagConfig, ClientCredentials, MetricsConfig, ProviderEntry, TenantOverride, ProviderKind {OpenaiResponses, OpenaiChatCompletions, VllmResponses, AnthropicMessages}, StorageKind {Openai, Azure}, ThumbnailConfig, OrphanWatchdogConfig, UploadReaperConfig, ThreadSummaryWorkerConfig, CleanupWorkerConfig, KnowledgeSearchConfig`; `MiniChatConfig::validate(&mut self) -> anyhow::Result<()>` (expands `${VAR}`, fills `upstream_alias`, checks ranges/charsets/references; logs deprecated-field warnings).
- Produces (ports, `domain/ports.rs`):
  - `#[async_trait] pub trait PolicyPort: Send + Sync { async fn current_snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError>; async fn snapshot_for_version(&self, user_id: Uuid, version: u64) -> Result<Arc<PolicySnapshot>, DomainError>; async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError>; async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError>; }`
  - `pub enum AuditResolution { Delivered, NoPlugin }`; `#[async_trait] pub trait AuditPort: Send + Sync { async fn emit(&self, ev: MiniChatAuditEvent) -> Result<AuditResolution, AuditPluginError>; }` (resolution failure → `AuditPluginError::Transient`).
  - (DomainError is created in Task 3; in this task declare `domain/error.rs` with a placeholder `DomainError::Internal(String)` + `PluginUnavailable(String)` that Task 3 extends.)
- Produces (infra): `gateways::policy_gateway::PolicyGateway` (impl `PolicyPort`, resolves plugin via types-registry + `choose_plugin_instance` with `vendor`), `gateways::audit_gateway::AuditGateway` (impl `AuditPort`; never caches "no plugin"; caches found instance id; client missing in ClientHub → reset cache, `Transient`).
- Gear struct `MiniChatGear` with `#[toolkit::gear(name = "mini-chat", deps = ["types-registry", "authn-resolver", "authz-resolver", "oagw"], capabilities = [db, rest, stateful], lifecycle(entry = "serve", stop_timeout = "30s", await_ready))]`; `DatabaseCapability::migrations()` returns empty vec for now (Task 2 fills).

- [ ] **Step 1: Write failing config tests** in `src/config_tests.rs`:
  - `defaults_match_design`: `MiniChatConfig` parsed from `{"client_credentials":{"client_id":"a","client_secret":"b"}}` has `url_prefix=="/mini-chat"`, `vendor=="constructorfabric"`, `streaming.sse_ping_interval_seconds==15`, `streaming.sse_channel_capacity==32`, `streaming.max_output_tokens==32768`, `estimation_budgets.minimal_generation_floor==50`, `quota.overshoot_tolerance_factor==1.10`, `quota.warning_threshold_pct==80`, `quota.web_search_max_calls_per_message==2`, `quota.web_search_daily_quota==75`, `quota.code_interpreter_max_calls_per_message==10`, `quota.code_interpreter_daily_quota==50`, `outbox.num_partitions==4`, queue names per S§3, `context.recent_messages_limit==10`, `rag.*` defaults (25600, 5120, 4, 50, 100, true, 10), thumbnail (128,128,131072,100000000,33554432), orphan (true,300,60), reaper (true,60,300), summary (true,300,3,80,"",4000), `cleanup_worker.max_attempts==5`, `knowledge_search.enabled==false`, `providers` contains key `openai` with `kind==OpenaiResponses`, `host=="api.openai.com"`, `storage_kind==Openai`.
  - `parses_repo_dev_config`: deserialize the `gears.mini-chat.config` block of `config/mini-chat.yaml` (load the YAML file, take that subtree) with `AZURE_OPENAI_API_HOST=example.openai.azure.com` set via `temp_env`; `validate()` OK; `providers["azure_openai"].upstream_alias == Some("example.openai.azure.com")`, `api_version == Some("2025-03-01-preview")`.
  - `rejects_out_of_range`: table over `sse_ping_interval_seconds=4`, `sse_channel_capacity=65`, `orphan_watchdog.timeout_secs=89`, `upload_reaper.stale_after_secs=59`, `num_partitions=3`, `overshoot_tolerance_factor=1.6`, `warning_threshold_pct=0`, `minimal_generation_floor=0`, azure provider without `api_version`, `rag_provider: "nope"`, host `"a/b"` → `validate()` is Err.
  - `rejects_unknown_keys`: top-level `mcp: {}`, `streaming.web_search_context_size`, `providers.x.supports_file_search_filters` → deserialization Err; `orphan_watchdog.foo: 1` → Ok.
- [ ] **Step 2: Write failing SDK tests** in `mini-chat-sdk/src/models_tests.rs`: `catalog_entry_from_dev_yaml` (deserialize each `model_catalog` entry of `config/mini-chat.yaml` → `tier == Premium` for `gpt-4.1`, `estimation_budgets.bytes_per_token_conservative == 4`, `general_config.tool_support.code_interpreter == true`), `tier_accepts_both_casings` (`"premium"` and `"Premium"`), `catalog_entry_defaults` (minimal JSON with only required fields → `enabled == false`, `max_tool_calls == 2`, `web_search_context_size == "low"`, defaults of `EstimationBudgets` per S§2), `usage_event_omits_user_and_turn_for_system` (serialize system event → JSON has no `user_id`/`turn_id` keys, `"usage"` present), `kill_switches_require_all_fields`.
- [ ] **Step 3: Run** `cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat` → FAIL (crate/types missing).
- [ ] **Step 4: Implement** the SDK, workspace wiring, `config.rs`, plugin gears (`static_model_policy`: config with `deny_unknown_fields`, `kill_switches` mirror struct defaulting false, multiplier/bpt validation at init, registers `PluginV1<MiniChatModelPolicyPluginSpecV1>` instance + scoped client, fixed `policy_version` 1, limits from `default_standard_limits`/`default_premium_limits`; `static_audit`: logs at info when `enabled`), gateways, gear skeleton whose `init` parses + validates config, registers both plugin schemas in types-registry, builds gateways, stores them; `serve` notifies ready and waits for cancel. Follow `gears/system/tenant-resolver` for plugin registration and `gears/system/types-registry/types-registry/src/gear.rs` for lifecycle.
- [ ] **Step 5: Add plugin tests** `infra/plugins/static_model_policy/mod_tests.rs`: `rejects_zero_multiplier`, `rejects_unknown_kill_switch`, `snapshot_version_is_1`, `default_limits` (100_000_000/1_000_000_000 and 50_000_000/500_000_000).
- [ ] **Step 6: Run tests** → PASS. **Run build** (Global Constraints command) → success.
- [ ] **Step 7: Smoke-start the server** with `config/mini-chat.yaml` (export `AZURE_OPENAI_API_HOST=127.0.0.1`, `AZURE_OPENAI_API_KEY=x`), confirm log line that mini-chat initialized and no panic, then stop by pid.
- [ ] **Step 8: Commit** `feat(mini-chat): sdk crate, config, gear skeleton, bundled plugins`.

---

### Task 2: Persistence — entities, migrations, repositories, outbox enqueuer

Covers: S§4, D§3.7; outbox port used by Tasks 4,9,10,11,13,14,15.

**Files:**
- Create: `src/infra/db/**`, `src/infra/outbox/{mod.rs,enqueuer.rs,payloads.rs}`, `src/domain/clock.rs`, `tests/common/mod.rs` (DB part), `tests/schema.rs`
- Modify: `src/gear.rs` (`migrations()` returns mini-chat migrations + `toolkit_db::outbox::outbox_migrations()`)

**Interfaces:**
- Produces entities (SeaORM, `Scopable`): `chat`, `message`, `chat_turn`, `attachment`, `message_attachment`, `thread_summary`, `chat_vector_store`, `quota_usage`, `message_reaction` with columns exactly per S§4 (Rust field names = column names). Secure attrs: chats/quota_usage/message_reactions `tenant_col="tenant_id", owner_col="user_id"`; others `tenant_col="tenant_id", no_owner` (resource_col `id` where the table has `id`).
- Produces `infra::db::migrations::Migrator` (`MigratorTrait`), single migration `m20261004_000001_initial` creating all tables/indexes/checks for SQLite and Postgres (branch on `manager.get_database_backend()`; partial indexes via raw SQL in the migration).
- Produces repos (`infra/db/repos/*.rs`), all taking `runner: &impl DBRunner` + `&AccessScope` (tenant(+owner) scope). Names other tasks call: `ChatRepo`, `MessageRepo`, `TurnRepo`, `AttachmentRepo`, `MessageAttachmentRepo`, `ThreadSummaryRepo`, `VectorStoreRepo`, `QuotaUsageRepo`, `ReactionRepo` (unit structs; methods added by the tasks that need them; this task adds `insert`/`find_by_id` basics + tests).
- Produces `domain::clock::Clock` trait `fn now(&self) -> OffsetDateTime` (time crate) with `SystemClock`; tests use `FixedClock`.
- Produces outbox port (`domain/ports.rs` additions):
  ```rust
  pub struct PendingWakes(Vec<toolkit_db::outbox::Wake>); // fire_all(self) after commit
  #[async_trait] pub trait OutboxPort: Send + Sync {
      async fn enqueue_usage(&self, tx: &DbTx<'_>, ev: &UsageEvent, wakes: &mut PendingWakes) -> Result<(), DomainError>;
      async fn enqueue_audit(&self, tx: &DbTx<'_>, ev: &MiniChatAuditEvent, wakes: &mut PendingWakes) -> Result<(), DomainError>;
      async fn enqueue_attachment_cleanup(&self, tx: &DbTx<'_>, p: &AttachmentCleanupPayload, wakes: &mut PendingWakes) -> Result<(), DomainError>;
      async fn enqueue_chat_cleanup(&self, tx: &DbTx<'_>, p: &ChatCleanupPayload, wakes: &mut PendingWakes) -> Result<(), DomainError>;
      async fn enqueue_thread_summary(&self, tx: &DbTx<'_>, p: &ThreadSummaryPayload, wakes: &mut PendingWakes) -> Result<(), DomainError>;
  }
  ```
  `infra::outbox::payloads::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload}` with fields per D "Attachment Deletion" table, D "Outbox payload and execution semantics", D "Execution stages" (serde). Partition: usage/audit/attachment cleanup by `tenant_id`, chat cleanup/thread summary by `chat_id` (stable hash mod `num_partitions`). `infra::outbox::enqueuer::OutboxEnqueuer` implements `OutboxPort` over `toolkit_db::outbox::Outbox` (set via `OnceLock` when the pipeline is built; enqueue before that → `DomainError::Internal`). Queue registration helper `infra::outbox::build_pipeline(db, cfg, handlers) -> Outbox builder` lands in Task 13; this task provides `infra::outbox::register_queues(builder, cfg)` with no-op handlers that `Retry` so tests can start it.
- Test harness (`tests/common/mod.rs`): `pub async fn test_db() -> Arc<DBProvider<DomainError>>` (shared-cache in-memory SQLite, runs `Migrator` + outbox migrations), `pub fn tenant_scope(tenant, user) -> AccessScope`.

- [ ] **Step 1: Write failing tests** `tests/schema.rs`:
  - `all_tables_created`: after `test_db()`, query `sqlite_master` (raw SQL allowed in tests) → contains `chats, messages, chat_turns, attachments, message_attachments, thread_summaries, chat_vector_stores, quota_usage, message_reactions` and a table starting with `toolkit_outbox`.
  - `one_running_turn_per_chat`: inserting two `chat_turns` rows `state='running'` for one chat → second insert fails with unique violation; a `completed` second row succeeds.
  - `turn_request_id_unique`: same `(chat_id, request_id)` twice → unique violation.
  - `message_request_role_unique_ignores_deleted`: two `(chat, request, 'user')` rows where the first has `deleted_at` set → OK; without `deleted_at` → violation.
  - `quota_usage_unique_bucket`, `reaction_unique_per_user`, `vector_store_unique_per_chat`.
  - `state_check_constraint`: `state='bogus'` → error.
  - `uuid_stored_as_blob`: `SELECT typeof(id) FROM chats` → `blob`.
- [ ] **Step 2: Run** `cargo test -p cf-gears-mini-chat --test schema` → FAIL.
- [ ] **Step 3: Implement** entities, migration (both backends), repos basics, clock, outbox port + enqueuer + payloads, harness.
- [ ] **Step 4: Run** → PASS. Add `tests/outbox_enqueue.rs::usage_row_written_in_tx_and_rolled_back`: enqueue inside a tx that returns Err → no outbox body rows; Ok → one row whose payload JSON has `dedupe_key`.
- [ ] **Step 5: Build** server; commit `feat(mini-chat): schema, repositories, outbox enqueuer`.

---

### Task 3: Domain errors, Problem mapping, authorization, model resolution, Models API, REST test harness

Covers: S§5.2, S§12, D "Models API". AC: "Error Mapping (REST)", "Models API", "Authorization" (foundation).

**Files:**
- Create/extend: `src/domain/error.rs`, `src/api/rest/error.rs` (+`error_tests.rs`), `src/domain/authz.rs`, `src/domain/services/model_resolver.rs`, `src/api/rest/{mod.rs,routes.rs,dto.rs}`, `src/api/rest/handlers/models.rs`, `tests/common/mod.rs` (app harness), `tests/models_api.rs`, `tests/authz.rs`
- Modify: `src/gear.rs` (RestApiCapability registers routes)

**Interfaces:**
- `DomainError` variants (exact names; each maps to the ADR-0004 row in parentheses): `ChatNotFound`, `MessageNotFound`, `TurnNotFound`, `AttachmentNotFound`, `ModelNotFound` (404 + resource_type `gts.cf.core.mini_chat.{chat|message|turn|attachment|model}.v1~`); `InvalidModel` (400 `field_violations[model]=INVALID_MODEL`); `EmptyContent`; `InvalidTitle`; `InvalidReaction`; `InvalidAttachment` (field `attachment`, reason `invalid_attachment`); `UnsupportedContentType`; `CodeInterpreterUnavailable` (field `file`, resource_type attachment); `Multipart { field: &'static str, reason: &'static str }`; `VisionNotSupported` (field `content_type`); `OutboxPayloadTooLarge(String)` (400, detail + `context.format`); `FileTooLarge` (out_of_range, field `content_length`); `TooManyImages` (field `image_count`); `InputTooLong`; `ContextBudgetExceeded`; `FeatureDisabled(FeatureSubject)` with `FeatureSubject::{WebSearch, Images}` (failed_precondition `{subject: web_search|images, type: FEATURE_DISABLED}`); `TurnNotTerminal` (`turn_state`/`STATE`); `ReactionTargetNotAssistant` (`reaction_target`/`STATE`); `AuthzDenied` (403 reason `AUTHZ_DENIED`); `AuthzUnavailable` (503, Retry-After 5); `TurnAlreadyRunning`, `RequestIdConflict`, `NotLatestTurn`, `GenerationInProgress`, `Replay` (409 aborted, reasons `turn_already_running`, `request_id_conflict`, `NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`, `REPLAY`); `AttachmentLocked`, `ProviderMismatch`, `UniqueViolation(String)` (409 already_exists, resource_name `attachment_locked`/`provider_mismatch`/code); `QuotaExceeded(QuotaScope)` with `QuotaScope::{Tokens, WebSearch, CodeInterpreter}` (429 `violations[{subject: tokens|web_search|code_interpreter, description: "quota_exceeded"}]`); `DocumentLimit`, `StorageLimit` (429 subjects `document_limit`/`storage_limit`); `StorageUnavailable` (503 RA 10); `UploadConcurrencyLimit` (503 RA 5); `PluginUnavailable(String)`, `ProviderResolution(String)`, `Internal(String)`, `Database(String)` (500). `impl From<EnforcerError> for DomainError` (Denied/CompileFailed → AuthzDenied, EvaluationFailed → AuthzUnavailable). `impl From<DomainError> for Problem` in `api/rest/error.rs` using `toolkit_canonical_errors` builders (`resource_error!`/`CanonicalError` helpers); detail texts generic.
- `domain::authz`: consts `CHAT`, `MODEL`, `USER_QUOTA: ResourceType` (names from S§12) and action consts `CREATE="create"`, `LIST="list"`, `READ="read"`, `UPDATE="update"`, `DELETE="delete"`, `LIST_MESSAGES`, `SEND_MESSAGE`, `UPLOAD_ATTACHMENT`, `READ_ATTACHMENT`, `DELETE_ATTACHMENT`, `READ_TURN`, `RETRY_TURN`, `EDIT_TURN`, `DELETE_TURN`, `SET_REACTION`, `DELETE_REACTION` (snake_case strings). `pub struct Pep { enforcer: PolicyEnforcer }` with `async fn chat_scope(&self, ctx: &SecurityContext, action: &str, chat_id: Option<Uuid>) -> Result<AccessScope, DomainError>` (adds tenant + owner predicates for the subject; create passes owner props), `async fn model_access(&self, ctx, action) -> Result<(), DomainError>` (require_constraints=false), `async fn quota_scope(&self, ctx) -> Result<AccessScope, DomainError>`.
- `ModelResolver` (`services/model_resolver.rs`): `async fn snapshot(&self, user_id) -> Result<Arc<PolicySnapshot>, DomainError>`; `fn find<'a>(snap: &'a PolicySnapshot, id: &str) -> Option<&'a ModelCatalogEntry>`; `async fn resolve_for_new_chat(&self, user_id, requested: Option<&str>) -> Result<ModelCatalogEntry, DomainError>` (enabled filter; default algorithm D "Model Catalog Configuration"; none → InvalidModel); `async fn resolve_chat_model(&self, user_id, model_id) -> Result<(Arc<PolicySnapshot>, ModelCatalogEntry), DomainError>` (no enabled filter; missing → InvalidModel); `async fn visible_models(&self, user_id) -> Result<Vec<ModelCatalogEntry>, DomainError>` (enabled only, catalog order).
- License gate: every route uses `.authenticated()` + `.require_license_features(...)` with the platform base feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` (see `gears/system/api-gateway/src/middleware/license_validation.rs` for how the feature id is declared); route paths built from `config.url_prefix` (`{url_prefix}/v1/...`).
- REST: `api::rest::routes::register_routes(router, openapi, state: Arc<AppServices>) -> Router` where `AppServices` (in `domain/services/mod.rs`) holds `Arc`s of every service (fields added by later tasks). Handlers extract `Extension<SecurityContext>`, `Extension<Arc<AppServices>>`.
- Test harness additions (`tests/common/mod.rs`): `pub struct TestApp { pub router: axum::Router, pub db, pub oagw: Arc<FakeOagw>, pub policy: Arc<FakePolicy>, pub pdp: Arc<FakePdp>, pub clock, ... }`; `TestApp::builder().catalog(vec![...]).kill_switches(..).limits(..).config(|c| ..).build().await`; `app.as_user(user_id, tenant_id).get(path)/post_json(path, body)/patch_json/delete/put_json/post_multipart` returning `TestResponse { status, headers, json(), text(), sse_events() -> Vec<(String /*event*/, serde_json::Value)> }`. `FakePdp` modes `Allow` (returns tenant `In` constraint like static-authz), `Deny`, `Fail`. Catalog fixtures `premium_model("p1")`, `standard_model("s1")`, `standard_no_vision("s-novision")`.

- [ ] **Step 1: Write failing unit tests** `api/rest/error_tests.rs`: one test per ADR-0004 row asserting `(status, category in Problem.type, machine field)`, e.g. `quota_tokens_is_429_with_subject_tokens`: `Problem::from(DomainError::QuotaExceeded(QuotaScope::Tokens))` → status 429, `context.violations[0].subject=="tokens"`, `description=="quota_exceeded"`; `authz_unavailable_has_retry_after_5`; `chat_not_found_resource_type`; `not_latest_turn_reason`; `attachment_locked_resource_name`; `feature_disabled_web_search`; `file_too_large_field_content_length`.
- [ ] **Step 2: Write failing integration tests** `tests/models_api.rs`: `list_returns_only_enabled_in_catalog_order` (body `items[*].model_id`), `model_dto_has_no_internal_fields` (no keys `provider_id`, `provider_model_id`, `input_tokens_credit_multiplier_micro`, `is_default`; `tier=="premium"`; `description` absent when empty), `get_disabled_or_unknown_is_404_model`, `pdp_deny_is_403`, `pdp_failure_is_503_retry_after` (header `Retry-After: 5`).
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS; build; commit `feat(mini-chat): errors, authz PEP, model resolution, models API`.

---

### Task 4: Chat CRUD + list with OData + chat delete enqueue

Covers: S§5, S§11 (chat delete). AC: "Chat CRUD" (all 3), "Tenant and owner isolation", "A chat's model is immutable", "Authorization", part of "Cleanup & Recovery" (enqueue).

**Files:** `src/domain/services/chat_service.rs`, `src/api/rest/handlers/chats.rs`, `src/api/rest/dto.rs` (ChatDetailDto with `ODataFilterable` fields `updated_at`, `id`, `title`; Create/UpdateChatReq), repos `ChatRepo` methods, `tests/chats.rs`.

**Interfaces:**
- `ChatService`: `create(ctx, title: Option<String>, model: Option<String>) -> Result<ChatDetail, DomainError>`; `get(ctx, id)`; `list(ctx, &ODataQuery) -> Result<Page<ChatDetail>, DomainError>`; `rename(ctx, id, title)`; `delete(ctx, id) -> Result<(), DomainError>`; `pub(crate) async fn load_scoped(&self, ctx, action, id) -> Result<(AccessScope, chat::Model), DomainError>` (used by every chat-scoped service). `domain::models::ChatDetail { id, model, title: Option<String>, is_temporary, message_count: i64, created_at, updated_at }`. `fn validate_title(raw: &str) -> Result<String, DomainError>` (trim, char count 1..=255).
- Delete txn: soft-delete chat (`deleted_at = updated_at = now`), mark attachments `cleanup_status='pending'`, enqueue `ChatCleanupPayload { tenant_id, chat_id, system_request_id: Uuid::new_v4(), reason: "chat_soft_delete", chat_deleted_at }`; payload-too-large → `OutboxPayloadTooLarge`.
- OData list: `paginate_odata` with default order `("updated_at", Desc)` + id tiebreak, limit default 20 max 100 (clamp >100), errors mapped to OData resource type (use platform extractor/mapping).

- [ ] **Step 1: Write failing tests** `tests/chats.rs`:
  - `create_returns_201_with_location_and_default_model` (status 201, header `Location == "/mini-chat/v1/chats/{id}"`, body `model` == first enabled `is_default`, no `title` key, `message_count==0`, `is_temporary==false`).
  - `create_with_unknown_or_disabled_model_is_400_invalid_model`; `create_without_enabled_models_is_400_invalid_model`.
  - `title_validation`: `"   "` → 400 INVALID_TITLE; 256 chars → 400; `"  hi  "` → title `"hi"`; 255 × `"é"` → 201 (Review Focus 1); invalid title with unknown model → INVALID_TITLE (validated first).
  - `patch_renames_and_ignores_model` (`{"title":"Renamed","model":"other"}` → 200, model unchanged, `updated_at` increased); `patch_missing_title_is_422`; `patch_without_json_content_type_is_415`; `malformed_json_is_400`.
  - `get_and_delete`: delete → 204; get → 404 chat; second delete → 404; `chat_cleanup` outbox row exists with payload `reason=="chat_soft_delete"`.
  - `list_orders_by_updated_at_desc_and_paginates` (3 chats, `limit=2` → 2 items + `next_cursor`, follow cursor → 1 item); `list_limit_over_100_clamped` (`page_info.limit == 100`); `list_limit_zero_is_400`; `list_filter_contains_title`; `list_unknown_field_is_400_odata_resource_type` (`context.resource_type=="gts.cf.core.odata.query.v1~"`); `list_bad_cursor_is_400`; `list_select_accepted_and_ignored`.
  - `isolation`: user B (same tenant) and user C (other tenant) get 404 on A's chat for GET/PATCH/DELETE and do not see it in list; non-UUID path → 400 `invalid_path_params`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; commit `feat(mini-chat): chat CRUD`.

---

### Task 5: LLM provider layer — types, OpenAI Responses adapter, sanitizer, provider resolver, OAGW provisioning, fake OAGW

Covers: S§9.1, S§9.4, S§1.2, ADR-0005. AC: "Provider-originated error details are sanitized", "Streaming responses are never buffered" (adapter part), "Tool availability … reflected in the assembled request" (request building).

**Files:** `src/infra/llm/{types.rs, sse_parser.rs, sanitize.rs, provider_resolver.rs, gateway.rs, providers/{mod.rs, openai_responses.rs}}` + `_tests.rs`, `src/infra/oagw_provisioning.rs` (+tests), `tests/fake_oagw.rs` (shared fake used via `mod`), `src/gear.rs` (start: S2S exchange + provisioning + reconcile loop).

**Interfaces:**
- `llm::types`: `LlmRequest { model: String /*provider_model_id*/, instructions: String, input: Vec<InputMessage>, max_output_tokens: u32, tools: Vec<ToolSpec>, max_tool_calls: u32, api_params: ApiParams, user: String, metadata: RequestMetadata, stream: bool }`; `InputMessage { role: Role /*User|Assistant*/, content: Vec<ContentPart> }`; `ContentPart::{Text(String), Image { file_id: String }}`; `ToolSpec::{FileSearch { vector_store_ids: Vec<String>, max_num_results: u32 }, WebSearch { search_context_size: String }, CodeInterpreter { file_ids: Vec<String> }, Function { name, description, parameters: Value }}`; `RequestMetadata { tenant_id, user_id, chat_id, request_type: &'static str /*chat|summary*/, feature: String }`; `LlmEvent::{TextDelta(String), ReasoningDelta(String), ToolStart { name, details: Value }, ToolDone { name, details: Value }, Citation(RawCitation), Completed(LlmTerminal), Incomplete { terminal: LlmTerminal, reason: String }, Failed(ProviderFailure), FunctionCall { call_id, name, arguments }}`; `LlmTerminal { usage: Option<UsageTokens>, response_id: Option<String> }`; `RawCitation::{Url { url, title, start, end, snippet }, File { provider_file_id, filename }}`; `ProviderFailure { code: StreamErrorCode, message: String /*sanitized*/, usage: Option<UsageTokens> }`; `StreamErrorCode::{ProviderError, ProviderTimeout, RateLimited}` (as_str → snake_case codes); `LlmCompletion { text: String, usage: Option<UsageTokens>, response_id }`.
- `providers::ProviderAdapter` trait: `fn build_body(&self, req: &LlmRequest) -> serde_json::Value`; `fn parse_event(&self, event: &SseEvent, state: &mut ParseState) -> Vec<LlmEvent>`; `fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderFailure>`. `OpenAiResponsesAdapter` implements it per S§9.1 (exact keys: `model, instructions, input, stream, max_output_tokens, store:false, tools, max_tool_calls, include, user, metadata, temperature, top_p, frequency_penalty, presence_penalty, stop, reasoning`; extra_body merge skipping controlled keys).
- `llm::gateway::LlmGateway` (impl port below): `async fn stream(&self, target: &ProviderTarget, req: LlmRequest, cancel: CancellationToken) -> Result<BoxStream<'static, LlmEvent>, ProviderFailure>`; `async fn complete(&self, target, req) -> Result<LlmCompletion, ProviderFailure>`; maps HTTP status (429 → RateLimited with `retry in {N}s`, OAGW 504 deadline_exceeded/gateway timeout → ProviderTimeout, else ProviderError) and drops the OAGW response body when `cancel` fires. Port in `domain/ports.rs`: `#[async_trait] pub trait LlmPort: Send + Sync { async fn stream(...); async fn complete(...); }` with the same signatures.
- `ProviderResolver`: `fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError>`; `fn resolve_storage(&self, provider_id: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError>` (follows `rag_provider`); `ProviderTarget { provider_id, kind: ProviderKind, alias: String, api_path: String }`; `StorageTarget { provider_id, storage_kind, alias, api_version: Option<String>, storage_backend: String }`.
- `sanitize::sanitize_provider_message(&str) -> String` (rules S§9.4); `sanitize::provider_user_field(tenant: Uuid, user: Uuid) -> String` (64 hex).
- `oagw_provisioning::provision_all(gw, s2s_ctx, cfg) -> ProvisionReport { ok, deferred: Vec<String> }` creating upstream (endpoint host/port/scheme, auth plugin+config, alias) — reuse on already-exists — and routes: POST chat route from `api_path` (`{model}` as path suffix), RAG routes for entries with `storage_kind` (S§9.3 prefixes, api-version query allowlist for azure).
- `tests/fake_oagw.rs`: `FakeOagw` implements `ServiceGatewayClientV1`; `push_sse(path_prefix, Vec<(event, json)>)`, `push_json(path_prefix, status, json)`, `push_sse_slow(..., delay)`, `requests() -> Vec<RecordedRequest { method, uri, json_body: Option<Value>, multipart_fields: Vec<(name, filename, content_type)> }>`; unmatched requests → 404 JSON; upstream/route CRUD recorded and succeed.

- [ ] **Step 1: Failing unit tests** `openai_responses_tests.rs`:
  - `body_minimal_chat`: no tools → JSON has `stream:true`, `store:false`, `max_tool_calls:2`, no `tools` key or empty, `user` length 64, `metadata.request_type=="chat"`, `metadata.feature=="none"`, `instructions` == system prompt, last `input` item role user content string.
  - `body_with_all_tools`: file_search → `{"type":"file_search","vector_store_ids":["vs_1"],"max_num_results":5}`; web_search → `{"type":"web_search","search_context_size":"low"}`; code_interpreter → `{"type":"code_interpreter","container":{"type":"auto","file_ids":["file-x"]}}` and `include == ["code_interpreter_call.outputs"]`; `metadata.feature == "file_search+web_search+code_interpreter"`.
  - `body_images`: current user content `[{"type":"input_text",…},{"type":"input_image","file_id":"file-img"}]`.
  - `api_params_sent_only_when_set`, `extra_body_merged_but_controlled_keys_ignored` (`extra_body {"foo":1,"model":"x"}` → `foo==1`, model unchanged).
  - `parse_stream`: sequence `response.output_text.delta`×2, `response.file_search_call.searching`, `.completed` (results 2 → `files_searched 2`), `response.web_search_call.searching/completed`, `response.code_interpreter_call.in_progress`, `response.output_item.done` with code_interpreter logs (→ ToolDone details `output` joined, 9000 chars → capped 8192 + `...[truncated]`), annotations url_citation/file_citation, `response.completed` with usage incl. `input_tokens_details.cached_tokens` and `output_tokens_details.reasoning_tokens` → expected `LlmEvent`s. Event name from `type` when `event:` missing.
  - `parse_incomplete`, `parse_failed_uses_response_error_and_keeps_usage`, `parse_error_event_flat`.
- [ ] **Step 2: Failing tests** `sanitize_tests.rs`: `"file-abc123def456ghi not found"` → `"[provider_id] not found"`; `"vs_abcdefghijklmn"` → `[provider_id]`; `"file-based"`/`"file_search"` unchanged; `"see https://x.y/z"` → `see [url]`; `"sk-ABCDEFGHIJ12"` → `[credential]`; `"Bearer abc.def"` → `[credential]`; `"resp_123abc"` → `[provider_id]`. `provider_user_field` → 64 lowercase hex.
- [ ] **Step 3: Failing tests** `gateway_tests.rs` (FakeOagw): `http_429_maps_rate_limited_with_retry_after` (`message` contains `retry in 7s`), `http_500_json_error_is_provider_error_sanitized` (Review Focus 3 unit part), `oagw_504_is_provider_timeout`, `stream_yields_deltas_before_completion` (slow fake: first delta observed while provider still open — no buffering), `cancel_drops_stream`.
- [ ] **Step 4: Failing tests** `oagw_provisioning_tests.rs`: `creates_upstream_and_routes_per_entry_and_override`, `existing_alias_is_reused`, `azure_rag_routes_have_api_version`.
- [ ] **Step 5: Run** → FAIL. **Step 6: Implement.** **Step 7: Run** → PASS; build; commit `feat(mini-chat): llm provider layer and OAGW provisioning`.

---

### Task 6: RAG storage clients (files, vector stores; OpenAI & Azure dispatch)

Covers: S§9.3.

**Files:** `src/infra/llm/storage/{mod.rs, files.rs, vector_stores.rs}` + tests.

**Interfaces (port in `domain/ports.rs`):**
```rust
#[async_trait] pub trait StoragePort: Send + Sync {
    async fn upload_file(&self, t: &StorageTarget, filename: &str, content_type: &str, bytes: Bytes) -> Result<String /*file id*/, StorageError>;
    async fn delete_file(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError>; // 2xx/404 Ok
    async fn create_vector_store(&self, t: &StorageTarget, chat_id: Uuid) -> Result<String, StorageError>;
    async fn add_file_to_vector_store(&self, t: &StorageTarget, vs: &str, file_id: &str, attachment_id: Uuid) -> Result<IndexStatus, StorageError>;
    async fn vector_store_file_status(&self, t: &StorageTarget, vs: &str, file_id: &str) -> Result<IndexStatus, StorageError>;
    async fn delete_vector_store(&self, t: &StorageTarget, vs: &str) -> Result<(), StorageError>; // 2xx/404 Ok
}
pub enum IndexStatus { InProgress, Completed, Failed /* failed|cancelled|unknown */ }
pub enum StorageError { Transient(String) /*5xx, gateway*/, Permanent(String) }
```
Implementation `RagStorage` over OAGW (`/{alias}/v1/...` or `/{alias}/openai/...?api-version=`), multipart via `oagw_sdk::MultipartBody` (`purpose=assistants`, `file`).

- [ ] **Step 1: Failing tests:** `upload_openai_multipart_fields` (recorded request `POST /{alias}/v1/files`, fields `purpose=assistants` and `file` filename as given), `azure_paths_have_api_version` (`/openai/files?api-version=2025-03-01-preview`), `delete_404_is_ok`, `delete_500_is_transient`, `vector_store_add_sends_attachment_attribute` (`attributes.attachment_id`), `missing_status_is_in_progress`, `cancelled_status_is_failed`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; commit `feat(mini-chat): rag storage clients`.

---

### Task 7: Quota service, estimation, credits, billing derivation, quota status API

Covers: S§7, D§5. AC: "Quota is checked before any outbound provider call", "Quota Status API", "Quota Enforcement" (3), "Settlement & Finalization" (settlement math part).

**Files:** `src/domain/estimation.rs`, `src/domain/billing.rs`, `src/domain/services/quota_service.rs` (+ `_tests.rs` each), `QuotaUsageRepo` methods, `src/api/rest/handlers/quota.rs`, `tests/quota_status.rs`.

**Interfaces:**
- `estimation`: `fn credits_micro(input: i64, output: i64, in_mult: i64, out_mult: i64) -> Result<i64, CreditError>`; `fn estimate_text_tokens(bytes: usize, b: &EstimationBudgets) -> i64`; `pub struct ReserveInputs { message_bytes: usize, prior_context_tokens: i64, image_count: u32, chat_has_ready_docs: bool, chat_has_ready_xlsx: bool, web_search_requested: bool }`; `fn candidate_reserve(m: &ModelCatalogEntry, i: &ReserveInputs, ks: &KillSwitches, cfg_max_out: u32, cfg_floor: u32) -> ReservePlan` where `ReservePlan { estimated_input_tokens, max_output_tokens_applied, reserve_tokens, reserved_credits_micro /* i64::MAX on error */, minimal_generation_floor_applied, tools: ToolGates { file_search, web_search, code_interpreter } }`.
- `billing`: `enum BillingOutcome { Completed, Failed, Aborted }`, `enum SettlementMethod { Actual, Estimated, Released }`; `fn derive(state: TurnState, error_code: Option<&str>, usage: Option<&UsageTokens>) -> (BillingOutcome, SettlementMethod)` (D§5.8 table; failed "usage known" requires non-zero field; cancelled/orphan always estimated); `fn settle_amount(method, turn: &TurnReserve, usage: Option<&UsageTokens>, tolerance: f64, mults: (i64,i64)) -> Result<Settlement, CreditError>` with `TurnReserve { reserve_tokens, max_output_tokens_applied, reserved_credits_micro, minimal_generation_floor_applied }`, `Settlement { committed_credits_micro, actual_tokens_for_telemetry: Option<(i64,i64)>, overshoot: bool }`.
- `QuotaService`: `async fn preflight(&self, tenant: Uuid, user: Uuid, selected_model: &str, snap: &PolicySnapshot, limits: &UserLimits, inputs: &ReserveInputs, now: OffsetDateTime) -> Result<PreflightDecision, DomainError>` → `PreflightDecision { effective: ModelCatalogEntry, effective_tier: ModelTier, decision: QuotaDecision /*Allow|Downgrade*/, downgrade_reason: Option<&'static str>, plan: ReservePlan, periods: PeriodStarts { daily: Date, monthly: Date }, policy_version: u64 }` (checks daily tool quotas, returns `QuotaExceeded(scope)`); `async fn reserve(&self, tx: &DbTx<'_>, tenant, user, d: &PreflightDecision, limits: &UserLimits) -> Result<(), DomainError>` (increments + re-check → `QuotaExceeded(Tokens)`); `async fn settle(&self, tx: &DbTx<'_>, s: &SettleInput) -> Result<SettleResult, DomainError>` where `SettleInput { tenant, user, effective_tier, periods, turn: TurnReserve, method, settlement, web_search_calls, code_interpreter_calls }`; `async fn status(&self, ctx) -> Result<QuotaStatus, DomainError>`; `async fn warnings(&self, tx/runner, tenant, user, limits, now) -> Vec<QuotaWarning>`; helper `fn period_starts(now) -> PeriodStarts`, `fn period_starts_from(started_at)`.

- [ ] **Step 1: Failing unit tests** (`estimation_tests.rs`, `billing_tests.rs`, `quota_service_tests.rs`):
  - `credits_ceil_per_component`: `credits_micro(1,1,1_000_000,3_000_000)==1+3`; `(1500, 0, 1, 1) == 1`; zero mult → Err; tokens 10_000_001 → Err.
  - `text_estimate`: bytes 10, bpt 4, overhead 100, margin 10 → `ceil((3+100)*110/100)=114`; empty → `110`.
  - `surcharges_gated_by_tool_support_and_kill_switch` (file_search surcharge only with ready docs & tool_support & !disable_file_search; same for CI; web search only when requested & supported).
  - `cascade_truth_tables`: premium daily exhausted → standard, reason `premium_quota_exhausted`; standard monthly exhausted → `QuotaExceeded(Tokens)`; `force_standard_tier` → reason `force_standard_tier`; disabled selected → `model_disabled`; standard never upgrades; candidate reserve larger than remaining → unavailable; default-of-tier selection.
  - `daily_web_search_quota_only_when_tool_sent` (usage 75 & requested & supported → `QuotaExceeded(WebSearch)`; not requested → OK; model without web_search → OK); same for code interpreter (requires ready XLSX & support & kill switch off).
  - `billing_derivation_table`: one assertion per D§5.8 row incl. `cancelled→(Aborted,Estimated)`, `orphan_timeout→(Aborted,Estimated)`, `provider_error + usage{0,0} → Estimated`, `turn_setup_failed → Released`, unknown code → `(Failed, Estimated)`.
  - `settlement_overshoot_cap`: reserve 10000 tokens / 2_500_000 credits, usage 11000+500, tolerance 1.10 → committed 2_500_000; usage 10500+0 → actual credits.
  - `estimated_settlement`: `credits(est_input, floor)`.
- [ ] **Step 2: Failing integration tests** (`tests/quota_status.rs`): `status_reports_premium_and_total_daily_monthly` (seed `quota_usage` rows → `used = spent+reserved`, `remaining_percentage` floor, `warning` at ≤20%, `exhausted` at 0, `next_reset` tomorrow 00:00Z / 1st next month, `warning_threshold_pct==80`); `limit_le_zero_period_omitted`; `reserve_recheck_rejects_concurrent_overbooking` (two reserves where only one fits → second `QuotaExceeded(Tokens)`, rows unchanged by rollback).
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS; commit `feat(mini-chat): quota service and billing`.

---

### Task 8: Context assembly and tool selection

Covers: S§8, D§4 "Context Plan…". AC: "Context window budget enforced…", "Context Assembly" (2), "Thread summary … invalidation" (consumer side).

**Files:** `src/domain/context.rs`, `src/domain/tools.rs` (+ tests).

**Interfaces:**
- `tools::select_tools(eff: &ModelCatalogEntry, ks: &KillSwitches, facts: &ChatToolFacts, web_search_requested: bool, knowledge: Option<&KnowledgeParams>) -> SelectedTools` with `ChatToolFacts { vector_store_id: Option<String>, has_ready_docs: bool, code_interpreter_file_ids: Vec<String> }`, `SelectedTools { specs: Vec<ToolSpec>, file_search: bool, web_search: bool, code_interpreter: bool, knowledge: bool, feature_label: String }`.
- `context::assemble(input: ContextInput) -> Result<ContextPlan, DomainError>`; `ContextInput { eff: &ModelCatalogEntry, max_output_tokens_applied: u32, system_prompt: &str, guards: Vec<&str>, summary: Option<SummaryForContext { text, token_estimate }>, recent: Vec<HistoryMessage { id, role, content, created_at }>, current_text: &str, current_images: Vec<String /*provider file ids*/>, surcharges: i64 }`; `ContextPlan { instructions: String, input: Vec<InputMessage>, messages_truncated: bool, assembled_context_tokens: i64, summary_applied: Option<i64 /*token_estimate*/>, effective_budget: i64 }`; constant `SUMMARY_PREAMBLE` (D B.5.5 exact text); `fn input_too_long(eff, current_text) -> bool`.

- [ ] **Step 1: Failing tests:** `order_is_system_summary_history_current` (summary as user message starting with SUMMARY_PREAMBLE); `guards_appended_only_for_sent_tools` (web guard text from P§5.2 exactly); `truncates_oldest_whole_turns` (tiny budget → oldest messages dropped, kept range never starts with assistant, `messages_truncated==true`); `summary_dropped_when_it_does_not_fit`; `mandatory_over_budget_is_context_budget_exceeded`; `max_out_ge_context_window_is_context_budget_exceeded`; `max_input_tokens_zero_means_no_limit`; `deterministic_same_inputs_same_plan`; tools: `file_search_requires_ready_docs_and_support_and_switch`, `web_search_dropped_when_model_lacks_support`, `code_interpreter_omitted_when_kill_switch`, `knowledge_off_when_file_search_present`, `feature_label_joined`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; commit `feat(mini-chat): context assembly and tool selection`.

---

### Task 9: Streaming send pipeline, finalization, replay, turn status, messages list

Covers: S§6.1–6.5 (send, relay, finalization, replay, turn status), S§5 messages list. AC: "Messages API" (3), "Streaming: Send Message" (3), "SSE Event Contract" (3), "Idempotency & Replay" (3), "Parallel Turn Enforcement" (2), "Turn Lifecycle" (2), "Streams never buffered", "Quota checked before provider", "Settlement & Finalization" (2), "Error mapping consistent across REST and streaming", "Chat ordering reflects most recent activity".

**Files:** `src/domain/services/{stream_service.rs, finalization.rs, replay.rs, message_service.rs, turn_service.rs (status only)}`, `src/api/rest/{sse.rs, handlers/stream.rs, handlers/messages.rs, handlers/turns.rs (GET)}`, repos (`MessageRepo`, `TurnRepo`, …), `tests/streaming.rs`, `tests/messages.rs`, `tests/turn_status.rs`.

**Interfaces:**
- `StreamService::send(&self, ctx: SecurityContext, chat_id: Uuid, req: SendMessage) -> Result<TurnStream, DomainError>` with `SendMessage { content: String, request_id: Option<Uuid>, attachment_ids: Vec<Uuid>, web_search_enabled: bool }`; `TurnStream::{Live(LiveTurn), Replay(ReplayTurn)}`; both convert to an SSE body via `api::rest::sse::into_sse(stream, ping_interval) -> Response`. `LiveTurn { started: StreamStartedData, events: mpsc::Receiver<StreamEvent>, cancel: CancellationToken /* cancelled when the response body is dropped */ }`. `StreamEvent::{Delta{kind, content}, Tool{phase, name, details}, Citations(Vec<CitationDto>), Done(DoneData), Error{code, message}}` (DTOs per OpenAPI: `StreamStartedData`, `DeltaData`, `ToolData`, `CitationsData`, `DoneData`, `ErrorData`, `Usage`, `QuotaWarning`). Internal entry also used by Task 11: `pub(crate) async fn run_turn(&self, setup: TurnSetup) -> LiveTurn` where `TurnSetup { ctx, chat, turn_id, request_id, assistant_message_id, decision: PreflightDecision, target: ProviderTarget, llm_request: LlmRequest, citation_map: HashMap<String,(Uuid,String)>, tool_flags: SelectedTools, plan: ContextPlan, selected_model: String, limits: UserLimits }`, and `pub(crate) async fn prepare_turn_request(&self, …) -> Result<(PreflightDecision-dependent parts), DomainError>` (implementer chooses internal split; Task 11 reuses context assembly + provider resolution + reserve + `run_turn`).
- `FinalizationService::finalize(&self, f: FinalizeInput) -> Result<FinalizeOutcome, DomainError>` with `FinalizeInput { turn_id, tenant, user, chat_id, request_id, terminal: Terminal /*Completed{text, usage, response_id, incomplete_reason: Option<String>}|Failed{code, detail, usage}|Cancelled{partial_text}*/, assistant_message_id, effective_model, selected_model, effective_tier, periods, reserve: TurnReserve, policy_version, decision, downgrade_reason, tool_counts { web_search, code_interpreter, file_search }, summary_trigger: SummaryTriggerInput, latency }`; `FinalizeOutcome::{Won { committed_state, quota_warnings }, Lost}`; Task 14 adds the summary enqueue inside the same txn via `ThreadSummaryService::maybe_enqueue(tx, …)` (this task calls a no-op hook `SummaryHook` trait so Task 14 only implements it).
- `MessageService::list(ctx, chat_id, &ODataQuery) -> Page<MessageView>`; `MessageView` → `MiniChatMessageDto`.
- `TurnService::status(ctx, chat_id, request_id) -> TurnStatusView`.
- Provider call: `LlmPort::stream` with `ProviderTarget` of the effective model; OAGW ctx = S2S ctx (gear stores it in an `S2sContext` holder; tests inject one).

- [ ] **Step 1: Failing tests** `tests/streaming.rs` (FakeOagw scripted Responses SSE):
  - `send_streams_started_deltas_done`: events `stream_started` (request_id echoes client id, `is_new_turn:true`, uuid `message_id`), `delta`×2 `{type:"text"}`, `done {usage{input_tokens,output_tokens}, effective_model, selected_model, quota_decision:"allow"}` and no `downgrade_*` keys; DB: turn `completed`, assistant message id == `message_id`, model == effective, tokens; user+assistant share request_id; `chats.updated_at` bumped; `quota_usage` total daily/monthly `reserved_credits_micro==0`, `spent>0`, `calls==1`; one `mini-chat.usage_snapshot` outbox row (`billing_outcome:"completed"`, `settlement_method:"actual"`, `dedupe_key` == `{tenant.simple}/{turn.simple}/{request.simple}`) and one audit row (`event_type:"turn_completed"`).
  - `server_generates_request_id_when_omitted` (v4 in stream_started; `GET /turns/{id}` → `done`).
  - `provider_request_contents` (recorded body: model == provider_model_id, `max_output_tokens == min(catalog, 32768)`, `user` 64 hex, `instructions` == system_prompt).
  - `preflight_rejections_are_json_not_sse` (empty/whitespace content → 400 EMPTY_CONTENT; duplicate attachment ids → 400 invalid_attachment; quota exhausted (seeded) → 429 tokens with `content-type: application/problem+json`; web_search with kill switch → 400 FEATURE_DISABLED; and in every case FakeOagw recorded **no** `/responses` request and no turn row).
  - `replay_completed_request_id`: second POST same request_id → `stream_started{is_new_turn:false, message_id == persisted}`, one delta with full text, `done` (no `quota_warnings`), and no new provider request, quota rows unchanged, no new outbox rows.
  - `replay_with_downgrade_has_downgrade_from_without_reason`.
  - `request_id_conflict_for_failed_or_running_or_deleted` (409 `request_id_conflict`).
  - `replay_checked_before_parallel_guard`: turn A completed; turn B running (slow fake); POST with A's request_id → replay 200, POST new id → 409 `turn_already_running`.
  - `new_turn_accepted_after_terminal` (after B completes → 200).
  - `concurrent_sends_one_wins` (Review Focus 4: two parallel POSTs with slow fake → statuses {200, 409 turn_already_running}).
  - `provider_http_500_sanitized_error_event` (Review Focus 3: `error {code:"provider_error", message}` without `file-`/`resp_` ids; turn `failed` with `error_code=provider_error`; quota settled estimated; follow-up send → 200).
  - `provider_429_rate_limited`, `provider_failed_event_settles_actual_when_usage_nonzero`.
  - `client_disconnect_cancels_turn` (Review Focus 2: read stream_started + first delta then drop body → within 2 s turn `cancelled`, partial assistant message persisted, turn status `cancelled` with `assistant_message_id`; usage event `billing_outcome:"aborted"`, `settlement_method:"estimated"`; next send → 200; no SSE error was produced).
  - `ping_before_first_delta` (`sse_ping_interval_seconds=5`… use test override 1 s via config + slow first delta → at least one `ping` event with `{}` before first delta, none after).
  - `deltas_relayed_before_provider_finishes` (slow fake: first delta received while fake still holds the stream open).
  - `citations_once_before_done` (file citation mapped to attachment id & filename; unknown provider file id omitted; web citation with url/title/snippet/span).
  - `web_search_calls_exceeded_mid_stream` (3 web_search starts with limit 2 → `error{web_search_calls_exceeded}`, turn failed, settlement estimated); `code_interpreter_calls_exceeded`.
  - `done_quota_warnings_entries` (seed usage at 85% → entry `warning:true` with `next_reset`; entries without warning lack `next_reset`).
  - `model_removed_from_catalog_is_400_invalid_model`; `disabled_model_downgrades_with_model_disabled` (done `downgrade_reason:"model_disabled"`).
  - `premium_exhausted_downgrades` (done `quota_decision:"downgrade"`, `effective_model != selected_model`, `downgrade_from == selected_model`, `downgrade_reason:"premium_quota_exhausted"`; `messages.model` == effective).
  - `input_too_long_and_context_budget` (400 INPUT_TOO_LONG / CONTEXT_BUDGET_EXCEEDED with no provider call).
  - `stream_interrupted_when_cas_lost` (pre-finalize the turn in DB while the fake is mid-stream → client gets `error{stream_interrupted}`).
  - `chat_list_order_reflects_last_send` (chat A created first, send in A → A listed first).
- [ ] **Step 2: Failing tests** `tests/messages.rs`: `list_messages_chronological_with_required_fields` (`request_id` present, `attachments == []`, `my_reaction` key present null, user message has no `model`/tokens keys, assistant has `model`, tokens), `messages_filter_orderby_pagination` (`$filter=role eq 'assistant'`, `$orderby=created_at desc`, `limit=1` + cursor, `limit=0` → 400, unknown field → 400), `message_count_tracks_turns` (`GET /chats/{id}` `message_count == 4` after two turns), `foreign_chat_messages_404`. `tests/turn_status.rs`: `status_mapping_running_done_error_cancelled`, `error_code_and_assistant_id_omitted_when_null`, `foreign_and_unknown_turn_404`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement** (spawned setup task per S§6.1; bounded channel `sse_channel_capacity`; `last_progress_at` refresh ≤ every 30 s; terminal events only after finalization commit; `Sse::keep_alive` 30 s after content). **Step 5: Run** → PASS; build; commit `feat(mini-chat): streaming send, finalization, replay, messages, turn status`.

---

### Task 10: Attachments (upload/get/delete), vector store protocol, thumbnails, background indexing, send-path integration

Covers: S§11; D§3.3 upload, D§3.6 File Upload, D§3.7 chat_vector_stores, ADR-0007. AC: "Attachments" (4), "Preflight validation (content, attachments, limits)", "Attachments correctly made available to provider tools", "Web Search" (tool/citation/quota parts not yet covered), "Image-specific" behaviors.

**Files:** `src/domain/services/attachment_service.rs`, `src/infra/{mime.rs, thumbnail.rs}`, `src/infra/workers/background_indexing.rs`, `src/api/rest/handlers/attachments.rs`, repos (`AttachmentRepo`, `VectorStoreRepo`, `MessageAttachmentRepo`), stream_service integration (attachment validation in reserve txn, images in input, code-interpreter file ids, file_search vector store id, citation map), `tests/attachments.rs`, `tests/attachments_in_turns.rs`.

**Interfaces:**
- `AttachmentService::upload(&self, ctx, chat_id, part: UploadPart) -> Result<AttachmentDetail, DomainError>` where the handler first calls `AttachmentService::prepare_upload(ctx, chat_id) -> Result<UploadContext { chat, model: ModelCatalogEntry, storage: StorageTarget, limits: UploadLimits, permit: OwnedSemaphorePermit, code_interpreter_available: bool, disable_images: bool }, DomainError>` before reading the body; `UploadPart { filename: String, content_type: String, stream: impl Stream<Item = Result<Bytes, …>> }` (implementer may buffer up to the limit — documents ≤ 25 MiB). `get(ctx, chat_id, attachment_id) -> AttachmentDetail`; `delete(ctx, chat_id, attachment_id) -> Result<(), DomainError>`. `AttachmentDetail` → `AttachmentDetailDto`.
- `mime::resolve(content_type: &str, filename: &str, allow_csv: bool) -> Result<ResolvedMime { mime: String, kind: AttachmentKind, for_file_search: bool, for_code_interpreter: bool, ext: String }, DomainError>`; `mime::sanitize_filename(raw: Option<&str>) -> String`.
- `thumbnail::make_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail { webp: Vec<u8>, width: u32, height: u32 }>`.
- `background_indexing::spawn(deps, attachment_id, …, cancel)` (gear cancellation token child).
- Indexing timings are constants with test overrides via config-free injection (`IndexingTimings { deadline: 25s, bg_round: 20s, bg_total: 10min, … }` passed in from `AppServices`; tests use milliseconds).

- [ ] **Step 1: Failing tests** `tests/attachments.rs` (FakeOagw scripts `/v1/files` → `{"id":"file-…"}`, vector store create/add/status):
  - `upload_document_ready_201` (status 201, `status:"ready"`, `kind:"document"`, no `error_code`/`doc_summary`/`img_thumbnail` keys; DB `for_file_search=true`, `provider_file_id` set, `chat_vector_stores` row with vector_store_id; provider requests: files POST with `purpose=assistants`, filename `{chat_id}_{attachment_id}.pdf`, vector store POST, file add with `attributes.attachment_id`).
  - `second_document_reuses_vector_store` (one create call).
  - `indexing_still_running_returns_uploaded_then_ready` (status in_progress until after deadline → 201 `uploaded`; poll GET until `ready`).
  - `indexing_failed_is_503_and_row_failed` (status `failed` → 503 `Retry-After: 10`, GET shows `status:"failed"`, `error_code:"indexing_failed"`, provider file DELETE recorded).
  - `background_indexing_failure_enqueues_cleanup` (in_progress → failed later → row failed, `cleanup_status:"pending"`, attachment cleanup outbox row `event_type:"attachment_indexing_failed"`).
  - `upload_image_ready_with_thumbnail` (PNG 300×200 → `img_thumbnail.content_type=="image/webp"`, width 128, height 85, decodes; no vector store call).
  - `xlsx_ready_for_code_interpreter_no_vector_store`; `xlsx_rejected_when_code_interpreter_unavailable` (kill switch or model without CI → 400 `field_violations[file].reason=CODE_INTERPRETER_UNAVAILABLE`).
  - `limits`: doc over `min(rag limit, model max_file_size_mb)` → 400 FILE_TOO_LARGE; image over 5120 KiB → 400; unsupported mime → 400 UNSUPPORTED_CONTENT_TYPE; `application/octet-stream` + `.pdf` → accepted as pdf; `text/csv` → stored `text/plain`; per-chat document limit (config 1) → 429 `document_limit`; storage limit (config 1 MB) → 429 `storage_limit`, failed attachments don't count; `disable_images` → 400 images FEATURE_DISABLED; concurrency limit 1 with one held permit → 503 `Retry-After: 5`.
  - `multipart_errors` (no boundary → BOUNDARY_REQUIRED, no file field → MISSING_FILE, part without content type → MISSING_CONTENT_TYPE).
  - `filename_rules` (Review Focus 5: missing → `upload`; `../../etc/passwd.txt` → `passwd.txt`; 300-char name `.pdf` → 255 chars ending `.pdf`).
  - `upload_to_unknown_chat_404_chat_before_body`; `chat_model_removed_is_400_invalid_model`; `provider_mismatch_409` (existing vector store row with other provider label).
  - `get_delete_rules`: other user's attachment in my chat → 404 attachment on GET and DELETE; delete → 204 + attachment cleanup outbox row `event_type:"attachment_deleted"`; re-delete → 204 with no second outbox row; referenced attachment (sent in a message) → 409 `attachment_locked`.
- [ ] **Step 2: Failing tests** `tests/attachments_in_turns.rs`:
  - `image_in_message_sent_as_input_image` (recorded `/responses` body has `input_image.file_id`), message `attachments[0]` has `kind:"image"`, `img_thumbnail`.
  - `file_search_tool_only_after_ready_document` (before upload: no tools; after: `file_search` with vector_store_ids; deleted document only remaining → no file_search) and guard text in `instructions`.
  - `code_interpreter_tool_with_ready_xlsx` (+`include`), omitted with kill switch.
  - `attachment_validation_in_reserve_txn` (foreign/other-chat/not-ready/unknown id → 400 invalid_attachment, quota `reserved_credits_micro` unchanged, no turn/message rows, no provider call); `too_many_images` (5 > 4 → 400 TOO_MANY_IMAGES); `vision_not_supported_after_downgrade` (premium exhausted, standard model without VISION_INPUT → 400 VISION_NOT_SUPPORTED, no provider call); `images_kill_switch_on_message`.
  - `web_search_tool_and_daily_quota` (enabled → tool present + guard text exact; seeded 75 calls → 429 `web_search`; model without support → no tool, no 429, `chat_turns.web_search_enabled == true`); `web_citations_source_web`; `web_search_calls_counted_in_quota_usage` (2 completed calls → `quota_usage.web_search_calls == 2`).
  - `code_interpreter_daily_quota_429`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement** (vector-store creation protocol incl. placeholder, loser polling, CAS, 120 s stale reclaim). **Step 5: Run** → PASS; build; commit `feat(mini-chat): attachments`.

---

### Task 11: Turn mutations (retry, edit, delete)

Covers: S§6.5, D§3.9. AC: "Turn Mutations" (4), "Thread summary … mutation-driven invalidation" (mutation side).

**Files:** `src/domain/services/turn_service.rs` (preview/commit), stream_service reuse, `src/api/rest/handlers/turns.rs` (retry/edit/delete), `tests/turn_mutations.rs`.

**Interfaces:**
- `TurnService::retry(ctx, chat_id, request_id) -> Result<LiveTurn, DomainError>`; `edit(ctx, chat_id, request_id, content: String) -> Result<LiveTurn, DomainError>`; `delete(ctx, chat_id, request_id) -> Result<(), DomainError>`; internal `preview(ctx, chat_id, request_id, action) -> Result<MutationTarget, DomainError>` and `commit(…) -> Result<NewTurn, DomainError>`; uses `StreamService` helpers from Task 9 and `QuotaService` from Task 7.

- [ ] **Step 1: Failing tests** `tests/turn_mutations.rs`:
  - `retry_latest_creates_new_turn` (SSE `stream_started.request_id` ≠ old; old turn `deleted_at` set + `replaced_by_request_id`; GET old turn → 404; messages list shows one user + one new assistant; `chats.updated_at` bumped; audit outbox `turn_retry` with `original_request_id`/`new_request_id`; usage event for new turn).
  - `edit_uses_new_content_and_copies_attachments` (image attachment copied, deleted attachment excluded; provider body has new content and `input_image`).
  - `delete_latest_204_and_audit` (`turn_delete`); next `GET /messages` excludes it.
  - `non_latest_is_409_not_latest_turn`; `already_deleted_is_409_not_latest_turn`; `running_is_400_turn_state`; `other_users_turn` → 404 (foreign chat) / 403 when same chat requester differs (insert turn with other requester); `unknown_turn_404`; `edit_empty_content_400`.
  - `quota_rejection_leaves_previous_turn_intact` (seed exhausted → 429, old turn still not deleted, no new turn).
  - `setup_failure_after_commit_marks_new_turn_failed` (context budget too small → 400 CONTEXT_BUDGET_EXCEEDED; new turn `failed` with `context_length_exceeded`; no usage event for it).
  - `concurrent_retries_deterministic` (two parallel retries → one 200, other 409 `GENERATION_IN_PROGRESS` or `NOT_LATEST_TURN`, never 500; exactly one running/new turn).
  - `old_request_id_after_retry_conflicts` (POST stream with old id → 409 request_id_conflict).
  - `mutation_deletes_covering_summary` (insert `thread_summaries` row whose frontier ≥ the latest user message → after delete/retry the row is gone and `is_compressed` cleared).
  - `retry_resends_web_search_flag` (stored `web_search_enabled` → tool present in new request).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; commit `feat(mini-chat): turn mutations`.

---

### Task 12: Reactions API

Covers: D "Message Reaction API". AC: "Reactions API".

**Files:** `src/domain/services/reaction_service.rs`, `src/api/rest/handlers/reactions.rs`, `ReactionRepo`, `tests/reactions.rs`; message list enrichment (`my_reaction`) in `MessageService`.

**Interfaces:** `ReactionService::set(ctx, chat_id, msg_id, reaction: &str) -> Result<ReactionView { message_id, reaction, created_at }, DomainError>` (validates `like|dislike` before authz → InvalidReaction); `remove(ctx, chat_id, msg_id) -> Result<(), DomainError>`.

- [ ] **Step 1: Failing tests:** `set_like_then_dislike_replaces` (200 bodies, one DB row, list shows `my_reaction:"dislike"`), `remove_is_idempotent_204`, `user_message_rejected_400_reaction_target_for_put_and_delete`, `invalid_value_400_before_authz` (with PDP Deny mode → still 400 INVALID_REACTION), `missing_field_422`, `unknown_message_404_message`, `foreign_chat_404_chat`, `my_reaction_is_per_user` (only the caller's reaction is shown).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; commit `feat(mini-chat): reactions`.

---

### Task 13: Outbox pipeline and handlers (usage, audit, attachment cleanup, chat cleanup)

Covers: S§10.1. AC: "Usage is published reliably and exactly once per turn", "Chat deletion triggers reliable background cleanup", "Cleanup and abandoned-upload recovery…" (handler side).

**Files:** `src/infra/outbox/{mod.rs, handlers/*.rs}` (+tests), `src/domain/services/cleanup.rs`, `src/gear.rs` (start pipeline; stop drains), `tests/outbox_handlers.rs`.

**Interfaces:** `infra::outbox::start_pipeline(db: Db, cfg: &MiniChatConfig, deps: HandlerDeps) -> Result<OutboxHandle, anyhow::Error>` registering five leased queues (thread summary handler from Task 14 plugged via `HandlerDeps.thread_summary: Arc<dyn ThreadSummaryRunner>`; until then a stub returning `Ok`). Handler result mapping exactly per S§10.1. `CleanupService::{process_attachment_cleanup(payload, attempt) -> HandlerOutcome, process_chat_cleanup(payload, attempt) -> HandlerOutcome}` with `HandlerOutcome::{Ok, Retry, Reject(String)}`.

- [ ] **Step 1: Failing tests** `tests/outbox_handlers.rs` (call handler functions directly with constructed messages + start-pipeline smoke):
  - `usage_published_once` (FakePolicy records one `publish_usage` with the event JSON; transient error → Retry; permanent → Reject; corrupt payload → Reject).
  - `audit_semantics` (no plugin → Ok and counted dropped; plugin registered later is used; client missing → Retry; timeout → Retry; corrupt → Reject even without plugin; attempt 120 Retry → Reject).
  - `attachment_cleanup_deletes_provider_file` (DELETE `/v1/files/file-…` recorded; row `cleanup_status:"done"`); `attachment_cleanup_404_is_success`; `attachment_cleanup_failure_counts_and_terminal_failed` (500 → attempts 1 + Retry; at `max_attempts` → `failed` + Reject); `attachment_cleanup_skips_deleted_chat`.
  - `chat_cleanup_deletes_files_then_vector_store` (2 attachments + vector store → 2 file DELETEs then vector store DELETE, `chat_vector_stores` row removed, attachments `done`); `chat_cleanup_rejects_active_chat`; `vector_store_delete_failure_retries_then_rejects_at_max`.
  - `pipeline_end_to_end`: start pipeline with real handlers; delete a chat with an attachment via REST → within 3 s FakeOagw recorded the DELETE calls.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; build; commit `feat(mini-chat): outbox handlers and pipeline`.

---

### Task 14: Thread summary (trigger + handler + context integration)

Covers: S§10.2, D§3.6 "Thread Summary Update", B.5.5. AC: "Thread summary generation, failure/retry, and mutation-driven invalidation", "System prompt, thread summary, and recent history are assembled…".

**Files:** `src/domain/services/thread_summary.rs` (+tests), `src/infra/outbox/handlers/thread_summary.rs`, finalization hook impl, stream_service context loading (summary + frontier), `tests/thread_summary.rs`.

**Interfaces:** `ThreadSummaryService` implements Task 9's `SummaryHook` (`async fn maybe_enqueue(&self, tx: &DbTx<'_>, input: &SummaryTriggerInput, wakes: &mut PendingWakes) -> Result<SummaryTriggerResult, DomainError>`) and Task 13's `ThreadSummaryRunner` (`async fn run(&self, payload: ThreadSummaryPayload, attempt: u32) -> HandlerOutcome`). Prompt builders `fn build_summary_prompt(existing: Option<&str>, msgs: &[HistoryMessage], limit: usize) -> String`, `fn parse_summary(text: &str) -> String`.

- [ ] **Step 1: Failing tests:** unit — `prompt_first_summary_starts_with_summarize_line`, `prompt_with_existing_summary_block`, `content_truncated_with_ellipsis`, `parse_extracts_summary_block`, `parse_without_tags_containing_markup_is_empty`, `token_estimate_fallback_bytes_div_4`. Integration (`tests/thread_summary.rs`, model `context_window` 4096 like `gpt-4.1-mini-tiny-ctx`): `proactive_trigger_enqueues_with_frozen_target` (turn crossing 80% → `mini-chat.thread_summary` outbox row; target = last message before the causing turn), `handler_commits_summary_and_marks_compressed` (FakeOagw non-stream `/responses` returns `<analysis>…</analysis><summary>S</summary>` → `thread_summaries.summary_text=="S"`, range `is_compressed`, system usage outbox event `billing_outcome:"system_task"`, `settlement_method:"none"`, `actual_credits_micro:0`, no `user_id`; recorded request `stream:false`, `metadata.request_type=="summary"`), `next_turn_uses_summary` (stream_started has `thread_summary_applied.token_estimate`; provider input starts with preamble message; compressed messages not sent), `provider_failure_keeps_previous_summary_retry_then_reject` (Retry; attempt == max_attempts → Reject), `cas_conflict_is_ok`, `frontier_deleted_skips_commit`, `missing_summary_model_rejects`, `existing_summary_without_truncation_does_not_trigger`, `disabled_worker_no_trigger`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; commit `feat(mini-chat): thread summary`.

---

### Task 15: Orphan watchdog, upload reaper, leader elector, metrics instruments

Covers: S§10.3, S§10.4, S§13. AC: "Turn state machine … consistent end to end" (orphan path), "Every terminal outcome settles quota exactly once", "Cleanup and abandoned-upload recovery behave correctly under failure".

**Files:** `src/infra/workers/{leader.rs, orphan_watchdog.rs, upload_reaper.rs}`, `src/infra/metrics.rs`, `src/gear.rs` (spawn in start after outbox), `tests/workers.rs`.

**Interfaces:** `trait LeaderElector: Send + Sync { fn is_leader(&self, role: &str) -> bool }` (`NoopElector` always true; k8s variant only behind `k8s` feature, may be a stub that is always leader if kube APIs are too heavy — document in code); `OrphanWatchdog::scan_once(&self, now) -> ScanReport { detected, finalized }`; `UploadReaper::scan_once(&self, now) -> ReapReport`; `MiniChatMetrics` (OTel instruments; `fn noop()` for tests).

- [ ] **Step 1: Failing tests** `tests/workers.rs`: `orphan_finalizes_stale_running_turn` (turn with reserve, `last_progress_at` older than 300 s → `failed`/`orphan_timeout`, quota reserved released + estimated spent, usage event `aborted`/`estimated` with `selected_model == effective_model`, audit `turn_failed`); `recent_progress_not_orphaned`; `null_progress_falls_back_to_started_at`; `null_reserve_skips_settlement_but_enqueues_usage` (`actual_credits_micro 0`, `usage null`, models `""`, `policy_version_applied 0`); `cas_loser_noop` (already completed → nothing); `reaper_marks_abandoned_and_enqueues_cleanup` (`uploaded` row with provider file, stale → `failed`/`upload_abandoned`, `cleanup_status:"pending"`, outbox `attachment_upload_abandoned`); `reaper_pending_without_file_no_cleanup`; `reaper_skips_rows_with_cleanup_status`; `stuck_turn_unblocks_chat` (after watchdog, new send → 200).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; build; commit `feat(mini-chat): orphan watchdog, upload reaper, metrics`.

---

### Task 16: Remaining provider adapters, Anthropic secondary files, knowledge search

Covers: S§9.2, ADR-0005, D§4 "Knowledge Search".

**Files:** `src/infra/llm/providers/{openai_chat_completions.rs, vllm_responses.rs, anthropic_messages.rs}` (+tests), `src/infra/llm/storage/anthropic_files.rs`, knowledge retriever + agentic loop in stream service, `tests/adapters.rs`.

**Interfaces:** each adapter implements `ProviderAdapter` (Task 5); `ProviderResolver` returns the adapter by `ProviderKind`; `KnowledgeRetriever` port `async fn search(&self, query: &str, top_k: u32) -> Result<Vec<KnowledgeChunk>, DomainError>`.

- [ ] **Step 1: Failing unit tests:** chat completions — `body_uses_messages_and_max_completion_tokens`, `drops_builtin_tools_keeps_function`, `parses_deltas_usage_finish_length_incomplete`; vllm — `drops_all_tools_no_metadata`, `think_block_becomes_reasoning_delta`; anthropic — `body_system_messages_max_tokens_metadata_user_id`, `web_search_and_code_execution_mapped_file_search_dropped`, `parses_message_events_usage_and_max_tokens_incomplete`, `code_execution_tool_events_named_code_interpreter`; knowledge — `search_knowledge_loop_runs_retrieval_and_continues`, `limit_reached_output_after_max_calls`, `iteration_cap_is_agentic_iterations_exceeded`, `unknown_function_is_unexpected_tool_use`, `knowledge_off_when_file_search_included`; anthropic files — `image_secondary_copy_uploaded_for_anthropic_chat`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; commit `feat(mini-chat): additional adapters and knowledge search`.

---

### Task 17: Black-box E2E suite against the real server + acceptance traceability

Covers: S§14.3–14.4. AC: all (black-box confirmation).

**Files:** `testing/e2e/suites/mini_chat_dev/{conftest.py, mock_llm.py, server_config.yaml.tmpl, test_chats.py, test_streaming.py, test_attachments.py, test_quota.py, test_mutations.py, test_misc.py, README.md}`, `gears/mini-chat/mini-chat/tests/ACCEPTANCE_TRACE.md`.

**Interfaces:** `conftest.py` fixtures: `mock_llm` (threaded HTTP server implementing `POST /v1/responses` (SSE or JSON per `stream`), `/v1/files`, `/v1/vector_stores`, `/v1/vector_stores/{id}/files[/{fid}]`, DELETEs; records requests; scripting hooks via `mock_llm.script(...)`), `server` (renders the config with mock host/port, `use_http: true`, OAGW `ssrf_policy.enabled: false`, `allow_http_upstream` if required, api-gateway `auth_disabled: false` + static-authn `static_tokens` for users A, B (tenant T1) and C (tenant T2), sqlite DB in a temp dir; starts `./target/debug/cf-gears-example-server --config <file> run`, writes pid to `/tmp/mini_chat_dev_server.pid`, waits for `/mini-chat/v1/models`, kills by pid on teardown), `api(user)` helper, `sse(resp)` parser, `db()` sqlite connection to the gear DB file.

- [ ] **Step 1: Write the suite** with tests mirroring the hidden-suite surface: chat CRUD + Location + isolation across A/B/C; stream event order and payloads; replay & 409s; provider request contents (tools, user, metadata, max_output_tokens); attachments (document ready, image thumbnail, delete → `attachment_cleanup` outbox row + mock DELETE); quota downgrade/429 by inserting `quota_usage` rows into the sqlite DB; retry/edit/delete; reactions; models; quota status; chat delete cleanup (mock receives file + vector store DELETE).
- [ ] **Step 2: Build the server** (Global Constraints command) and **run** `python3 -m pytest testing/e2e/suites/mini_chat_dev -q` → fix gear defects until PASS (defects go in the owning module with a regression unit/integration test).
- [ ] **Step 3: Write `ACCEPTANCE_TRACE.md`** mapping every bullet of `gears/mini-chat/docs/acceptance-criteria.md` to ≥1 Rust test (file::name) and the E2E test(s); every bullet must have an entry.
- [ ] **Step 4: Run all** `cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat` and the E2E suite → PASS; commit `test(mini-chat): black-box e2e suite and acceptance trace`.
