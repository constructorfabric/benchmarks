# Mini-Chat Gear + SDK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the `mini-chat` ToolKit gear (`gears/mini-chat/mini-chat`) and its SDK crate (`gears/mini-chat/mini-chat-sdk`) so that every item of `gears/mini-chat/docs/acceptance-criteria.md` is implemented and tested, and the example server builds and serves the API.

**Architecture:** One gear crate layered api → domain → infra (spec §1) plus an SDK crate with plugin contracts and shared models. Domain services get infra through `Arc<dyn …>` ports and concrete SeaORM repositories, so an in-process harness (`mini_chat::testing`: SQLite in-memory + fake OAGW provider + mock PDP + in-process static policy plugin) runs the real services and router end to end. Background work uses the platform outbox (five leased queues) and two leader-elected workers.

**Tech Stack:** Rust 1.98 (edition 2024), toolkit (gear macro, OperationBuilder, canonical errors), toolkit-db (SeaORM 2, secure ORM, outbox), toolkit-odata, authz-resolver-sdk (PolicyEnforcer), authn-resolver-sdk, oagw-sdk (`ServiceGatewayClientV1`), axum SSE + multipart, `image` (WebP thumbnails), tokio.

**Spec:** `docs/superpowers/specs/2026-10-04-mini-chat-gear-design.md` (read it fully). It references `gears/mini-chat/docs/DESIGN.md` sections as `D§x`; those sections hold the exact behaviour and values and win over the spec. Companion files: `docs/superpowers/specs/2026-10-04-mini-chat-openapi-subset.json` (DTO shapes) and `docs/superpowers/specs/2026-10-04-mini-chat-platform-notes.md` (platform APIs).

## Global Constraints

- Packages: `cf-gears-mini-chat-sdk` (lib `mini_chat_sdk`, path `gears/mini-chat/mini-chat-sdk`) and `cf-gears-mini-chat` (lib `mini_chat`). The SDK is a workspace member and in `[workspace.dependencies]` as `mini-chat-sdk = { package = "cf-gears-mini-chat-sdk", path = "gears/mini-chat/mini-chat-sdk" }`.
- Offline only: every cargo command uses `--offline`; never add a crate that is not in `Cargo.lock`.
- Server build that must succeed: `cargo build --offline --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`.
- Keep `pub mod infra::plugins::{static_audit, static_model_policy}` importable from `mini_chat` (the example server imports them).
- Gear name `mini-chat`; plugin gear names `static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin`; gear deps `[types_registry, authn_resolver, authz_resolver, oagw]`.
- REST paths under config `url_prefix` (default `/mini-chat`) + `/v1/...`; operation ids `mini_chat.<op>` exactly as in the OpenAPI subset; license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` on every route.
- REST errors only through `toolkit_canonical_errors::CanonicalError` (RFC 9457 Problem); SSE errors as `{code, message}`.
- Every OAGW call (provisioning, chat, summary, files, vector stores) uses the gear's S2S `SecurityContext` from `client_credentials`.
- No provider identifier (`resp_…`, `file-…`, `vs_…`, `assistant-…`, `chatcmpl-…`, `msg_…`) in any API body, SSE payload or error text.
- Time = application UTC clock truncated to microseconds; UUIDs stored via SeaORM `Uuid` (16-byte BLOB on SQLite).
- Tests: unit tests in sibling `*_tests.rs` files (`#[cfg(test)] #[path = "x_tests.rs"] mod x_tests;`, file header `#![allow(clippy::unwrap_used, clippy::expect_used)]`); integration tests in `gears/mini-chat/mini-chat/tests/*.rs` using `mini_chat::testing`. Never put tests in `testing/e2e/suites/mini_chat/`.
- Commit after each task: `git -c user.name="Claude" -c user.email="noreply@anthropic.com" commit -m "<msg>"` (no push).
- When a value is needed that this plan does not state, take it from the D§ section the spec cites; do not invent alternatives.

## Review Focus

1. **Client disconnect at any moment** (before `stream_started`, mid-stream, during finalization): the turn must end `cancelled` (or its provider outcome) exactly once, quota settled once, no `running` turn left behind. Test owned by Task 14 (`stream_disconnect_mid_stream_cancels_turn_and_settles_once`) and Task 14 (`stream_disconnect_during_setup_still_finalizes`).
2. **Concurrent sends / mutations on one chat** (two requests racing): one wins, the other gets 409 `turn_already_running` / `GENERATION_IN_PROGRESS`, reserves never exceed limits. Tests in Task 14 (`concurrent_sends_one_wins_other_409`) and Task 16 (`concurrent_retries_one_wins_other_generation_in_progress`).
3. **Unicode / long text in titles and messages** (multi-byte characters, 255-char boundary, emoji in content estimation): validation counts characters, estimation counts UTF-8 bytes. Tests in Task 7 (`title_255_multibyte_chars_accepted_256_rejected`) and Task 4 (`text_tokens_count_utf8_bytes`).
4. **Malformed / hostile provider output** (garbage SSE lines, missing `type`, unknown events, error messages containing provider ids and URLs, stream ending without terminal event): never crash, never leak ids, terminal `error` sent. Tests in Task 9 (`unknown_and_malformed_events_are_ignored`, `stream_end_without_terminal_is_provider_error`) and Task 14 (`provider_error_message_is_sanitized_in_sse`).
5. **Policy plugin returning catalogs with disabled/removed models for existing chats**: send/upload on a removed model → 400 `INVALID_MODEL`; disabled → downgrade `model_disabled`; Models API hides both. Tests in Task 10 (`cascade_disabled_selected_model_downgrades_with_model_disabled`) and Task 14 (`send_on_removed_model_is_invalid_model`).

---

## File map (who creates what)

| Task | Main files |
|---|---|
| 1 | `mini-chat-sdk/**`, root `Cargo.toml`, `mini-chat/Cargo.toml` |
| 2 | `mini-chat/src/config.rs`, `config_tests.rs` |
| 3 | `src/infra/db/{mod.rs, entities/*.rs, migrations/*.rs}`, `migrations_tests.rs` |
| 4 | `src/domain/{credits.rs, estimation.rs, sanitize.rs, mime.rs, model.rs}` |
| 5 | `src/domain/error.rs`, `src/api/rest/error.rs` |
| 6 | `src/infra/plugins/{static_model_policy,static_audit}/*`, `src/infra/gateways/{model_policy.rs,audit.rs}` |
| 7 | `src/gear.rs`, `src/lib.rs`, `src/domain/{authz.rs, services/mod.rs, services/chat.rs, services/model_catalog.rs}`, `src/infra/db/repos/{chat.rs,…}`, `src/infra/outbox/{mod.rs,payloads.rs}`, `src/api/rest/{routes.rs, dto.rs, handlers/chats.rs}`, `src/testing/{mod.rs, harness.rs, authz.rs}`, `tests/chats_api.rs` |
| 8 | `services/{message.rs, reaction.rs}`, handlers `{messages.rs, reactions.rs, models.rs, quota.rs}`, `tests/read_apis.rs` |
| 9 | `src/infra/llm/{mod.rs, sse_reader.rs, openai_responses.rs, resolver.rs, client.rs}`, `src/testing/fake_provider.rs` |
| 10 | `src/domain/services/quota.rs`, `src/infra/db/repos/quota.rs`, `tests/quota.rs` |
| 11 | `src/domain/services/context.rs` |
| 12 | `src/infra/outbox/{usage.rs, audit.rs, pipeline.rs}`, `src/infra/db/repos/turn.rs` |
| 13 | `src/domain/services/finalization.rs` |
| 14 | `src/domain/services/stream/{mod.rs, preflight.rs, provider_task.rs, relay.rs, replay.rs}`, `src/api/rest/{sse.rs, handlers/stream.rs, handlers/turns.rs (status only)}`, `tests/stream_api.rs` |
| 15 | `src/infra/llm/storage.rs`, `src/infra/thumbnail.rs`, `src/domain/services/attachment.rs`, `src/infra/workers/background_indexing.rs`, `handlers/attachments.rs`, `tests/attachments_api.rs` |
| 16 | `src/domain/services/turn.rs`, `handlers/turns.rs`, `tests/turns_api.rs` |
| 17 | `src/domain/services/thread_summary.rs`, `src/infra/outbox/thread_summary.rs`, `tests/thread_summary.rs` |
| 18 | `src/infra/outbox/{attachment_cleanup.rs, chat_cleanup.rs}`, `tests/cleanup.rs` |
| 19 | `src/infra/workers/{leader.rs, orphan_watchdog.rs, upload_reaper.rs}`, `tests/workers.rs` |
| 20 | `src/infra/oagw/{provisioning.rs, s2s.rs}`, `src/gear.rs` (start/stop), `config/mini-chat.yaml` |
| 21 | `src/infra/llm/{openai_chat_completions.rs, vllm_responses.rs, anthropic_messages.rs, anthropic_files.rs}`, knowledge search |
| 22 | `gears/mini-chat/e2e-smoke/**`, `gears/mini-chat/docs/acceptance-tests.md` |

---

### Task 1: SDK crate and workspace wiring

**Files:**
- Create: `gears/mini-chat/mini-chat-sdk/{Cargo.toml, src/lib.rs, src/models.rs, src/usage.rs, src/audit.rs, src/plugin_api.rs, src/gts.rs, src/error.rs, src/models_tests.rs}`
- Modify: `Cargo.toml` (members + workspace dependency), `gears/mini-chat/mini-chat/Cargo.toml` (add `mini-chat-sdk = { workspace = true }`, `schemars`, `toolkit-odata-macros`, dev-deps `tower`, `tempfile`, `types-registry-sdk` with `test-util`)

**Interfaces:**
- Produces (all `pub`, re-exported at `mini_chat_sdk::*`): `ModelTier`, `ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `ModelApiParams`, `ModelFeatures`, `ModelToolSupport`, `ModelSupportedEndpoints`, `ModelPreference`, `KillSwitches`, `PolicySnapshot`, `PolicyVersionInfo`, `TierLimits`, `UserLimits`, `UserLicenseStatus`, `UsageEvent`, `UsageTokens`, `MiniChatAuditEvent` (+ `TurnAuditEvent`, `TurnMutationAuditEvent`, `AuditToolCalls`, `AuditPolicyDecisions`, `AuditQuotaDecision`), traits `MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`, errors `MiniChatModelPolicyPluginError`, `PublishError`, `MiniChatAuditPluginError`, GTS specs `MiniChatModelPolicyPluginSpecV1`, `MiniChatAuditPluginSpecV1`. Field lists and defaults: spec §2.
- `ModelCatalogEntry::has_capability(&self, cap: &str) -> bool`; `PolicySnapshot::find(&self, model_id: &str) -> Option<&ModelCatalogEntry>`; `MiniChatAuditEvent::tenant_id(&self) -> Uuid`.

- [ ] **Step 1: Write failing tests** in `models_tests.rs`:
  - `catalog_entry_minimal_json_applies_defaults` — deserialize an entry with only the required fields; assert `enabled == false`, `max_tool_calls == 2`, `web_search_context_size == "low"`, `estimation_budgets.bytes_per_token_conservative == 4`, `estimation_budgets.minimal_generation_floor == 50`, `preference.is_none()`.
  - `catalog_entry_missing_required_field_fails` — drop `max_num_results` → error.
  - `tier_accepts_capitalized_and_serializes_lowercase` — `"Premium"` and `"premium"` both parse; serializes `"premium"`.
  - `catalog_entry_ignores_unknown_keys` — extra key `"foo": 1` parses.
  - `kill_switches_require_every_field` — JSON missing `disable_images` fails.
  - `usage_event_system_task_omits_user_and_turn` — `user_id: None, turn_id: None, system_task_type: Some("thread_summary_update")` serializes without `user_id`/`turn_id` keys and with `"system_task_type"`; a user event with `usage: None` serializes `"usage": null` and no `system_task_type` key.
  - `audit_event_tagged_by_event_type` — `TurnMutationAuditEvent` retry serializes `"event_type":"turn_retry"` with `original_request_id`, `new_request_id`.
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat-sdk` → FAIL (crate missing).
- [ ] **Step 3: Implement** the crate (Cargo.toml modeled on `gears/system/tenant-resolver/tenant-resolver-sdk/Cargo.toml`; GTS specs with `#[gts_type_schema(... base = PluginV1, type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~") ...)]` and `…audit_plugin.v1~`); wire workspace + gear dependency.
- [ ] **Step 4: Run** `cargo test --offline -p cf-gears-mini-chat-sdk` → PASS; `cargo build --offline -p cf-gears-mini-chat` → OK.
- [ ] **Step 5: Commit** `feat(mini-chat-sdk): models, plugin contracts, usage and audit events`.

### Task 2: Gear configuration

**Files:** Create `gears/mini-chat/mini-chat/src/config.rs`, `src/config_tests.rs`; Modify `src/lib.rs` (`pub mod config;`).

**Interfaces:**
- Produces: `MiniChatConfig` with sub-structs named `StreamingConfig, EstimationBudgetsConfig, QuotaConfig, OutboxConfig, ContextConfig, RagConfig, ClientCredentialsConfig, MetricsConfig, ProviderEntry, ProviderKind {OpenaiResponses, OpenaiChatCompletions, VllmResponses, AnthropicMessages}` (serde snake_case), `StorageKind {Openai, Azure}`, `TenantOverride, ThumbnailConfig, KnowledgeSearchConfig, OrphanWatchdogConfig, UploadReaperConfig, ThreadSummaryWorkerConfig, CleanupWorkerConfig`; fields exactly spec §3.
- `MiniChatConfig::apply_defaults(&mut self)` — fills `upstream_alias` (entry host; override host), `storage_backend` (provider id), `port` (443/80).
- `MiniChatConfig::validate(&self) -> Result<(), String>`; `MiniChatConfig::deprecation_warnings(&self) -> Vec<String>`; `ThreadSummaryWorkerConfig::effective_model_id(&self) -> &str` (`""` → `"gpt-4.1-mini"`); `ProviderEntry::rag_provider_id(&self, own_id: &str) -> &str`.
- Constants `DEFAULT_WEB_SEARCH_GUARD` (exact sentence of D§4 "Web Search Configuration"), `DEFAULT_FILE_SEARCH_GUARD` (spec §9), `DEFAULT_KNOWLEDGE_SEARCH_GUARD`, `DEFAULT_SUMMARY_SYSTEM_PROMPT` (B.5.5 text).

- [ ] **Step 1: Failing tests** in `config_tests.rs`:
  - `defaults_are_valid_and_match_appendix_b` — `MiniChatConfig::default()` has `url_prefix == "/mini-chat"`, `streaming.sse_ping_interval_seconds == 15`, `streaming.sse_channel_capacity == 32`, `streaming.max_output_tokens == 32768`, `quota.overshoot_tolerance_factor == 1.10`, `quota.warning_threshold_pct == 80`, `quota.web_search_daily_quota == 75`, `outbox.num_partitions == 4`, `outbox.queue_name == "mini-chat.usage_snapshot"`, `rag.max_images_per_message == 4`, `orphan_watchdog.timeout_secs == 300`, `upload_reaper.stale_after_secs == 300`, `thread_summary_worker.max_attempts == 3`, `cleanup_worker.max_attempts == 5`; provider `openai` with `storage_kind == Openai`, `host == "api.openai.com"`; `validate()` ok once client_credentials are set.
  - `unknown_top_level_key_rejected` / `unknown_streaming_key_rejected` (`web_search_context_size`) / `removed_provider_key_rejected` (`supports_file_search_filters`) / `unknown_key_in_worker_sections_accepted` (`orphan_watchdog.foo`).
  - `range_validation` table test: ping 4 and 61, capacity 15 and 65, floor 0 and > max_output, overshoot 0.99 and 1.51, warning 0 and 100, num_partitions 3 and 128, orphan timeout 89, reaper stale 59, recent_messages_limit 101, claim_timeout 29 → each `Err`.
  - `azure_requires_valid_api_version`, `host_with_slash_rejected`, `rag_provider_must_exist`, `empty_client_credentials_rejected`.
  - `apply_defaults_fills_alias_backend_and_port` (use_http → 80).
  - `deprecated_cleanup_fields_warn_when_non_default`.
  - `parses_repo_dev_config_section` — parse the `gears.mini-chat.config` object from `config/mini-chat.yaml` (load YAML with `serde-saphyr` if it is a dev-dep, else embed the snippet) → validate ok after setting env-expanded host.
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat config` → FAIL.
- [ ] **Step 3: Implement** with `#[serde(default, deny_unknown_fields)]` per spec §3 (worker sections without `deny_unknown_fields`); `client_secret` as `secrecy::SecretString` (or redacted `Debug`). `${VAR}` expansion: use `toolkit_macros::ExpandVars` if it supports the fields; otherwise a `fn expand_env(&mut self)` applied in Task 7 init over `providers.*.host`, `auth_config` values, overrides, `client_credentials`.
- [ ] **Step 4: Run** tests → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): gear configuration and validation`.

### Task 3: Database schema, migrations and entities

**Files:** Create `src/infra/{mod.rs, db/mod.rs, db/entities/{mod.rs, chat.rs, message.rs, chat_turn.rs, attachment.rs, message_attachment.rs, thread_summary.rs, chat_vector_store.rs, quota_usage.rs, message_reaction.rs}, db/migrations/{mod.rs, m0001_initial.rs}, db/migrations_tests.rs}`.

**Interfaces:**
- Produces: `infra::db::migrations::Migrator` (`MigratorTrait`), `infra::db::OUTBOX_TABLE_PREFIX: &str = "mini_chat_outbox"`, `infra::db::all_migrations() -> Vec<Box<dyn MigrationTrait>>` (gear migrations + outbox migrations with the prefix).
- Entities `infra::db::entities::{chat, message, chat_turn, attachment, message_attachment, thread_summary, chat_vector_store, quota_usage, message_reaction}` each with `Entity, Model, ActiveModel, Column`, field names = column names of D§3.7, `Scopable` per spec §4. String enums stay `String` columns; domain enums in Task 4 convert.
- Test helper `infra::db::test_db() -> toolkit_db::Db`-ish async fn (`#[doc(hidden)] pub`) creating an in-memory SQLite (`sqlite:file:mc_{uuid}?mode=memory&cache=shared`, max_conns 1) with `all_migrations()` applied — used by later tasks and by `mini_chat::testing`.

- [ ] **Step 1: Failing tests** `migrations_tests.rs`:
  - `sqlite_migrations_create_all_tables` — tables `chats, messages, chat_turns, attachments, message_attachments, thread_summaries, chat_vector_stores, quota_usage, message_reactions` and an outbox table with prefix `mini_chat_outbox` exist; no table name contains `mcp`.
  - `columns_match_design` — `PRAGMA table_info` of each table equals the D§3.7 column list (assert the full set per table).
  - `one_running_turn_per_chat_index` — inserting two `state='running'` turns for one chat fails; a second running turn after the first has `deleted_at` set succeeds.
  - `turn_request_id_unique_per_chat`, `message_request_role_unique_for_live_rows` (live dup fails; dup with `deleted_at` set ok), `quota_usage_bucket_unique`, `reaction_unique_per_user_message`, `vector_store_unique_per_chat`.
  - `check_constraints_enforced` — `chat_turns.state='bogus'` fails; `attachments.attachment_kind='video'` fails; `message_reactions.reaction='love'` fails; `attachments.cleanup_status='weird'` succeeds (not enforced).
  - `message_attachment_composite_fk_rejects_cross_chat` (with `PRAGMA foreign_keys=ON` if the platform enables it; otherwise assert the FK is declared via `pragma foreign_key_list`).
  - `entities_roundtrip` — insert and read back one row per entity via SeaORM (UUID and timestamps equal).
  - `postgres_sql_variant_is_provided` — `Migration` returns non-empty SQL for `DatabaseBackend::Postgres` (unit-test the SQL builder function `pg_statements()`; contains `TIMESTAMPTZ`, `UUID`, `JSONB`, `BYTEA`, `WHERE state = 'running' AND deleted_at IS NULL`).
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat migrations` → FAIL.
- [ ] **Step 3: Implement** raw-SQL migration per backend (credstore `m0001_initial_schema.rs` pattern), spec §4 types/defaults/indexes.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): database schema, migrations and entities`.

### Task 4: Pure domain helpers (enums, credits, estimation, sanitization, MIME)

**Files:** Create `src/domain/{mod.rs, model.rs, credits.rs, estimation.rs, sanitize.rs, mime.rs}` + `*_tests.rs` for the last four.

**Interfaces (all `pub`):**
- `domain::model`: `TurnState {Running, Completed, Failed, Cancelled}` (`as_str`/`parse`: `running|completed|failed|cancelled`; `api_state()` → `running|done|error|cancelled`), `MessageRole {User, Assistant, System}`, `AttachmentKind {Document, Image}`, `AttachmentStatus {Pending, Uploaded, Ready, Failed}`, `QuotaDecision {Allow, Downgrade}`, `DowngradeReason {PremiumQuotaExhausted, ForceStandardTier, DisablePremiumTier, ModelDisabled}` (snake_case strings), `BillingOutcome {Completed, Failed, Aborted, SystemTask}`, `SettlementMethod {Actual, Estimated, Released, None}`, `QuotaScope {Tokens, WebSearch, CodeInterpreter}` (`tokens|web_search|code_interpreter`), `PeriodType {Daily, Monthly}`, `Bucket {Total, TierPremium}` (`total|tier:premium`), module `error_codes` with consts `PROVIDER_ERROR, PROVIDER_TIMEOUT, RATE_LIMITED, WEB_SEARCH_CALLS_EXCEEDED, CODE_INTERPRETER_CALLS_EXCEEDED, AGENTIC_ITERATIONS_EXCEEDED, UNEXPECTED_TOOL_USE, MESSAGE_PERSISTENCE_FAILED, FINALIZATION_FAILED, STREAM_INTERRUPTED, ORPHAN_TIMEOUT, CONTEXT_LENGTH_EXCEEDED, TURN_SETUP_FAILED, QUOTA_EXCEEDED, INDEXING_FAILED, UPLOAD_FAILED, UPLOAD_ABANDONED`.
- `credits::credits_micro(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> Result<i64, CreditsError>` (D§5.3 checked rules; `CreditsError {InvalidTokenCount, ZeroMultiplier, MultiplierTooLarge, Overflow}`).
- `estimation::estimated_text_tokens(text: &str, b: &EstimationBudgets) -> i64`; `estimation::item_tokens(text: &str, b) -> i64` (same formula, used by context assembly); `estimation::period_starts(now: OffsetDateTime) -> (Date, Date)` (daily, monthly); `estimation::next_reset(period: PeriodType, now) -> OffsetDateTime`.
- `sanitize::sanitize_provider_message(msg: &str) -> String` (spec §11.4); `sanitize::user_field(tenant: &str, user: &str) -> String` (64-char simple hex concat, fallback `"{t}:{u}"`).
- `mime::resolve_content_type(part_ct: Option<&str>, filename: &str, allow_csv: bool) -> Result<String, MimeError>`; `mime::attachment_kind(ct: &str) -> AttachmentKind`; `mime::purposes(ct: &str) -> (bool /*file_search*/, bool /*code_interpreter*/)`; `mime::normalize_filename(name: Option<&str>) -> String` (default `upload`, ≤255 chars keeping extension); `mime::extension_for(ct: &str) -> &'static str`; `const XLSX_MIME: &str`.

- [ ] **Step 1: Failing tests:**
  - credits: `per_component_ceil` (`credits_micro(1, 1, 1, 1) == Ok(2)`), `design_example` (`credits_micro(1000, 500, 2_500_000_000, 2_500_000_000) == Ok(3_750_000)`), `zero_multiplier_rejected`, `multiplier_above_1e10_rejected`, `tokens_above_10m_rejected`.
  - estimation: `text_tokens_formula` (100 ASCII bytes, bpt 4, overhead 100, margin 10 → `ceil((25+100)*110/100) == 138`), `empty_text_is_overhead_with_margin` (110), `text_tokens_count_utf8_bytes` ("é"×10 = 20 bytes), `zero_bpt_clamped_to_one`, `period_starts_utc` (2026-02-28T23:59:59Z → daily 2026-02-28, monthly 2026-02-01), `next_reset_daily_and_monthly`.
  - sanitize: `replaces_response_ids` (`"bad resp_abc123 x"` → `"bad [provider_id] x"`), `file_ids_need_12_chars` (`file-abc123def456` replaced; `file-based` and `file_search` kept), `vs_and_assistant_ids`, `urls_replaced`, `sk_keys_and_bearer_tokens_replaced`, `user_field_is_64_hex`.
  - mime: `octet_stream_inferred_from_extension` (`report.pdf` → `application/pdf`), `unknown_extension_rejected`, `csv_remapped_to_text_plain_when_allowed`, `csv_rejected_when_disallowed`, `xlsx_is_code_interpreter_only`, `images_have_no_purpose`, `filename_default_and_truncation_keeps_extension` (300-char name `.pdf` → 255 chars ending `.pdf`).
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat domain::` → FAIL.
- [ ] **Step 3: Implement.** MIME allowlist = spec §12 list (map extensions: pdf, docx, pptx, xlsx, txt, md, html/htm, json, py, java, js, ts, rs, go, cs, rb, sql, csv, png, jpg/jpeg, webp, gif).
- [ ] **Step 4: Run** → PASS. **Step 5: Commit** `feat(mini-chat): domain enums, credit arithmetic, estimation, sanitization, MIME rules`.

### Task 5: Domain error and canonical error mapping

**Files:** Create `src/domain/error.rs`, `src/api/{mod.rs, rest/mod.rs, rest/error.rs, rest/error_tests.rs}`.

**Interfaces:**
- Produces `domain::error::DomainError` (thiserror) with exactly these variants: `ChatNotFound, MessageNotFound, TurnNotFound, AttachmentNotFound, ModelNotFound, InvalidModel, EmptyContent, InvalidTitle, InvalidReaction, InvalidAttachment(String), TooManyImages { max: u32 }, InputTooLong, ContextBudgetExceeded, VisionNotSupported, FileTooLarge { limit_bytes: u64 }, UnsupportedContentType(String), CodeInterpreterUnavailable, Multipart { field: &'static str, reason: &'static str, detail: String }, FeatureDisabled { subject: &'static str }, TurnNotTerminal, ReactionTargetNotAssistant, PermissionDenied, AuthzUnavailable, TurnAlreadyRunning, RequestIdConflict, NotLatestTurn, GenerationInProgress, Replay, AttachmentLocked, ProviderMismatch, Conflict { code: String }, QuotaExceeded { scope: QuotaScope }, DocumentLimit, StorageLimit, StorageUnavailable(String), UploadConcurrencyLimit, OutboxPayloadTooLarge(String), Internal(String)`; `impl From<toolkit_db::DbError> for DomainError` (→ `Internal`, unique violation → `Conflict{code:"unique_violation"}`); `type DomainResult<T> = Result<T, DomainError>`.
- Produces `impl From<DomainError> for CanonicalError` in `api/rest/error.rs`, plus resource markers `ChatResource, MessageResource, TurnResource, AttachmentResource, ModelResource` (`gts_id!("cf.core.mini_chat.chat.v1~")` etc.).

- [ ] **Step 1: Failing tests** `error_tests.rs`, one test per mapping row of D§3.3 "Error Codes" (render each `CanonicalError` to a `Problem` / JSON and assert status + machine field). Examples that pin the shapes:
  - `chat_not_found_is_404_with_chat_resource_type` → status 404, `context.resource_type == "gts.cf.core.mini_chat.chat.v1~"`.
  - `invalid_title` → 400, `context.field_violations[0] == {field:"title", reason:"INVALID_TITLE"}`.
  - `empty_content` (`content`/`EMPTY_CONTENT`), `invalid_model` (`model`/`INVALID_MODEL`), `invalid_reaction` (`reaction`/`INVALID_REACTION`), `invalid_attachment` (`attachment`/`invalid_attachment`), `too_many_images` (400 out_of_range, `image_count`/`TOO_MANY_IMAGES`), `input_too_long` (`content`/`INPUT_TOO_LONG`), `context_budget` (`content`/`CONTEXT_BUDGET_EXCEEDED`), `vision` (`content_type`/`VISION_NOT_SUPPORTED`), `file_too_large` (400, `content_length`/`FILE_TOO_LARGE`), `unsupported_ct`, `code_interpreter_unavailable` (`file`, attachment resource type), `multipart_reasons` (4 cases).
  - `feature_disabled_is_failed_precondition` → 400, `context.violations[0] == {subject:"web_search", type:"FEATURE_DISABLED", ...}`; `turn_not_terminal` (`turn_state`/`STATE`); `reaction_target` (`reaction_target`/`STATE`).
  - `permission_denied_reason` → 403 `context.reason == "AUTHZ_DENIED"`; `authz_unavailable_is_503_retry_5` → 503, header/`context.retry_after_seconds == 5`.
  - `aborted_reasons` → 409 with `context.reason` `turn_already_running`, `request_id_conflict`, `NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`, `REPLAY`.
  - `already_exists_resource_names` → 409 `resource_name` `attachment_locked`, `provider_mismatch`, `unique_violation`.
  - `quota_exceeded_scopes` → 429 `context.violations[0] == {subject:"web_search", description:"quota_exceeded"}`; `document_and_storage_limit` → 429 subjects `document_limit` / `storage_limit`.
  - `storage_unavailable_503_retry_10`, `upload_concurrency_503_retry_5`, `outbox_payload_too_large_is_400_with_format`, `internal_hides_detail` (detail does not contain the inner message).
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat rest::error` → FAIL.
- [ ] **Step 3: Implement** with the `#[resource_error]` builders (platform notes §4.1).
- [ ] **Step 4: Run** → PASS. **Step 5: Commit** `feat(mini-chat): domain error and canonical error contract`.

### Task 6: Bundled plugins and plugin gateways

**Files:** Create `src/infra/plugins/{mod.rs, static_model_policy/{mod.rs, config.rs, service.rs, service_tests.rs}, static_audit/{mod.rs, config.rs, service.rs, service_tests.rs}}`, `src/infra/gateways/{mod.rs, model_policy.rs, audit.rs, gateways_tests.rs}`. Remove the empty module stubs from `lib.rs` (keep the same public paths).

**Interfaces:**
- Plugin gears (`#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]` / `"static-mini-chat-audit-plugin"`), configs per spec §3 "Plugin configs".
- `StaticModelPolicyService::from_config(cfg: &StaticModelPolicyConfig) -> anyhow::Result<Self>` implementing `MiniChatModelPolicyPluginClientV1` (policy version 1, `generated_at` = construction time).
- `StaticAuditService` implementing `MiniChatAuditPluginClientV1`.
- `infra::gateways::model_policy::ModelPolicyGateway` trait (domain-facing port, `#[async_trait]`): `async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<Arc<PolicySnapshot>>`, `async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<Arc<PolicySnapshot>>`, `async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits>`, `async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome>` where `PublishOutcome { Retry(String), Reject(String) }`. Impl `PluginModelPolicyGateway::new(hub: Arc<ClientHub>, vendor: String)` (lazy `GtsPluginSelector`); plugin errors → `DomainError::Internal`.
- `infra::gateways::audit::AuditGateway` trait: `async fn emit(&self, ev: MiniChatAuditEvent) -> AuditEmitResult` with `AuditEmitResult { Delivered, Dropped, Retry(String), Reject(String) }`. Impl `PluginAuditGateway::new(hub, vendor)`: no instance → `Dropped` (not cached; warn throttled); instance without client → `Retry` + selector reset; plugin `Transient|PluginTimeout` → `Retry`; `Permanent` → `Reject`; 30 s timeout → `Retry`.
- `infra::gateways::model_policy::InProcessModelPolicyGateway::new(Arc<dyn MiniChatModelPolicyPluginClientV1>)` — used by `mini_chat::testing` (bypasses types-registry).

- [ ] **Step 1: Failing tests:**
  - `static_policy_rejects_zero_multiplier` / `…_multiplier_over_1e10` / `…_zero_bytes_per_token`; `static_policy_unknown_top_level_key_rejected`; `static_kill_switches_default_false_and_unknown_switch_rejected`.
  - `static_policy_serves_version_1_and_catalog_in_order`; `static_policy_default_limits` (standard daily 100_000_000 / monthly 1_000_000_000; premium 50_000_000 / 500_000_000).
  - `static_audit_disabled_still_ok`.
  - Gateway tests with `types_registry_sdk::testing::MockTypesRegistryClient` + `ClientHub`: `policy_gateway_picks_lowest_priority_instance_of_vendor`, `audit_gateway_drops_when_no_plugin_and_finds_late_registered_plugin`, `audit_gateway_retries_when_client_missing`, `audit_gateway_maps_plugin_errors` (Transient→Retry, Permanent→Reject, PluginTimeout→Retry).
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat plugins gateways` → FAIL.
- [ ] **Step 3: Implement** following `static-credstore-plugin/src/gear.rs` and `credstore/src/infra/plugin_select.rs`.
- [ ] **Step 4: Run** → PASS; `cargo build --offline --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore` → OK.
- [ ] **Step 5: Commit** `feat(mini-chat): static model-policy and audit plugins with lazy gateways`.

### Task 7: Gear skeleton, authz, outbox enqueuer, test harness, chats CRUD

**Files:** Create `src/gear.rs`, `src/domain/{authz.rs, services/mod.rs, services/chat.rs, services/model_catalog.rs}`, `src/infra/db/repos/{mod.rs, chat.rs, message.rs (count only for now)}`, `src/infra/outbox/{mod.rs, payloads.rs, enqueuer.rs, pipeline.rs}`, `src/api/rest/{routes.rs, dto.rs, handlers/{mod.rs, chats.rs}}`, `src/testing/{mod.rs, harness.rs, authz.rs, users.rs}`, `tests/chats_api.rs`; Modify `src/lib.rs`.

**Interfaces:**
- `domain::authz::ChatAuthz::new(enforcer: PolicyEnforcer)`; `async fn chat_scope(&self, ctx: &SecurityContext, action: &'static str, chat_id: Option<Uuid>) -> DomainResult<AccessScope>` (owner props, `require_constraints`, `ensure_owner(ctx.subject_id())`, error mapping spec §5); `async fn model_permission(&self, ctx, action) -> DomainResult<()>`; `async fn quota_scope(&self, ctx) -> DomainResult<AccessScope>`. Action consts in `domain::authz::actions` (spec §5 list).
- `domain::services::ServiceDeps { config: Arc<MiniChatConfig>, db: Arc<DBProvider<DomainError>>, authz: Arc<ChatAuthz>, policy: Arc<dyn ModelPolicyGateway>, outbox: Arc<OutboxEnqueuer>, llm: Arc<LlmClient> /* Task 9; use Option until then */, rag: … /* Task 15 */ }` and `domain::services::Services { chats: Arc<ChatService>, models: Arc<ModelCatalogService>, … }` built by `pub fn build_services(deps: ServiceDeps) -> Services` (later tasks add fields; keep construction in this one function).
- `ModelCatalogService`: `async fn resolve_chat_model(&self, user_id: Uuid, model_id: &str) -> DomainResult<ResolvedModel>` (no enabled filter; missing → `InvalidModel`), `async fn resolve_for_create(&self, user_id, requested: Option<&str>) -> DomainResult<ResolvedModel>` (enabled; default rule D§2.2), `async fn list_visible(&self, user_id) -> DomainResult<Vec<ModelCatalogEntry>>`, `async fn get_visible(&self, user_id, id) -> DomainResult<ModelCatalogEntry>` (`ModelNotFound`). `ResolvedModel { entry: ModelCatalogEntry, snapshot: Arc<PolicySnapshot> }`.
- `ChatService`: `create(ctx, title: Option<String>, model: Option<String>) -> DomainResult<ChatView>`, `get(ctx, id)`, `list(ctx, query: &ODataQuery) -> Result<Page<ChatView>, ListError>` (`ListError::{OData(toolkit_odata::Error), Domain(DomainError)}`), `update_title(ctx, id, title: String)`, `delete(ctx, id) -> DomainResult<()>`; `async fn load_chat(&self, scope: &AccessScope, id: Uuid) -> DomainResult<chat::Model>` (shared by later services). `ChatView { chat: chat::Model, message_count: i64 }`.
- `infra::outbox::OutboxEnqueuer` (holds `OnceLock<Arc<Outbox>>` + `OutboxConfig`): `fn set_outbox(&self, Arc<Outbox>)`; `async fn enqueue_json<T: Serialize>(&self, tx: &(impl DBRunner + Sync), queue: QueueKind, partition_key: Uuid, payload_type: &str, payload: &T) -> DomainResult<Wake>`; `QueueKind { Usage, AttachmentCleanup, ChatCleanup, ThreadSummary, Audit }` → configured names; `fn partition_for(key: Uuid, partitions: u32) -> u32` (first 4 bytes BE mod n); payload > 64 KiB → `OutboxPayloadTooLarge`; not started → `Internal("outbox not started")`. `PendingWakes { push(Wake), fire() }`.
- `infra::outbox::payloads`: `ChatCleanupPayload { tenant_id, chat_id, system_request_id, reason: "chat_soft_delete", chat_deleted_at }`, `AttachmentCleanupPayload` (D§4 Phase 1 table fields; `event_type` ∈ `attachment_deleted|attachment_upload_abandoned|attachment_indexing_failed`), `ThreadSummaryPayload` (spec §14 fields).
- `infra::outbox::pipeline::start_outbox(db: Db, cfg: &MiniChatConfig, handlers: OutboxHandlers) -> anyhow::Result<OutboxHandle>` where `OutboxHandlers { usage, attachment_cleanup, chat_cleanup, thread_summary, audit: Arc<dyn LeasedMessageHandler> }`; until their tasks exist, register `AckAllHandler` placeholders for the queues.
- `api::rest::routes::register_routes(router: Router, openapi: &dyn OpenApiRegistry, services: Services, cfg: &MiniChatConfig) -> Router`.
- `gear.rs`: `MiniChatGear` with `init` (config load → `apply_defaults` → `validate` → warnings; DB; ClientHub clients; gateways; services) and `register_rest`; start/stop wiring finished in Task 20 (for now `start` starts the outbox pipeline).
- `mini_chat::testing` (`#[doc(hidden)] pub mod testing`, always compiled): `TestUser { tenant_id: Uuid, user_id: Uuid }` with consts `TestUser::A1`, `TestUser::A2` (same tenant, other user), `TestUser::B1` (other tenant); `MockPdp` implementing `AuthZResolverApi` (static-plugin behaviour: `In(owner_tenant_id,[tid])`; modes `Allow`, `Deny`, `Fail`); `TestApp::builder()` with `.catalog(Vec<ModelCatalogEntry>)`, `.kill_switches(KillSwitches)`, `.limits(TierLimits standard, TierLimits premium)`, `.config(impl FnOnce(&mut MiniChatConfig))`, `.pdp(PdpMode)`, `.build().await -> TestApp`; `TestApp` fields `db`, `services`, `router`, `outbox: OutboxHandle`; methods `async fn call(&self, user: TestUser, method: Method, path: &str, body: Option<Value>) -> TestResponse { status, headers, json: Value }`, `async fn raw(&self, req: Request<Body>) -> Response<Body>` (inserts `SecurityContext` extension built from a `TestUser`); fixtures `testing::catalog::{premium_model(id), standard_model(id), default_catalog()}` (`default_catalog`: `gpt-premium` premium default VISION_INPUT all tools, `gpt-standard` standard, both provider `openai`, multipliers 1_000_000/3_000_000, context_window 128000, max_output 4096, max_input 0).

- [ ] **Step 1: Failing tests** `tests/chats_api.rs`:
  - `create_chat_defaults_model_and_returns_201_location` — POST `/mini-chat/v1/chats` `{}` → 201, `model == "gpt-premium"`, `Location == "/mini-chat/v1/chats/{id}"`, `message_count == 0`, `is_temporary == false`, no `title` key, `created_at == updated_at`.
  - `create_chat_with_title_trims`; `title_255_multibyte_chars_accepted_256_rejected` (255×"é" → 201; 256 → 400 `INVALID_TITLE`); `whitespace_title_rejected`; `invalid_title_checked_before_model` (bad title + unknown model → `INVALID_TITLE`).
  - `create_chat_unknown_or_disabled_model_is_invalid_model`; `default_falls_back_to_first_enabled_when_no_is_default`.
  - `get_chat_foreign_user_and_tenant_are_404` (A2 and B1 get 404 on A1's chat); `get_deleted_chat_404`.
  - `patch_title_updates_updated_at_and_ignores_model` (`{"title":"X","model":"gpt-standard"}` → 200, model unchanged, `updated_at` > before); `patch_missing_title_is_422`; `patch_null_title_is_422`; `malformed_json_is_400_json_syntax_error`; `missing_content_type_is_415`; `non_uuid_path_is_400_invalid_path_params`.
  - `delete_chat_204_then_404_and_enqueues_chat_cleanup` (DB: `chats.deleted_at` set; one outbox row in the chat-cleanup queue whose payload has `reason == "chat_soft_delete"`).
  - `list_chats_orders_by_updated_at_desc_and_paginates` (create 3, rename the first → it is first; `limit=2` → 2 items + `next_cursor`; following the cursor returns the third; `limit=500` → `page_info.limit == 100`; `limit=0` → 400 `INVALID_LIMIT` with `resource_type == "gts.cf.core.odata.query.v1~"`).
  - `list_chats_filter_and_orderby` (`$filter=title eq 'b'`, `$orderby=title asc`; unknown field → 400 `INVALID_FILTER` / `INVALID_ORDERBY_FIELD`; bad cursor → `INVALID_CURSOR`; `$skip=1` → `UNSUPPORTED_QUERY_PARAM`; `$select=id` accepted, full items returned).
  - `list_chats_only_own_and_not_deleted`.
  - `pdp_deny_is_403_and_pdp_failure_is_503` (harness `.pdp(Deny)` / `.pdp(Fail)`).
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat --test chats_api` → FAIL.
- [ ] **Step 3: Implement** (chat list via `OPager`/`paginate_with_odata` with `FieldMap` extractors for every orderable field; `title` filter supports contains/startswith/endswith; DTOs from the OpenAPI subset; handlers with `extract::Json`, `extract::Path`, `OData`; create route with the throttling spec of spec §6; the harness mirrors `gear.rs` wiring via `build_services`).
- [ ] **Step 4: Run** → PASS; server build command → OK.
- [ ] **Step 5: Commit** `feat(mini-chat): gear skeleton, PEP, outbox enqueuer, harness and chats CRUD`.

### Task 8: Messages list, reactions, models API, quota status endpoint

**Files:** Create `src/domain/services/{message.rs, reaction.rs, quota_status.rs}`, `src/infra/db/repos/{message.rs (extend), reaction.rs, attachment.rs (summary query), quota.rs (read part)}`, handlers `{messages.rs, reactions.rs, models.rs, quota.rs}`, `tests/read_apis.rs`; `src/testing/seed.rs`.

**Interfaces:**
- `MessageService::list(ctx, chat_id, query) -> Result<Page<MessageView>, ListError>`; `MessageView { message: message::Model, attachments: Vec<AttachmentSummary>, my_reaction: Option<String> }` (batch queries per page; null `request_id` → `Internal`).
- `ReactionService::set(ctx, chat_id, msg_id, reaction: &str) -> DomainResult<message_reaction::Model>`, `remove(ctx, chat_id, msg_id) -> DomainResult<()>`.
- `QuotaStatusService::status(ctx) -> DomainResult<QuotaStatus>` and `pub fn compute_tier_status(limits: &UserLimits, rows: &[quota_usage::Model], now: OffsetDateTime, warning_threshold_pct: u8) -> Vec<TierStatus>` (shared with `done.quota_warnings`); `TierStatus { tier: "premium"|"total", periods: Vec<PeriodStatus{period, limit, used, remaining, remaining_percentage, next_reset, warning, exhausted}> }`.
- `repos::quota::QuotaRepo::rows_for_periods(conn, tenant_id, user_id, daily: Date, monthly: Date) -> DomainResult<Vec<quota_usage::Model>>`.
- `testing::seed::{insert_message(db, chat_id, role, content, request_id, created_at) -> Uuid, insert_quota_row(db, …), insert_attachment(db, …)}` for direct DB seeding in tests.

- [ ] **Step 1: Failing tests** `tests/read_apis.rs`:
  - `messages_chronological_with_required_fields` — seed user/assistant messages → items in `created_at asc`; each item has `request_id`, `attachments: []`, `my_reaction: null`; assistant has `model`, `input_tokens`; user has no `model` key; zero tokens omitted.
  - `messages_pagination_filter_orderby` (`$filter=role eq 'assistant'`, `$orderby=created_at desc`, cursor), `messages_exclude_soft_deleted`, `messages_of_foreign_chat_404`.
  - `message_attachments_listed_without_deleted` (seed links; deleted attachment absent; image ready shows `img_thumbnail`).
  - `reaction_put_upserts_and_shows_in_list` (like → 200 `{message_id, reaction:"like", created_at}`; dislike replaces; list shows `my_reaction:"dislike"` for A1 only).
  - `reaction_on_user_message_400_reaction_target` (PUT and DELETE); `reaction_invalid_value_400_before_authz` (with `.pdp(Deny)` still 400); `reaction_missing_field_422`; `reaction_delete_idempotent_204`; `reaction_unknown_message_404_message`.
  - `models_list_only_enabled_without_internal_fields` (keys exactly `model_id, display_name, tier, multiplier_display, description?, multimodal_capabilities, context_window`; tier lowercase); `get_disabled_model_404_model_resource`.
  - `quota_status_reports_tiers_periods` — seed `total` daily spent 80_000_000 reserved 0 with limit 100_000_000 → `used 80_000_000`, `remaining_percentage 20`, `warning true`, `exhausted false`, `next_reset` = next UTC midnight; `tier:premium` absent rows → used 0, remaining 100; `warning_threshold_pct == 80`; limit 0 period omitted.
  - `quota_status_exhausted_below_one_percent` (remaining 0.5% → `remaining_percentage 0`, `exhausted true`).
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat --test read_apis` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): messages list, reactions, models and quota status APIs`.

### Task 9: LLM layer — OpenAI Responses adapter, resolver, client, fake provider

**Files:** Create `src/infra/llm/{mod.rs, types.rs, sse_reader.rs, openai_responses.rs, openai_responses_tests.rs, resolver.rs, resolver_tests.rs, client.rs}`, `src/infra/oagw/{mod.rs, s2s.rs}`, `src/testing/fake_provider.rs`.

**Interfaces:**
- `infra::llm::types`: `LlmRequest { model: String /*provider_model_id*/, instructions: String, input: Vec<LlmMessage>, max_output_tokens: u32, tools: Vec<LlmTool>, max_tool_calls: Option<u32>, api_params: ModelApiParams, user: String, metadata: RequestMetadata, stream: bool }`; `LlmMessage { role: MessageRole, text: String, image_file_ids: Vec<String> }`; `LlmTool { FileSearch{vector_store_id, max_num_results}, WebSearch{context_size}, CodeInterpreter{file_ids}, SearchKnowledge }`; `RequestMetadata { tenant_id, user_id, chat_id, request_type: "chat"|"summary", feature }`; `LlmEvent { TextDelta(String), ReasoningDelta(String), ToolStart{name: String, details: Value}, ToolDone{name: String, details: Value}, Citation(RawCitation), Completed{usage: Option<LlmUsage>, response_id: Option<String>, incomplete_reason: Option<String>}, Failed{error: LlmError, usage: Option<LlmUsage>} }`; `RawCitation { Web{url, title, snippet, span: Option<(u32,u32)>}, File{provider_file_id, span: Option<(u32,u32)>} }`; `LlmUsage { input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_tokens: i64 }`; `LlmError { RateLimited{retry_after_secs: Option<u64>, message: String}, Timeout(String), Provider{message: String}, Unavailable(String) }` with `fn sse_code(&self) -> &'static str` and `fn client_message(&self) -> String` (sanitized; rate limited adds `retry in {N}s`).
- `trait ProviderAdapter: Send + Sync { fn build_body(&self, req: &LlmRequest) -> Value; fn parse_event(&self, st: &mut ParseState, event: &str, data: &str) -> Vec<LlmEvent>; fn parse_completion(&self, body: &[u8]) -> Result<CompletionResult, LlmError>; }`; `CompletionResult { text: String, usage: Option<LlmUsage> }`; `OpenAiResponsesAdapter`.
- `resolver::ProviderResolver::new(cfg: &MiniChatConfig)`; `fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> DomainResult<ResolvedProvider>`; `fn resolve_rag(&self, provider_id, tenant_id) -> DomainResult<ResolvedProvider>`; `ResolvedProvider { provider_id, kind: ProviderKind, alias: String, api_path: String, storage_kind: Option<StorageKind>, storage_backend: String, api_version: Option<String> }`; `ResolvedProvider::chat_uri(&self, provider_model_id: &str) -> String` (`/{alias}{api_path with {model} replaced}`).
- `infra::oagw::s2s::S2sContext` (`RwLock<Option<SecurityContext>>`): `fn set(&self, ctx)`, `fn get(&self) -> DomainResult<SecurityContext>`.
- `client::LlmClient::new(gw: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>)`; `async fn stream(&self, p: &ResolvedProvider, req: &LlmRequest, cancel: CancellationToken) -> Result<BoxStream<'static, LlmEvent>, LlmError>` (pre-stream HTTP errors → `Err`; mid-stream failures and missing terminal → final `Failed` event); `async fn complete(&self, p, req) -> Result<CompletionResult, LlmError>`.
- `testing::fake_provider::FakeProvider` implementing `ServiceGatewayClientV1` (only `proxy_request` + no-op upstream/route methods returning stub objects): `FakeProvider::new() -> Arc<Self>`; `fn push_stream(&self, s: ScriptedStream)`; `ScriptedStream { events: Vec<(String, Value)>, delay_between: Duration, hold_after: Option<usize> /*pause before event n until release()*/, http_error: Option<(u16, Value, Option<u64>)> }`, helpers `ScriptedStream::text(chunks: &[&str], input_tokens: i64, output_tokens: i64)`, `::failed(message: &str)`, `::http(status, body)`, `::no_terminal(chunks)`; `fn release(&self)`; `fn requests(&self) -> Vec<RecordedRequest { method, path, query: Option<String>, body: Bytes, json: Option<Value> }>`; `fn chat_requests(&self) -> Vec<Value>`; files/vector-store emulation used by Task 15 (`fn set_vector_store_file_statuses(&self, Vec<&str>)`, `fn fail_next(&self, path_prefix: &str, status: u16)`), summary (`push_completion(text, usage)`). Default (empty queue) stream = `text(["Hello"], 10, 5)`.

- [ ] **Step 1: Failing tests** `openai_responses_tests.rs`:
  - `body_has_wire_fields` — assert JSON equality on: `model`, `instructions`, `stream: true`, `store: false`, `max_output_tokens`, `user` (64 chars), `metadata == {tenant_id, user_id, chat_id, request_type:"chat", feature:"file_search+web_search"}`, `tools == [{"type":"file_search","vector_store_ids":["vs_1"],"max_num_results":5},{"type":"web_search","search_context_size":"low"}]`, `max_tool_calls == 2`, no `include`.
  - `code_interpreter_adds_include`; `no_tools_omits_tools_and_max_tool_calls_and_feature_none`; `image_input_content_array` (`[{"type":"input_text","text":"hi"},{"type":"input_image","file_id":"file-1"}]`); `api_params_only_when_set` (temperature absent when None; present when Some); `extra_body_merged_except_controlled_keys` (`{"model":"x","foo":1}` → `foo` present, model unchanged).
  - `parses_delta_tool_citation_completed` — feed events `response.output_text.delta`, `response.web_search_call.searching`, `response.web_search_call.completed`, `response.output_text.annotation.added` (url_citation with start/end), `response.completed` with usage incl. `input_tokens_details.cached_tokens` → expected `LlmEvent` sequence; `response.id` captured.
  - `event_name_falls_back_to_type_field` (`event: message`); `file_search_done_counts_results`; `code_interpreter_output_joined_and_truncated` (9000 chars → 8192 + `...[truncated]`); `incomplete_is_completed_with_reason`; `failed_reads_response_error_and_usage`; `error_event_flat_and_raw`; `unknown_and_malformed_events_are_ignored`.
  - client tests (with `FakeProvider`): `http_429_maps_to_rate_limited_with_retry_after`, `gateway_504_maps_to_timeout`, `http_500_body_message_sanitized`, `stream_end_without_terminal_is_provider_error`, `cancel_token_stops_stream`, `requests_use_s2s_context_and_alias_path` (recorded path `/api.openai.com/v1/responses`).
  - resolver: `tenant_override_changes_alias`, `model_placeholder_replaced`, `rag_provider_used_for_storage`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat llm` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): llm provider layer with OpenAI Responses adapter and fake provider`.

### Task 10: Quota service (preflight cascade, reserve, settlement)

**Files:** Create `src/domain/services/quota.rs`, `quota_tests.rs`; extend `src/infra/db/repos/quota.rs`; `tests/quota.rs`.

**Interfaces:**
- `PreflightInput { tenant_id, user_id, selected_model_id: String, snapshot: Arc<PolicySnapshot>, user_limits: UserLimits, content: String, image_count: u32, prior_context_tokens: i64, chat_has_ready_docs: bool, chat_has_ready_xlsx: bool, web_search_requested: bool, now: OffsetDateTime }`.
- `PreflightDecision { effective: ModelCatalogEntry, effective_tier: ModelTier, decision: QuotaDecision, downgrade_reason: Option<DowngradeReason>, estimated_input_tokens: i64, max_output_tokens_applied: i64, reserve_tokens: i64, reserved_credits_micro: i64, minimal_generation_floor_applied: i64, policy_version: u64, daily_start: Date, monthly_start: Date, send_file_search: bool, send_web_search: bool, send_code_interpreter: bool }`.
- `QuotaService::new(cfg, db)`; `async fn preflight(&self, input: &PreflightInput) -> DomainResult<PreflightDecision>` (one read tx; `QuotaExceeded{scope}`); `async fn reserve_in_tx(&self, tx: &DbTx<'_>, tenant_id, user_id, d: &PreflightDecision, user_limits: &UserLimits) -> DomainResult<()>` (upsert increments + re-read check → `QuotaExceeded{Tokens}`); `async fn settle_in_tx(&self, tx, s: &Settlement) -> DomainResult<()>` with `Settlement { tenant_id, user_id, daily_start, monthly_start, premium: bool, turn_reserved_credits_micro: i64, committed_credits_micro: i64, actual_input_tokens: i64, actual_output_tokens: i64, web_search_calls: i64, code_interpreter_calls: i64, method: SettlementMethod }`.
- Pure: `pub fn candidate_reserve(entry: &ModelCatalogEntry, input: &PreflightInput, cfg: &MiniChatConfig, ks: &KillSwitches) -> (i64 /*est input*/, i64 /*max out*/, i64 /*credits or i64::MAX*/)`; `pub fn committed_credits(actual_in: i64, actual_out: i64, reserve_tokens: i64, reserved_credits: i64, tolerance: f64, entry: &ModelCatalogEntry) -> DomainResult<(i64, bool /*capped*/)>`; `pub fn estimated_credits(reserve_tokens: i64, max_out_applied: i64, floor_applied: i64, entry) -> DomainResult<i64>`.

- [ ] **Step 1: Failing tests** (`quota_tests.rs` unit + `tests/quota.rs` with SQLite):
  - `cascade_allows_selected_premium_when_available` (decision allow, reserve booked equals candidate reserve).
  - `cascade_premium_daily_exhausted_downgrades_premium_quota_exhausted` (D§5.10 numbers: premium P mult 2.5e9, standard S 1e9, seeded rows → effective S, reserve 1_500_000 when the estimate is forced to 1000 input / 500 output via budgets).
  - `cascade_monthly_exhausted_also_downgrades`; `cascade_all_exhausted_is_quota_exceeded_tokens`.
  - `force_standard_tier_and_disable_premium_reasons`; `cascade_disabled_selected_model_downgrades_with_model_disabled`; `standard_never_upgrades`; `tier_without_enabled_model_skipped`.
  - `web_search_daily_quota_checked_only_when_requested_and_supported` (daily `web_search_calls == 75` → 429 `web_search` when requested; ok when not requested; ok when model lacks web_search support).
  - `code_interpreter_daily_quota_only_with_ready_xlsx`.
  - `surcharges_follow_candidate_tool_support_and_kill_switches`.
  - `reserve_upserts_total_and_premium_rows_daily_and_monthly`; `reserve_recheck_rejects_when_concurrent_reserve_filled_limit`; `standard_turn_touches_only_total`.
  - `settle_actual_updates_counters` (total: reserved -=, spent +=, calls +1, input/output tokens +=, web_search_calls +=; premium: no token telemetry); `settle_estimated_adds_tool_calls_not_tokens`; `settle_released_counts_call_only`.
  - `overshoot_within_tolerance_charges_actual`, `overshoot_beyond_tolerance_caps_at_reserved` (11000+500 vs reserve 10000 at 1.10 → committed = reserved).
  - `estimated_credits_formula` (`credits_micro(reserve - max_out, floor, …)`).
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat quota` and `--test quota` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): quota preflight cascade, reserve and settlement`.

### Task 11: Context assembly

**Files:** Create `src/domain/services/context.rs`, `context_tests.rs`; extend `repos/message.rs` (`recent_for_context`, `snapshot_boundary`, `prior_context_tokens`), `repos/thread_summary.rs` (`get_for_chat`).

**Interfaces:**
- `ContextInput { entry: ModelCatalogEntry, max_output_tokens_applied: i64, system_prompt_extra: Vec<String> /*guards*/, summary: Option<(String, i64 /*token_estimate*/)>, history: Vec<HistoryMessage{role, content}> /*chronological, already filtered*/, user_text: String, image_file_ids: Vec<String>, surcharges: i64 }`.
- `ContextPlan { instructions: String, messages: Vec<LlmMessage>, summary_applied: Option<i64>, messages_truncated: bool, assembled_context_tokens: i64, effective_budget: i64 }`.
- `pub fn assemble(input: &ContextInput) -> DomainResult<ContextPlan>` (pure; `ContextBudgetExceeded`); `pub fn guards_for(tools: &ToolSet, cfg: &MiniChatConfig) -> Vec<String>`; `ToolSet { file_search: Option<String /*vs id*/>, web_search: bool, code_interpreter: Vec<String>, search_knowledge: bool }`; `pub const SUMMARY_PREAMBLE: &str` (B.5.5 text).
- Repo: `MessageRepo::recent_for_context(conn, tenant_id, chat_id, boundary: (OffsetDateTime, Uuid), frontier: Option<(OffsetDateTime, Uuid)>, limit: u32) -> DomainResult<Vec<message::Model>>` (the D§4 query); `MessageRepo::snapshot_boundary(conn, tenant_id, chat_id) -> DomainResult<Option<(OffsetDateTime, Uuid)>>`; `MessageRepo::prior_context_tokens(conn, tenant_id, chat_id) -> DomainResult<i64>`.

- [ ] **Step 1: Failing tests:**
  - `instructions_are_system_prompt_plus_guards`; `web_guard_only_when_web_search_sent`; `file_guard_only_with_file_search`.
  - `summary_sent_as_user_message_with_preamble` (first message role user, starts with `SUMMARY_PREAMBLE`).
  - `history_then_user_message_with_images`.
  - `mandatory_over_budget_is_context_budget_exceeded`; `max_output_ge_context_window_is_exceeded`; `max_input_tokens_caps_budget` (max_input 0 = no cap).
  - `oldest_history_dropped_first_and_no_leading_assistant` (`messages_truncated == true`).
  - `summary_dropped_when_it_does_not_fit`; `deterministic_for_same_input`.
  - repo (SQLite): `recent_query_excludes_compressed_deleted_after_boundary_and_before_frontier`, `prior_context_tokens_uses_latest_assistant_with_usage`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat context` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): deterministic context assembly and truncation`.

### Task 12: Turn repository and usage/audit outbox handlers

**Files:** Create `src/infra/db/repos/turn.rs`, `turn_tests.rs`, `src/infra/outbox/{usage.rs, audit.rs, handlers_tests.rs}`; modify `pipeline.rs` to use them.

**Interfaces:**
- `TurnRepo` (assoc fns over a runner): `find_by_request(conn, tenant_id, chat_id, request_id) -> DomainResult<Option<chat_turn::Model>>`, `running_in_chat(conn, tenant_id, chat_id) -> Option<…>`, `latest_live(conn, tenant_id, chat_id) -> Option<…>` (max `(started_at,id)`, `deleted_at IS NULL`), `insert_running(tx, NewTurn) -> DomainResult<chat_turn::Model>` (unique violations → `TurnAlreadyRunning` for the running index, `RequestIdConflict` for `(chat_id, request_id)`; distinguish by re-reading), `fill_preflight(tx, turn_id, &PreflightFields) -> DomainResult<()>` (only when NULL), `touch_progress(conn, turn_id, counters: TurnCounters) -> DomainResult<()>`, `cas_finalize(tx, turn_id, f: &TerminalUpdate) -> DomainResult<bool>` (`WHERE state='running'`), `cas_orphan(tx, turn_id, cutoff, now) -> DomainResult<bool>`, `stale_running(conn, cutoff, limit: u64) -> DomainResult<Vec<chat_turn::Model>>`, `soft_delete(tx, turn_id, replaced_by: Option<Uuid>, now)`. `NewTurn`, `PreflightFields`, `TurnCounters { web_search, code_interpreter, file_search }`, `TerminalUpdate { state, error_code: Option<String>, error_detail: Option<String>, assistant_message_id: Option<Uuid>, provider_response_id: Option<String>, counters: TurnCounters, now }` — define in `turn.rs`.
- `UsageHandler::new(policy: Arc<dyn ModelPolicyGateway>)` and `AuditHandler::new(audit: Arc<dyn AuditGateway>)` implementing `LeasedMessageHandler` per spec §13.1 (audit: `attempts >= 120` turns Retry into Reject).

- [ ] **Step 1: Failing tests:**
  - `cas_finalize_only_once` (two calls → true, false; state/completed_at set).
  - `cas_orphan_rechecks_staleness` (fresh `last_progress_at` → false; stale → true; NULL progress falls back to `started_at`).
  - `insert_running_conflicts_map_to_domain_errors`; `latest_live_ignores_deleted`.
  - handler tests: `usage_handler_publishes_and_maps_outcomes` (bad JSON → Reject; plugin Transient → Retry; Permanent → Reject); `audit_handler_rejects_bad_payload_even_without_plugin`, `audit_handler_drops_when_no_plugin`, `audit_handler_120th_retry_becomes_reject`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat turn_tests handlers_tests` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): turn repository and usage/audit outbox handlers`.

### Task 13: Finalization service

**Files:** Create `src/domain/services/finalization.rs`, `finalization_tests.rs`.

**Interfaces:**
- `pub fn derive_billing(state: TurnState, error_code: Option<&str>, usage: Option<&LlmUsage>) -> (BillingOutcome, SettlementMethod)` (D§5.8 table; usage "known" for failed = some non-zero field; completed always Actual).
- `FinalizeInput { turn: chat_turn::Model /*with preflight fields*/, chat_model: String /*selected*/, user_id: Uuid, terminal: TerminalKind, text: String, usage: Option<LlmUsage>, provider_response_id: Option<String>, assistant_message_id: Uuid, counters: TurnCounters, file_search_calls: u32, daily_start: Date, monthly_start: Date, decision: Option<(QuotaDecision, Option<DowngradeReason>)>, latency_ms: i64, thread_summary: Option<ThreadSummaryPayload> }`; `TerminalKind { Completed{incomplete_reason: Option<String>}, Failed{code: String, detail: String}, Cancelled }`.
- `FinalizationService::new(deps)`; `async fn finalize(&self, input: FinalizeInput) -> FinalizeResult` with `FinalizeResult { Won{state: TurnState, quota_status: Vec<TierStatus> /* Task 8 type; the stream maps it to done.quota_warnings */}, Lost, PersistenceFailed /*turn now failed: message_persistence_failed*/, TxFailed }`. One transaction: assistant message insert (completed; cancelled with non-empty text best-effort via savepoint or pre-check) → CAS → settlement (multipliers from `policy.snapshot(user_id, policy_version_applied)`) → `UsageEvent` + `TurnAuditEvent` enqueue → optional thread-summary enqueue → commit → fire wakes. Reserve fields NULL → skip settlement (warn).
- `pub fn build_usage_event(...) -> UsageEvent` (dedupe key spec §8.3), `pub fn build_turn_audit(...) -> TurnAuditEvent`.

- [ ] **Step 1: Failing tests** (SQLite + in-process policy):
  - `derive_billing_table` — one assertion per D§5.8 row incl. unknown code → (Failed, Estimated) and failed with zero usage → Estimated.
  - `completed_persists_message_settles_actual_and_enqueues_usage_and_audit` — message row (id = pre-allocated, model = effective, tokens), turn `completed` with `assistant_message_id`, quota rows updated, exactly one usage outbox row (`billing_outcome:"completed"`, `settlement_method:"actual"`, `dedupe_key` = `{tenant.simple}/{turn.simple}/{request.simple}`, `policy_version_applied: 1`) and one audit row (`event_type:"turn_completed"`).
  - `second_finalize_is_lost_without_side_effects`.
  - `failed_without_usage_settles_estimated`; `failed_with_usage_settles_actual`; `cancelled_settles_estimated_aborted_and_persists_partial_text`; `cancelled_with_empty_text_has_no_message`; `empty_completed_text_still_persists_message`.
  - `quota_status_returned_on_win` (Won carries `TierStatus` for premium and total).
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat finalization` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): CAS-guarded turn finalization with settlement and outbox events`.

### Task 14: Send-message streaming, SSE relay, replay, turn status

**Files:** Create `src/domain/services/stream/{mod.rs, preflight.rs, provider_task.rs, relay.rs, replay.rs, citations.rs}`, `src/api/rest/{sse.rs, handlers/stream.rs, handlers/turns.rs}`, `tests/stream_api.rs`; `src/testing/sse.rs` (`parse_sse(body) -> Vec<(String, Value)>`, `TestApp::stream(user, path, body) -> SseCapture { status, events, problem: Option<Value> }`, `TestApp::stream_until(user, path, body, n_events) -> (Vec<…>, DropHandle)` for disconnect tests).

**Interfaces:**
- `StreamService::send(&self, ctx: SecurityContext, chat_id: Uuid, req: SendRequest) -> DomainResult<StreamStart>`; `SendRequest { content: String, request_id: Option<Uuid>, attachment_ids: Vec<Uuid>, web_search: bool }`; `StreamStart { Live(LiveStream), Replay(Vec<SseEvent>) }`; `LiveStream { events: mpsc::Receiver<SseEvent>, cancel: CancellationToken, first: SseEvent /*stream_started*/ }`.
- `pub(crate) async fn prepare_turn(&self, p: TurnPrep) -> DomainResult<PreparedTurn>` and `pub(crate) fn spawn_turn(&self, t: PreparedTurn) -> LiveStream` — reused by retry/edit in Task 16. `TurnPrep` carries chat, scope/tenant, user, content, images, web flag, request_id, preflight decision; `PreparedTurn` carries the inserted turn, context plan, resolved provider, LLM request, citation map, periods.
- `SseEvent { StreamStarted(StreamStartedData), Ping, Delta(DeltaData), Tool(ToolData), Citations(CitationsData), Done(DoneData), Error(ErrorData) }` with `fn into_axum(self) -> axum::response::sse::Event`; DTO structs from the OpenAPI subset live in `api/rest/dto.rs` (`pub`).
- `TurnService::status(ctx, chat_id, request_id) -> DomainResult<chat_turn::Model>` (Task 16 adds mutations to the same service).
- Handler: `handlers::stream::stream_message` — spawns the setup with `tokio::spawn` and awaits it; returns Problem JSON or `Sse` with `KeepAlive::new().interval(30s)`; dropping the body cancels the turn token (guard in `relay.rs`).

- [ ] **Step 1: Failing tests** `tests/stream_api.rs`:
  - `stream_happy_path_event_order_and_persistence` — events `stream_started{is_new_turn:true, request_id, message_id}`, `delta`×N, `done{usage:{input_tokens:10,output_tokens:5}, effective_model:"gpt-premium", selected_model:"gpt-premium", quota_decision:"allow", quota_warnings:[…]}`; no `downgrade_*` keys; content-type `text/event-stream`; DB: user + assistant messages share `request_id`, assistant `id == message_id`, turn `completed`; chat `updated_at` bumped; `message_count == 2`.
  - `server_generates_request_id_when_omitted`.
  - `provider_request_contains_expected_fields` — `FakeProvider::chat_requests()[0]` has `model` = provider_model_id, `instructions` starts with the catalog `system_prompt`, input ends with the user message, `user` 64 chars, `metadata.request_type == "chat"`, `max_output_tokens == min(4096, 32768)`.
  - `history_carried_into_second_turn_and_message_order` (second request input has prior user+assistant; list shows 4 messages in order).
  - `no_buffering_first_delta_before_provider_finishes` — script `hold_after: Some(1)`; read first delta before `release()`.
  - `ping_before_first_content` — config ping 5 s with a held stream (use `tokio::time::pause` or a 5 s hold) → a `ping` event with `{}` appears before the first delta and never after.
  - `tool_and_citation_events` — script web_search start/done + url annotation → `tool` start/done, one `citations` before `done` with `{source:"web", url, title, snippet, span}`; turn `web_search_completed_count == 1`; quota `total` daily `web_search_calls == 1`.
  - `web_search_calls_exceeded_mid_turn` — 3 web_search starts with limit 2 → SSE `error{code:"web_search_calls_exceeded"}`, turn failed, usage event `failed`/`estimated`.
  - `web_search_kill_switch_400_feature_disabled`; `web_search_daily_quota_429_web_search`.
  - `preflight_validation_before_provider_call` — empty content → 400 `EMPTY_CONTENT`; duplicate attachment ids → 400 `invalid_attachment`; foreign/not-ready attachment → 400 `invalid_attachment` and no turn/message/reserve rows; >4 images → 400 `TOO_MANY_IMAGES`; image + `disable_images` → 400 `images`/`FEATURE_DISABLED`; image + effective model without VISION_INPUT (premium exhausted → downgraded to non-vision standard) → 400 `VISION_NOT_SUPPORTED`; `INPUT_TOO_LONG` (max_input_tokens 50, long text); `CONTEXT_BUDGET_EXCEEDED` (context_window 200); in all cases `FakeProvider::chat_requests()` is empty.
  - `send_on_removed_model_is_invalid_model`.
  - `quota_exhausted_429_tokens_no_provider_call_no_rows`.
  - `downgrade_reported_in_done` — premium exhausted → `effective_model:"gpt-standard"`, `quota_decision:"downgrade"`, `downgrade_from:"gpt-premium"`, `downgrade_reason:"premium_quota_exhausted"`; assistant message `model == "gpt-standard"`.
  - `replay_completed_request_id_is_side_effect_free` — resend same `request_id` → `stream_started{is_new_turn:false, message_id = assistant id}`, one delta with full text, `done` without `quota_warnings`; no new provider request, quota rows unchanged, no new outbox rows.
  - `replay_checked_before_parallel_guard` — turn 1 completed; turn 2 held running; resend turn-1 id → replay (not 409).
  - `request_id_conflict_for_failed_or_running_or_deleted_turn` → 409 `request_id_conflict`.
  - `concurrent_sends_one_wins_other_409` (second send while first held → 409 `turn_already_running`); `new_turn_accepted_after_previous_terminal`.
  - `provider_error_message_is_sanitized_in_sse` — `ScriptedStream::failed("bad file-abcdefghijklmnop at https://x.y resp_123")` → `error{code:"provider_error", message}` without `file-`, `https://`, `resp_`; turn `failed`, `error_code "provider_error"`; usage event `failed`/`estimated`.
  - `provider_429_rate_limited`, `provider_timeout`.
  - `stream_disconnect_mid_stream_cancels_turn_and_settles_once` — read 1 delta, drop body → eventually turn `cancelled`, assistant partial message persisted with the received text, `assistant_message_id` set, one usage event `aborted`/`estimated`, quota `reserved_credits_micro` back to 0.
  - `stream_disconnect_during_setup_still_finalizes` (drop the request future right after sending; turn ends terminal, not running).
  - `cancel_before_any_text_has_null_assistant_message`.
  - `turn_status_states` — running (held) → `{state:"running"}` without `assistant_message_id`; done → `assistant_message_id`; error → `error_code`; unknown → 404 turn; other user → 404.
  - `stream_interrupted_when_watchdog_wins` — finalize the turn as failed directly in DB (simulate orphan CAS) before the provider completes → client gets `error{code:"stream_interrupted"}`.
  - `foreign_chat_send_404`.
- [ ] **Step 2: Run** `cargo test --offline -p cf-gears-mini-chat --test stream_api` → FAIL.
- [ ] **Step 3: Implement** per spec §8 (step order exactly as listed), §9, §11.2 tool selection; citations map built only when file_search is sent; progress touch ≤ every 30 s; terminal events only after `finalize` returns `Won`.
- [ ] **Step 4: Run** → PASS (also whole crate `cargo test --offline -p cf-gears-mini-chat`).
- [ ] **Step 5: Commit** `feat(mini-chat): SSE send-message pipeline, replay and turn status`.

### Task 15: Attachments (upload, get, delete, indexing, thumbnails, tools)

**Files:** Create `src/infra/llm/storage.rs`, `storage_tests.rs`, `src/infra/thumbnail.rs`, `thumbnail_tests.rs`, `src/domain/services/attachment.rs`, `src/infra/workers/{mod.rs, background_indexing.rs}`, `src/infra/db/repos/{attachment.rs (extend), vector_store.rs}`, `handlers/attachments.rs`, `tests/attachments_api.rs`; extend `FakeProvider` file/vector-store emulation.

**Interfaces:**
- `RagClient::new(gw, s2s)`: `upload_file(&self, p: &ResolvedProvider, filename: &str, content_type: &str, bytes: Bytes) -> Result<String, StorageError>`, `delete_file(p, file_id) -> Result<(), StorageError>` (404 ok), `create_vector_store(p, name) -> Result<String, _>`, `add_file(p, vs, file_id, attachment_id: Uuid) -> Result<VsFileStatus, _>`, `file_status(p, vs, file_id) -> Result<VsFileStatus, _>`, `delete_vector_store(p, vs) -> Result<(), _>`; `VsFileStatus { InProgress, Completed, Failed }`; `StorageError { Transient(String), Failed(String) }`. Paths/queries per spec §11.7.
- `thumbnail::make_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail { webp: Vec<u8>, width: u32, height: u32 }>`.
- `AttachmentService::upload(ctx, chat_id, part: UploadPart) -> DomainResult<attachment::Model>` where the handler first calls `AttachmentService::prepare_upload(ctx, chat_id) -> DomainResult<UploadContext>` (authz + chat + model + limits) before reading the body; `get(ctx, chat_id, id)`, `delete(ctx, chat_id, id)`; `chat_tool_facts(conn, tenant_id, chat_id) -> DomainResult<ChatToolFacts { ready_docs: bool, xlsx_file_ids: Vec<String>, vector_store_id: Option<String>, citation_map: HashMap<String, (Uuid, String)> }>` (used by Task 14's tool selection — wire it in this task, replacing any Task 14 stub).
- `background_indexing::spawn(...)` with gear-stop cancellation token.

- [ ] **Step 1: Failing tests** `tests/attachments_api.rs` (+ unit tests for storage paths and thumbnails):
  - `upload_document_ready_and_indexed` — PDF → 201 `{status:"ready", kind:"document"}` without `img_thumbnail`/`error_code`; provider got `POST /…/v1/files` multipart with `purpose=assistants`, filename `{chat_id}_{attachment_id}.pdf`; vector store created once; `chat_vector_stores` row; second doc reuses it.
  - `upload_image_ready_with_webp_thumbnail` (PNG 400×200 → thumbnail 128×64, `content_type:"image/webp"`, not added to vector store).
  - `upload_xlsx_ready_not_indexed`; `xlsx_rejected_when_code_interpreter_disabled_or_unsupported` (`CODE_INTERPRETER_UNAVAILABLE`).
  - `image_upload_rejected_when_disable_images`.
  - `multipart_errors` (no boundary, garbage body, no `file`, part without content type); `unsupported_mime_400`; `octet_stream_inferred`; `file_too_large_400` (`uploaded_file_max_size_kb` = 1, 2 KiB file); `document_limit_429` (`max_documents_per_chat` = 1); `storage_limit_429`.
  - `upload_into_foreign_chat_404_chat_resource`; `upload_on_removed_model_400_invalid_model`.
  - `provider_upload_failure_503_retry_10_row_failed` (row `status=failed`, `error_code=upload_failed`, GET shows it).
  - `indexing_failed_503_and_provider_file_deleted`.
  - `indexing_still_in_progress_returns_uploaded_then_background_ready` (statuses in_progress until after deadline — make the 25 s deadline injectable via `UploadTimings` in config-free test hook) → 201 `uploaded`; polling GET becomes `ready`.
  - `background_indexing_failure_marks_failed_and_enqueues_cleanup`.
  - `get_attachment_other_uploader_404` (A2 in A1's chat cannot exist since chats are owner-only — assert via seeded row with `uploaded_by_user_id` ≠ caller); `get_deleted_404`.
  - `delete_unreferenced_204_enqueues_cleanup_and_repeat_204_without_new_event`; `delete_referenced_409_attachment_locked`.
  - `file_search_tool_sent_only_with_ready_docs` (stream before upload: no tools; after: `file_search` with the vs id + guard in instructions; after deleting the doc: no file_search).
  - `code_interpreter_tool_with_ready_xlsx` (`container.file_ids`, `include`).
  - `image_attached_to_message_sent_as_input_image_once` (turn 2 without re-attach has no `input_image`).
  - `file_citation_mapped_to_attachment_id_and_filename` (provider file id never in SSE).
  - `message_attachments_recorded_and_listed`.
  - `provider_mismatch_409` (seed `chat_vector_stores.provider = "other"`).
  - `upload_concurrency_limit_503_retry_5` (`max_concurrent_uploads` = 1, first upload held in the fake provider → second gets 503 with `Retry-After: 5`).
  - unit: `vector_store_loser_polls_then_503`, `stale_placeholder_reclaimed`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat --test attachments_api` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): attachments upload/get/delete, vector stores, indexing and thumbnails`.

### Task 16: Turn mutations (retry, edit, delete)

**Files:** Modify `src/domain/services/turn.rs`, `handlers/turns.rs`; Create `tests/turns_api.rs`.

**Interfaces:**
- `TurnService::retry(ctx, chat_id, request_id) -> DomainResult<LiveStream>`, `edit(ctx, chat_id, request_id, content: String) -> DomainResult<LiveStream>`, `delete(ctx, chat_id, request_id) -> DomainResult<()>`; uses `StreamService::prepare_turn/spawn_turn` (Task 14) and `ThreadSummaryRepo::invalidate_if_covers(tx, tenant_id, chat_id, user_msg_created_at, user_msg_id)`.

- [ ] **Step 1: Failing tests** `tests/turns_api.rs`:
  - `retry_latest_creates_new_turn_with_server_request_id` — SSE `stream_started.request_id` ≠ old; old turn `deleted_at` set and `replaced_by_request_id` = new; old messages soft-deleted; new user message same content; messages list shows only the new pair; audit outbox row `turn_retry` with `original_request_id`/`new_request_id`; chat `updated_at` bumped.
  - `edit_replaces_content`; `edit_empty_content_400`.
  - `delete_latest_204_and_audit_turn_delete`; then `GET turn` → 404 and resend of its request_id → 409 `request_id_conflict`.
  - `mutation_of_non_latest_409_not_latest_turn`; `mutation_of_deleted_turn_409_not_latest_turn`; `mutation_of_running_turn_400_turn_state` (held stream); `mutation_unknown_turn_404`; `foreign_chat_404`.
  - `mutation_by_other_requester_403` (seed turn `requester_user_id` = other user in A1's chat).
  - `retry_preflight_rejection_keeps_previous_answer` (quota exhausted → 429, old turn still live and not deleted).
  - `retry_copies_non_deleted_attachments_and_resends_images` (image re-sent as `input_image`; deleted attachment not copied).
  - `retry_setup_failure_marks_new_turn_failed` (context budget exceeded after commit → 400, new turn `failed` `context_length_exceeded`, no usage event).
  - `concurrent_retries_one_wins_other_generation_in_progress` (or `NOT_LATEST_TURN`, whichever the race produces — assert one 200 stream and one 409).
  - `mutation_invalidates_covering_summary` (seed `thread_summaries` frontier at the turn's user message → row deleted, `is_compressed` cleared).
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat --test turns_api` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): retry, edit and delete of the last turn`.

### Task 17: Thread summary (trigger, handler, context use)

**Files:** Create `src/domain/services/thread_summary.rs`, `thread_summary_tests.rs`, `src/infra/outbox/thread_summary.rs`; extend `repos/thread_summary.rs`; `tests/thread_summary.rs`; wire trigger into finalization input (Task 14 code) and the handler into the pipeline.

**Interfaces:**
- `pub fn should_trigger(plan: &ContextPlan, has_summary: bool, pct: u32) -> bool`; `ThreadSummaryService::build_payload(conn, tenant_id, chat_id, causing_request_id) -> DomainResult<Option<ThreadSummaryPayload>>`; `ThreadSummaryHandler` (`LeasedMessageHandler`); `pub fn build_summary_prompt(existing: Option<&str>, msgs: &[(MessageRole, String)], limit: usize) -> String`; `pub fn parse_summary(text: &str) -> String`.

- [ ] **Step 1: Failing tests:**
  - unit: `parse_summary_extracts_summary_block_and_collapses_blank_lines`, `parse_summary_without_tags_keeps_text`, `parse_summary_with_dangling_markup_is_empty`, `prompt_contains_existing_summary_block_and_entries`, `message_content_truncated_with_ellipsis`, `token_estimate_rules` (output − reasoning, else ceil(bytes/4)).
  - integration (`context_window` small so the proactive threshold fires): `completed_turn_enqueues_summary_with_frozen_target_excluding_causing_turn`; `handler_commits_summary_marks_compressed_and_emits_system_usage_event` (`billing_outcome:"system_task"`, `settlement_method:"none"`, `requester_type:"system"`, no `user_id`/`turn_id`, dedupe `{tenant}/thread_summary_update/{sys_id}`; summary request `metadata.request_type == "summary"`, `stream:false`); `next_turn_uses_summary_and_stream_started_reports_thread_summary_applied`; `provider_failure_retries_then_rejects_at_max_attempts_keeping_old_summary`; `empty_summary_retries`; `missing_summary_model_rejects`; `cas_conflict_does_not_double_commit`; `frontier_deleted_skips_commit`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat thread_summary` and `--test thread_summary` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): outbox-driven thread summaries`.

### Task 18: Cleanup handlers (attachment cleanup, chat cleanup)

**Files:** Create `src/infra/outbox/{attachment_cleanup.rs, chat_cleanup.rs}`, `tests/cleanup.rs`; wire into the pipeline.

**Interfaces:** `AttachmentCleanupHandler::new(deps)`, `ChatCleanupHandler::new(deps)` implementing `LeasedMessageHandler` per spec §13.1 / D§3.6 cleanup + D§4 Phase 2.

- [ ] **Step 1: Failing tests** `tests/cleanup.rs`:
  - `chat_delete_cleans_files_then_vector_store` — chat with 2 docs + 1 image → after outbox drain: provider got `DELETE …/files/{id}` ×3 and `DELETE …/vector_stores/{vs}`; attachments `cleanup_status='done'`; `chat_vector_stores` row gone.
  - `file_delete_404_counts_as_success`.
  - `failed_file_delete_retries_then_marks_failed_and_still_deletes_vector_store` (`max_attempts` 2).
  - `vector_store_delete_failure_keeps_row_and_rejects_at_max_attempts`.
  - `chat_cleanup_for_active_chat_rejected`.
  - `attachment_cleanup_deletes_provider_file_and_marks_done`; `attachment_cleanup_skips_when_chat_deleted`; `attachment_cleanup_without_provider_file_marks_done`; `malformed_payload_rejected`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat --test cleanup` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): outbox cleanup of provider files and vector stores`.

### Task 19: Orphan watchdog, upload reaper, leader election

**Files:** Create `src/infra/workers/{leader.rs, orphan_watchdog.rs, upload_reaper.rs}`, `tests/workers.rs`.

**Interfaces:**
- `trait LeaderElector: Send + Sync { async fn is_leader(&self, role: &str) -> bool; }`; `NoopElector`; `#[cfg(feature = "k8s")] K8sLeaseElector` (Lease `mini-chat-{role}`, 15 s / 2 s, `POD_NAMESPACE`, `POD_NAME`).
- `OrphanWatchdog::scan_once(&self, now: OffsetDateTime) -> DomainResult<u32>`; `UploadReaper::scan_once(&self, now) -> DomainResult<u32>`; both `fn spawn(self: Arc<Self>, cancel: CancellationToken) -> JoinHandle<()>` honoring intervals and leadership.

- [ ] **Step 1: Failing tests** `tests/workers.rs`:
  - `watchdog_fails_stale_running_turn_with_estimated_settlement` — held stream with `last_progress_at` backdated 400 s → `scan_once` → turn `failed`/`orphan_timeout`; quota reserve released and spent = estimated credits; one usage event `aborted`/`estimated` with `selected_model == effective_model`; audit `turn_failed` with decision `unknown`; the still-open client stream ends with `error{code:"stream_interrupted"}`.
  - `watchdog_skips_fresh_and_terminal_turns`; `watchdog_null_progress_uses_started_at`; `watchdog_null_reserve_fields_skips_settlement_but_emits_usage` (`policy_version_applied: 0`, `usage: null`, `actual_credits_micro: 0`).
  - `reaper_fails_stale_pending_and_uploaded_rows` (`error_code='upload_abandoned'`; cleanup event only when `provider_file_id` present; `cleanup_status` rows skipped; fresh rows skipped).
  - `noop_elector_is_always_leader`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat --test workers` → FAIL → implement → PASS. Also `cargo check --offline -p cf-gears-mini-chat --features k8s` → OK.
- [ ] **Step 5: Commit** `feat(mini-chat): orphan watchdog, upload reaper and leader election`.

### Task 20: OAGW provisioning, S2S context, gear lifecycle

**Files:** Create `src/infra/oagw/provisioning.rs`, `provisioning_tests.rs`; Modify `src/gear.rs` (start/stop: S2S exchange → provisioning → outbox pipeline with all real handlers → summary model check → watchdog + reaper; stop: cancel workers + background indexing, join with timeout, stop outbox), `config/mini-chat.yaml` (keep valid; add a commented `allow_http_upstream` hint under `oagw` if useful).

**Interfaces:**
- `pub fn route_specs(entry: &ProviderEntry) -> Vec<RouteSpec { method: HttpMethod, path: String, query_allowlist: Vec<String> }>`; `pub fn upstream_request(entry_or_override) -> CreateUpstreamRequest`; `Provisioner::provision_all(&self, ctx: SecurityContext) -> Result<Vec<String /*deferred provider ids*/>, anyhow::Error>`; `Provisioner::spawn_reconcile(self: Arc<Self>, deferred, cancel)`.
- `S2sBootstrap::exchange(authn: Arc<dyn AuthNResolverClient>, creds) -> anyhow::Result<SecurityContext>` (retry ~30 s while the plugin is not registered).

- [ ] **Step 1: Failing tests** `provisioning_tests.rs` (with a recording fake `ServiceGatewayClientV1`):
  - `chat_route_prefix_and_query_allowlist` (`/openai/v1/responses?api-version=x` → POST `/openai/v1/responses`, allowlist `["api-version"]`; `/v1/chat/{model}/x` → prefix `/v1/chat`).
  - `rag_routes_for_openai_and_azure` (openai `/v1/files` POST, `/v1/files` DELETE, `/v1/vector_stores` POST/DELETE/GET; azure `/openai/…` with `api-version`).
  - `upstream_uses_http_and_port_when_use_http`, `alias_defaults_to_host`, `tenant_override_gets_own_upstream`.
  - `already_exists_is_reused`; `failed_precondition_is_deferred_not_fatal`; `validation_error_is_fatal`; `auto_derived_alias_rejection_retries_without_alias`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat provisioning` → FAIL → implement → PASS.
- [ ] **Step 5: Verify the real server** — build with the Global Constraints command; run it with `config/mini-chat.yaml` patched copy in `/tmp` (static_tokens not needed; `AZURE_OPENAI_API_HOST=127.0.0.1`, `oagw.config.allow_http_upstream: true`), record pid (`echo $! > /tmp/server.pid`), `curl -s localhost:8087/mini-chat/v1/models` → 200 JSON, `POST /mini-chat/v1/chats` → 201; stop with `kill "$(cat /tmp/server.pid)"`. Never use `pkill -f`/`pgrep -f`.
- [ ] **Step 6: Commit** `feat(mini-chat): OAGW provisioning and full gear lifecycle`.

### Task 21: Remaining adapters and knowledge search

**Files:** Create `src/infra/llm/{openai_chat_completions.rs, vllm_responses.rs, anthropic_messages.rs, anthropic_files.rs}` + tests; knowledge search retriever `src/infra/llm/knowledge.rs` + agentic loop in `provider_task.rs`.

**Interfaces:** each adapter implements `ProviderAdapter` (Task 9); `adapter_for(kind: ProviderKind) -> Arc<dyn ProviderAdapter>`; `KnowledgeRetriever::search(query, top_k) -> Result<Vec<String>, LlmError>`.

- [ ] **Step 1: Failing tests:**
  - chat completions: `body_uses_messages_and_stream_options` (system message first, `stream_options.include_usage`, `max_completion_tokens` or `max_tokens` per D§, no built-in tools, `user` set), `parses_choices_delta_and_usage` (`prompt_tokens`→input), `function_call_tool_events`.
  - vllm: `drops_all_tools_and_metadata`, `think_blocks_become_reasoning_deltas`.
  - anthropic: `anthropic_chat_image_gets_secondary_copy` (image upload in a chat whose model's provider is `anthropic_messages` → `secondary_status='uploaded'`, `secondary_provider_kind='anthropic'`, secondary file deleted by attachment cleanup via `secondary_ref`), `body_has_system_messages_max_tokens_metadata_user_id`, `drops_file_search_maps_web_search_and_code_execution`, `parses_content_block_deltas_and_message_delta_usage`, `max_tokens_stop_is_incomplete`.
  - knowledge: `search_knowledge_tool_offered_only_without_file_search`, `agentic_loop_runs_retrieval_and_continues`, `iterations_exceeded_error`, `unexpected_tool_use_error`.
- [ ] **Step 2–4: Run** `cargo test --offline -p cf-gears-mini-chat adapters knowledge` → FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(mini-chat): chat-completions, vLLM and Anthropic adapters and knowledge search`.

### Task 22: Black-box smoke test and acceptance traceability

**Files:** Create `gears/mini-chat/e2e-smoke/{README.md, mock_openai.py, config.template.yaml, run_smoke.py}`, `gears/mini-chat/docs/acceptance-tests.md`.

**Interfaces:** `python3 gears/mini-chat/e2e-smoke/run_smoke.py` exits 0 on success. Mock: stdlib `http.server` (threaded) implementing `POST /v1/responses` (SSE text completion; records bodies), `/v1/files`, `/v1/vector_stores*`.

- [ ] **Step 1: Write the smoke script** covering: start mock + server (generated config: static_tokens authn with two tenants, `oagw.allow_http_upstream: true`, `ssrf_policy.enabled: false`, provider `openai {kind: openai_responses, storage_kind: openai, host: 127.0.0.1, port: <mock>, use_http: true, upstream_alias: "127.0.0.1"}`, credstore secret `openai-key`, sqlite DB in a temp dir, throttling zones defined); create chat → stream (assert event order, `done.usage`) → replay → messages list → turn status → second tenant gets 404 → upload text document (ready) → stream again (mock recorded request has `file_search`) → delete chat (204) → DB checks via `sqlite3` (`chat_turns.state='completed'`, `quota_usage` rows, outbox rows). Kill server by pid only.
- [ ] **Step 2: Run** `python3 gears/mini-chat/e2e-smoke/run_smoke.py` → PASS (fix the gear, not the script, when behaviour diverges from the spec).
- [ ] **Step 3: Write `acceptance-tests.md`** — one row per acceptance-criteria item → test names (file::test) from Tasks 1–22.
- [ ] **Step 4: Run the full suite** `cargo test --offline -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat` → PASS.
- [ ] **Step 5: Commit** `test(mini-chat): black-box smoke test and acceptance traceability`.
