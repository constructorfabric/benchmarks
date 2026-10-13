# Tasks: Mini Chat Gear (multi-tenant AI chat) and its SDK

**Input**: Design documents from `/specs/001-mini-chat-gear/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: REQUIRED by the spec — every acceptance item AC-01..AC-48 maps to at least one test task,
and test tasks precede the implementation tasks they verify (TDD ordering). Integration tests live
in `gears/mini-chat/mini-chat/tests/` (never in `testing/e2e/suites/mini_chat/`); unit tests live in
`*_tests.rs` companion files.

**Organization**: grouped by user story (spec.md US1..US7). Paths are relative to the repository root;
`GC` = `gears/mini-chat/mini-chat`, `SDK` = `gears/mini-chat/mini-chat-sdk`.

## Format: `[ID] [P?] [Story] Description`

---

## Phase 1: Setup (shared infrastructure)

- [X] T001 Create SDK crate manifest `SDK/Cargo.toml` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`, deps async-trait, thiserror, uuid, time, serde, serde_json, schemars, gts, toolkit-gts, toolkit-security) and `SDK/src/lib.rs`
- [X] T002 Register the SDK in the root `Cargo.toml` (`[workspace] members` + `[workspace.dependencies] mini-chat-sdk`) and add `mini-chat-sdk`, `toolkit-odata-macros` (if needed), and dev-deps (`oagw` with `test-utils`, `tower`, `types-registry-sdk` test-util, `tempfile`) to `GC/Cargo.toml`
- [X] T003 Create the gear module skeleton in `GC/src/lib.rs` (`api`, `config`, `domain`, `gear`, `infra` with `pub mod infra::plugins::{static_audit, static_model_policy}`) and verify `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`

---

## Phase 2: Foundational (blocking prerequisites)

### Tests first

- [X] T004 [P] SDK serde tests (catalog entry from the `config/mini-chat.yaml` shape incl. `tier: Premium`, kill switches, UsageEvent JSON with omitted `user_id`/`turn_id`/`system_task_type`, `usage: null`) in `SDK/src/models_tests.rs`
- [X] T005 [P] Config tests (defaults per contracts/config.md, `deny_unknown_fields` on strict sections, worker sections accept unknown keys, validation ranges, `${VAR}` expansion, default `openai` provider) in `GC/src/config_tests.rs`
- [X] T006 [P] Credit arithmetic and estimation unit tests (`credits_micro_checked` bounds/rounding/zero multipliers; text estimate, reserve, floor) in `GC/src/domain/credits_tests.rs`
- [X] T007 [P] Provider error sanitizer tests (provider ids, URLs, `sk-` keys, Bearer tokens, `file-based`/`file_search` untouched) in `GC/src/domain/sanitize_tests.rs` (AC-44)
- [X] T008 [P] Canonical error mapping tests for every DomainError variant (category, status, reason, resource_type) in `GC/src/domain/error_tests.rs` (AC-43)

### Implementation

- [X] T009 Implement SDK models (`ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `ModelToolSupport`, `KillSwitches`, `TierLimits`, `UserLimits`, `PolicySnapshot`, `PolicyVersionInfo`, `ModelTier`) in `SDK/src/models.rs`
- [X] T010 [P] Implement SDK `UsageEvent`/`UsageTokens` in `SDK/src/usage.rs` and audit events (`TurnAuditEvent`, `TurnRetryAuditEvent`, `TurnEditAuditEvent`, `TurnDeleteAuditEvent`) in `SDK/src/audit.rs`
- [X] T011 [P] Implement SDK plugin traits + errors in `SDK/src/plugin_api.rs`, `SDK/src/error.rs` and GTS specs `MiniChatModelPolicyPluginSpecV1`, `MiniChatAuditPluginSpecV1` in `SDK/src/gts.rs`
- [X] T012 Implement gear config with validation and env expansion in `GC/src/config.rs`
- [X] T013 [P] Implement `credits_micro_checked`, text/reserve estimation in `GC/src/domain/credits.rs`
- [X] T014 [P] Implement sanitizer in `GC/src/domain/sanitize.rs`
- [X] T015 Implement `DomainError` + `From<DomainError> for CanonicalError` with `#[resource_error]` markers in `GC/src/domain/error.rs`
- [X] T016 Implement migrations (all tables/indexes of data-model.md, PG + SQLite) in `GC/src/infra/db/migrations.rs`
- [X] T017 [P] Implement SeaORM entities with `Scopable` in `GC/src/infra/db/entities/{chats,messages,chat_turns,attachments,message_attachments,thread_summaries,chat_vector_stores,quota_usage,message_reactions}.rs`
- [X] T018 Implement authz PEP helpers (resource types, actions, `ensure_owner`, enforcer error mapping) in `GC/src/domain/authz.rs`
- [X] T019 Implement plugin selection (lazy, vendor/priority, reset on missing client) in `GC/src/infra/plugin_select.rs` and policy gateway (`snapshot`, `user_limits`, `resolve_model`) in `GC/src/domain/policy.rs`
- [X] T020 [P] Implement bundled static model policy plugin gear (config, catalog validation, fixed version 1, default limits, `publish_usage` logs) in `GC/src/infra/plugins/static_model_policy/mod.rs`
- [X] T021 [P] Implement bundled static audit plugin gear (`enabled`, logs events) in `GC/src/infra/plugins/static_audit/mod.rs`
- [X] T022 Implement OAGW provisioning + provider resolver (aliases, tenant overrides, routes, reconcile loop) in `GC/src/infra/oagw_provisioning.rs` and `GC/src/infra/llm/resolver.rs`
- [X] T023 Implement outbox enqueuer (5 queues, partitions, payload types, `Wake`) in `GC/src/infra/outbox/mod.rs`
- [X] T024 Implement gear struct (`init`, `start`, `stop`, `migrations`, `register_rest`) in `GC/src/gear.rs`
- [X] T025 Build the integration test harness (shared-cache SQLite + migrations, OAGW `AppHarness` facade, scripted in-process mock OpenAI provider, PDP stub allow/deny/fail, static policy/audit plugins, router driven with `oneshot`, SSE parser helper, multi-user contexts) in `GC/tests/common/mod.rs`

**Checkpoint**: foundation compiles; unit tests T004–T008 pass.

---

## Phase 3: User Story 1 — Manage chats (P1) 🎯 MVP

**Goal**: chat CRUD with OData listing and isolation. **Independent test**: `GC/tests/chats_test.rs`.

- [X] T026 [P] [US1] Integration tests: create (default model, explicit model, INVALID_MODEL unknown/disabled, INVALID_TITLE empty/whitespace/256, 201 + Location, title omitted when null), get, rename (only title, `updated_at` bump, unknown fields ignored), delete (204 then 404), 422/400/415 body errors, invalid path param — in `GC/tests/chats_test.rs` (AC-06, AC-04)
- [X] T027 [P] [US1] Integration tests: list ordering by `updated_at desc`, `limit` clamp/0, cursor paging, `$filter contains(title,…)`, `$orderby title`, unknown field 400 with OData resource type, malformed cursor, `$select` accepted — in `GC/tests/chats_list_test.rs` (AC-07, AC-08)
- [X] T028 [P] [US1] Integration tests: another user (same tenant) and another tenant get 404 on chat routes; PDP deny → 403 `AUTHZ_DENIED`; PDP failure → 503 `Retry-After: 5` — in `GC/tests/authz_test.rs` (AC-01, AC-48)
- [X] T029 [US1] Implement chat repository (create/get/list via `paginate_odata`, rename, soft delete, touch `updated_at`, message_count) and OData field mapping in `GC/src/infra/db/repo/chats.rs`, `GC/src/infra/db/odata.rs`
- [X] T030 [US1] Implement chat service (default model algorithm, validation, PEP) in `GC/src/domain/chat_service.rs`
- [X] T031 [US1] Implement REST DTOs and chat routes/handlers (OperationBuilder, license gate, canonical extractors) in `GC/src/api/rest/dto.rs`, `GC/src/api/rest/routes.rs`, `GC/src/api/rest/handlers/chats.rs`
- [X] T032 [US1] Chat deletion: soft delete + mark attachments cleanup pending + enqueue chat-cleanup outbox event in one transaction (payload-too-large → 400) in `GC/src/domain/chat_service.rs`

**Checkpoint**: US1 tests pass.

---

## Phase 4: User Story 2 — Send a message and stream (P1)

**Goal**: streaming turns with persistence, quota, replay. **Independent test**: `GC/tests/stream_test.rs`, `GC/tests/idempotency_test.rs`.

- [X] T033 [P] [US2] Unit tests for the Responses SSE parser/translator (delta, tool events, code interpreter output, annotations → citations, completed/incomplete/failed/error, usage incl. cached/reasoning) and request builder (instructions + guards, input history, images, tools, max_tool_calls, include, user 64 hex, metadata, api_params, extra_body reserved keys) in `GC/src/infra/llm/responses_tests.rs` (AC-15, AC-42)
- [X] T034 [P] [US2] Unit tests for context assembly (budget formula, mandatory overflow → CONTEXT_BUDGET_EXCEEDED, summary drop, oldest whole turns dropped, never start with assistant, `messages_truncated`, preamble) in `GC/src/domain/context_tests.rs` (AC-02, AC-41)
- [X] T035 [P] [US2] Unit tests for quota cascade (allow, premium exhausted → downgrade, disabled model → `model_disabled`, kill switches, all exhausted → reject, candidate reserve per model, tool surcharges by tool_support) and warnings/status math in `GC/src/domain/quota_tests.rs` (AC-37, AC-38, AC-35)
- [X] T036 [P] [US2] Integration tests: happy stream (event order, `stream_started` fields, deltas, `done` fields incl. `quota_warnings`, no provider ids), provider request shape recorded by the mock, persisted user+assistant messages, turn `completed`, quota_usage rows settled, usage+audit outbox rows; `[[error]]` → SSE `error provider_error` sanitized; 429 → `rate_limited`; 500 → `provider_error`; incomplete → `done`; empty → `done` with empty message — in `GC/tests/stream_test.rs` (AC-12, AC-14, AC-15, AC-16, AC-17, AC-03, AC-05, AC-36, AC-39, AC-40, AC-44)
- [X] T037 [P] [US2] Integration tests: preflight rejections before any provider call (EMPTY_CONTENT, invalid/duplicate/foreign/not-ready attachment ids, too many ids, TOO_MANY_IMAGES, INPUT_TOO_LONG, VISION_NOT_SUPPORTED, web search kill switch, 429 tokens/web_search/code_interpreter) with zero mock requests and no turn rows — in `GC/tests/preflight_test.rs` (AC-13, AC-05, AC-02, AC-43)
- [X] T038 [P] [US2] Integration tests: replay of completed request id (is_new_turn=false, one delta, done, no provider call, no quota/outbox change), conflict 409 `request_id_conflict` for running/failed/cancelled/deleted turns, `turn_already_running` for different request id, replay checked before parallel guard, new turn accepted after terminal — in `GC/tests/idempotency_test.rs` (AC-18..AC-22)
- [X] T039 [P] [US2] Integration tests: client disconnect mid-stream → turn `cancelled`, partial text persisted, estimated settlement + `aborted` usage event; disconnect before any text → no assistant message — in `GC/tests/cancel_test.rs` (AC-27, AC-28, AC-39)
- [X] T040 [US2] Implement quota service (preflight cascade with per-candidate reserve, daily tool quotas, reserve write + re-check, settlement actual/estimated/overshoot cap, warnings, status) in `GC/src/domain/quota.rs` and `GC/src/infra/db/repo/quota.rs`
- [X] T041 [US2] Implement context assembly in `GC/src/domain/context.rs`
- [X] T042 [US2] Implement OpenAI Responses adapter (request builder + SSE translation + error mapping) in `GC/src/infra/llm/responses.rs` and provider client trait in `GC/src/infra/llm/mod.rs`
- [X] T043 [P] [US2] Implement Chat Completions, vLLM Responses and Anthropic Messages adapters per DESIGN tool rules in `GC/src/infra/llm/{chat_completions,vllm,anthropic}.rs`
- [X] T044 [US2] Implement turn/message repositories (insert turn+user message+attachment links, CAS finalization, progress refresh, counters, latest turn) in `GC/src/infra/db/repo/{turns,messages}.rs`
- [X] T045 [US2] Implement finalization (CAS, assistant message persistence/downgrade to failed, settlement, usage + audit events, thread-summary trigger) in `GC/src/domain/finalization.rs`
- [X] T046 [US2] Implement stream service (checks order, preflight, reserve tx, provider task with cancellation, tool call limits, progress heartbeat, citations mapping, terminal event gating) in `GC/src/domain/stream_service.rs`
- [X] T047 [US2] Implement SSE relay (bounded channel, ping before first content, keep-alive comments, `stream_interrupted`, disconnect → cancel) and stream handler in `GC/src/api/rest/sse.rs`, `GC/src/api/rest/handlers/stream.rs`

**Checkpoint**: US1 + US2 tests pass (MVP).

---

## Phase 5: User Story 3 — History, turn status, reactions (P1)

- [X] T048 [P] [US3] Integration tests: list messages default order, `$filter role eq 'user'`, `$orderby created_at desc`, cursor paging, `limit` clamp, invalid field 400; message DTO fields always present (`request_id`, `attachments`, `my_reaction`), `model`/token counts on assistant only, deleted attachments excluded; `message_count` across turns — in `GC/tests/messages_test.rs` (AC-09, AC-10, AC-11)
- [X] T049 [P] [US3] Integration tests: turn status states and optional fields (`done` + assistant_message_id, `error` + error_code, `cancelled`), 404 for deleted/foreign turns — in `GC/tests/turn_status_test.rs` (AC-27)
- [X] T050 [P] [US3] Integration tests: reactions like→dislike replace, DELETE idempotent 204, user message 400 `reaction_target`, INVALID_REACTION before authz, 422 missing field, unknown message 404, `my_reaction` reflected in list — in `GC/tests/reactions_test.rs` (AC-34)
- [X] T051 [US3] Implement message listing (OData mapping, batch attachments + reactions enrichment) in `GC/src/domain/message_service.rs` and handlers `GC/src/api/rest/handlers/messages.rs`
- [X] T052 [US3] Implement turn status endpoint in `GC/src/domain/turn_service.rs` and `GC/src/api/rest/handlers/turns.rs`
- [X] T053 [US3] Implement reactions repo/service/handlers in `GC/src/infra/db/repo/reactions.rs`, `GC/src/domain/reaction_service.rs`, `GC/src/api/rest/handlers/reactions.rs`

---

## Phase 6: User Story 4 — Retry, edit, delete last turn (P2)

- [X] T054 [P] [US4] Integration tests: retry/edit stream with new server request id, old turn soft-deleted with `replaced_by_request_id`, edit EMPTY_CONTENT, delete 204; non-latest → 409 NOT_LATEST_TURN; running → 400 turn_state; quota 429 leaves previous turn intact; concurrent retries → exactly one stream + one 409; attachment links copied (deleted excluded); `web_search_enabled` reused; mutation audit events — in `GC/tests/turn_mutations_test.rs` (AC-23..AC-26, AC-24)
- [X] T055 [US4] Implement mutation preview/commit (latest turn, terminal state, requester check, soft delete, new user message, attachment copy, summary invalidation, audit event) in `GC/src/domain/turn_service.rs` and repo support in `GC/src/infra/db/repo/turns.rs`
- [X] T056 [US4] Wire retry/edit through the stream service (preflight before commit, unstarted-turn failure codes) and delete handler in `GC/src/domain/stream_service.rs`, `GC/src/api/rest/handlers/turns.rs`

---

## Phase 7: User Story 5 — Attachments (P2)

- [X] T057 [P] [US5] Unit tests: MIME allowlist/inference from extension, CSV remap, filename default/truncation, purposes, thumbnail generation (fit, webp, size cap, decode limits) in `GC/src/infra/mime_tests.rs`, `GC/src/infra/thumbnail_tests.rs`
- [X] T058 [P] [US5] Integration tests: document upload → ready (mock file + vector store calls, chat_vector_stores row), image upload → ready + thumbnail, XLSX → code interpreter purpose, unsupported type 400, too large 400, per-chat document/storage limits 429, multipart errors, disable_images, INVALID_MODEL before body read, indexing failed → 503 + failed row, `in_progress` at deadline → `uploaded` then background `ready`, GET/DELETE foreign 404, delete referenced 409 `attachment_locked`, delete twice 204 + outbox event once — in `GC/tests/attachments_test.rs` (AC-29, AC-30, AC-32)
- [X] T059 [P] [US5] Integration tests: provider request tools — `file_search` with vector store id after ready document, `code_interpreter` container file ids + include, `input_image` file ids, citations mapped to attachment ids (deleted/unknown omitted) — in `GC/tests/tools_test.rs` (AC-31, AC-42)
- [X] T060 [US5] Implement MIME + thumbnail helpers in `GC/src/infra/mime.rs`, `GC/src/infra/thumbnail.rs`
- [X] T061 [US5] Implement storage clients (files upload/delete, vector store create/add/status/delete for openai/azure) in `GC/src/infra/llm/storage.rs`
- [X] T062 [US5] Implement attachment repo + service (limits, vector store creation protocol, indexing poll with 25 s deadline, background task, delete with outbox) in `GC/src/infra/db/repo/attachments.rs`, `GC/src/domain/attachment_service.rs`
- [X] T063 [US5] Implement upload/get/delete handlers (streaming multipart, concurrency semaphore 503) in `GC/src/api/rest/handlers/attachments.rs`

---

## Phase 8: User Story 6 — Quota, models, policy (P2)

- [X] T064 [P] [US6] Integration tests: models list/get (enabled only, no internal fields, 404 disabled/unknown); quota status tiers/periods/flags consistent with seeded `quota_usage`; premium exhausted → downgrade fields in `done`; all exhausted → 429 tokens; daily web search / code interpreter quotas; web search tool+guard+citations+`web_search_calls` accounting; per-turn web search limit → SSE `web_search_calls_exceeded` — in `GC/tests/quota_models_test.rs` (AC-33, AC-35, AC-37, AC-38, AC-45)
- [X] T065 [US6] Implement models handlers/service in `GC/src/domain/model_service.rs`, `GC/src/api/rest/handlers/models.rs`
- [X] T066 [US6] Implement quota status handler in `GC/src/api/rest/handlers/quota.rs`

---

## Phase 9: User Story 7 — Background reliability (P3)

- [X] T067 [P] [US7] Integration tests: chat cleanup handler deletes provider files then vector store (404 = success, retries, max attempts → failed), attachment cleanup handler, reaper marks stale `pending`/`uploaded` rows `upload_abandoned` and enqueues cleanup, orphan watchdog fails stale turns with estimated settlement + `aborted` usage event, usage handler publishes via plugin, audit handler drop/retry/reject — in `GC/tests/background_test.rs` (AC-46, AC-32, AC-39, AC-40, AC-27)
- [X] T068 [P] [US7] Integration tests: thread summary trigger (threshold / truncation), handler commit with CAS frontier, `is_compressed`, system usage event, provider failure keeps summary, mutation invalidation, summary applied in next request (`thread_summary_applied`, preamble) — in `GC/tests/thread_summary_test.rs` (AC-47, AC-41)
- [X] T069 [US7] Implement outbox handlers usage/audit/attachment cleanup/chat cleanup in `GC/src/infra/outbox/{usage,audit,attachment_cleanup,chat_cleanup}.rs`
- [X] T070 [US7] Implement thread summary handler in `GC/src/infra/outbox/thread_summary.rs`
- [X] T071 [US7] Implement orphan watchdog, upload reaper and leader elector (no-op; k8s Lease behind `k8s`) in `GC/src/infra/workers/{orphan_watchdog,upload_reaper,leader}.rs`

---

## Phase 10: Polish & cross-cutting

- [X] T072 [P] Metrics instruments (`mini_chat_*`) recorded on the documented paths in `GC/src/infra/metrics.rs`
- [X] T073 [P] Smoke e2e: `GC/tests/e2e_smoke/{mock_provider.py,smoke-config.yaml,run_smoke.sh,test_smoke.py}` against the real server binary
- [X] T074 Run quickstart validation (build gate, `cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat`, smoke run) and fix failures
- [X] T075 Update `config/mini-chat.yaml` comment (binary name) and add the AC → test traceability table to `GC/tests/README.md`

---

## Dependencies & Execution Order

- Phase 1 → Phase 2 (blocks everything) → US1 → US2 (needs chats) → US3 (needs turns) → US4 (needs
  stream) → US5 (attachments used by US2 tools tests) → US6 → US7 → Polish.
- Within each story, test tasks come first and must fail before implementation.
- [P] tasks touch different files and can run in parallel.

## Parallel Example

```text
T004, T005, T006, T007, T008 (foundational unit tests) in parallel
T033, T034, T035 (US2 unit tests) in parallel
T048, T049, T050 (US3 integration tests) in parallel
```

## Implementation Strategy

MVP = Phase 1 + 2 + US1 + US2 (create chats and stream answers). Then add US3..US7 incrementally,
keeping the server build green after every phase.

## AC → Test Traceability

| AC | Test task(s) |
|---|---|
| AC-01, AC-48 | T028 |
| AC-02 | T034, T037 |
| AC-03 | T036 |
| AC-04 | T026 |
| AC-05, AC-36 | T036, T037 |
| AC-06 | T026 |
| AC-07, AC-08 | T027 |
| AC-09, AC-10, AC-11 | T048 |
| AC-12, AC-14, AC-15, AC-16, AC-17 | T033, T036 |
| AC-13 | T037 |
| AC-18..AC-22 | T038 |
| AC-23..AC-26 | T054 |
| AC-27, AC-28 | T039, T049, T067 |
| AC-29, AC-30, AC-32 | T058, T067 |
| AC-31 | T059 |
| AC-33, AC-35 | T064 |
| AC-34 | T050 |
| AC-37, AC-38 | T035, T064 |
| AC-39, AC-40 | T036, T039, T067 |
| AC-41 | T034, T068 |
| AC-42 | T033, T059 |
| AC-43 | T008, T037 |
| AC-44 | T007, T036 |
| AC-45 | T064 |
| AC-46 | T067 |
| AC-47 | T068 |

## Phase 11: Convergence

- [X] T076 Implement the Kubernetes Lease leader elector behind the `k8s` cargo feature (Leases `mini-chat-orphan-watchdog` / `mini-chat-upload-reaper` in `POD_NAMESPACE`, holder `POD_NAME`, 15 s lease, 2 s renew, create the Lease when missing) in `GC/src/domain/workers.rs` / `GC/src/infra/leader.rs`, select it in `start` when built with `k8s`, and keep `NoopElector` otherwise, per T071 / plan R6 (partial)
- [X] T077 Record in `specs/001-mini-chat-gear/research.md` the decisions to start the outbox pipeline in `init` (prevents the start-time race between queue registration and request transactions and the unbound-enqueuer window) and to open every write transaction with a no-op write (`begin_write`, avoids `SQLITE_BUSY_SNAPSHOT`), per plan R6 / R7 (contradicts)
- [X] T078 Make `cargo clippy -p cf-gears-mini-chat --all-targets` pass under the workspace lint profile (fix findings; targeted `#[allow(..., reason = ...)]` only for wire-format structs), keeping all tests green, per Constitution II (partial)
