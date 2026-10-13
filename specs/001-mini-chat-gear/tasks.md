# Tasks: Mini Chat Gear

**Input**: Design documents from `/specs/001-mini-chat-gear/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: REQUIRED (spec FR-033, constitution III). Test tasks precede the implementation tasks of each story; tests are written first, run red, then made green.

**Paths**: `G` = `gears/mini-chat/mini-chat`, `S` = `gears/mini-chat/mini-chat-sdk`.

## Format: `[ID] [P?] [Story] Description`

## Acceptance-criteria → test map

| acceptance-criteria.md item | test file(s) |
|---|---|
| Tenant/owner isolation | `G/tests/authz_isolation_test.rs` |
| Context window budget (message + assembled) | `G/tests/context_budget_test.rs`, `G/src/domain/context_tests.rs` |
| Streaming never buffered | `G/tests/stream_test.rs::stream_relays_before_provider_finishes` |
| Chat model immutable | `G/tests/chats_api_test.rs::update_ignores_model` |
| Quota checked before provider call | `G/tests/quota_test.rs::rejected_preflight_makes_no_provider_call` |
| Chat CRUD + model/title validation | `G/tests/chats_api_test.rs` |
| List filtering/ordering/pagination/malformed input | `G/tests/chats_api_test.rs`, `G/tests/messages_api_test.rs` |
| Chat ordering by activity | `G/tests/chats_api_test.rs::list_orders_by_recent_activity` |
| Messages list / contract / count & order | `G/tests/messages_api_test.rs` |
| Send-message stream + request id / preflight / persistence | `G/tests/stream_test.rs` |
| SSE contract / done payload / error terminal & sanitized | `G/tests/stream_test.rs`, `G/src/infra/llm/openai_responses_tests.rs` |
| Idempotency & replay / conflicts / replay before guard | `G/tests/idempotency_test.rs` |
| Parallel turns | `G/tests/idempotency_test.rs` |
| Turn mutations (4 items) | `G/tests/turns_test.rs` |
| Turn lifecycle / partial & null content | `G/tests/turns_test.rs` |
| Attachments (4 items) | `G/tests/attachments_test.rs` |
| Models API | `G/tests/models_reactions_quota_api_test.rs` |
| Reactions API | `G/tests/models_reactions_quota_api_test.rs` |
| Quota status API | `G/tests/models_reactions_quota_api_test.rs` |
| Quota enforcement (reserve, downgrade, credits) | `G/tests/quota_test.rs`, `G/src/domain/credits_tests.rs`, `G/src/domain/quota_tests.rs` |
| Settlement exactly once / usage published once | `G/tests/quota_test.rs`, `G/tests/background_test.rs` |
| Context assembly / tool availability & guidance | `G/tests/context_budget_test.rs` |
| Error mapping & sanitization | `G/tests/errors_test.rs`, `G/src/domain/sanitize_tests.rs` |
| Web search | `G/tests/web_search_test.rs` |
| Cleanup & recovery (chat cleanup, thread summary) | `G/tests/background_test.rs` |
| Authorization | `G/tests/authz_isolation_test.rs` |

---

## Phase 1: Setup

- [X] T001 Create SDK crate `S/Cargo.toml` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) and `S/src/lib.rs`; add `gears/mini-chat/mini-chat-sdk` to root `Cargo.toml` workspace members and a `mini-chat-sdk` workspace dependency
- [X] T002 Add `mini-chat-sdk`, `toolkit-odata-macros`, `tokio-stream`, `async-stream` deps and dev-deps (`tower` util, `http-body-util`, `tempfile`) to `G/Cargo.toml`
- [X] T003 Create module skeleton `G/src/{lib.rs,gear.rs,config.rs,api/,domain/,infra/}` keeping `infra::plugins::{static_audit, static_model_policy}` public

## Phase 2: Foundational (blocking)

- [X] T004 [P] SDK models (`ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `ModelTier` accepting `Premium`/`premium`, `KillSwitches`, `PolicySnapshot`, `UserLimits`, `TierLimits`, `UsageEvent`, audit events) in `S/src/models.rs`
- [X] T005 [P] SDK plugin traits `MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1` + errors in `S/src/plugin_api.rs`, `S/src/error.rs`; GTS specs in `S/src/gts.rs`
- [X] T006 [P] Unit tests for SDK serde (catalog YAML shape from `config/mini-chat.yaml`) in `S/src/models_tests.rs`
- [X] T007 Config structs with defaults/validation per DESIGN Appendix B in `G/src/config.rs`; unit tests in `G/src/config_tests.rs`
- [X] T008 Migrations for all 10 tables (PG + SQLite) in `G/src/infra/db/migrations/` + outbox migrations
- [X] T009 SeaORM entities with `Scopable` in `G/src/infra/db/entities/`
- [X] T010 Domain error enum `G/src/domain/error.rs` and canonical mapping + resource types in `G/src/api/rest/error.rs`
- [X] T011 PEP helper (`ResourceType`s, actions, `ensure_owner`, 403/503 mapping) in `G/src/domain/authz.rs`
- [X] T012 [P] Credits arithmetic (checked, per-component ceil) `G/src/domain/credits.rs` + tests `credits_tests.rs`
- [X] T013 [P] Token estimation `G/src/domain/estimation.rs` + tests
- [X] T014 [P] Provider-id sanitization `G/src/domain/sanitize.rs` + tests `sanitize_tests.rs`
- [X] T015 Model policy gateway (plugin selection, empty catalog fallback) `G/src/infra/policy.rs`; audit gateway `G/src/infra/audit.rs`
- [X] T016 Static plugins gears `G/src/infra/plugins/static_model_policy/`, `G/src/infra/plugins/static_audit/`
- [X] T017 Outbox enqueuer + payload types `G/src/infra/outbox/{mod.rs,payloads.rs}`
- [X] T018 Gear wiring (init/migrations/register_rest/start/stop) `G/src/gear.rs`
- [X] T019 Integration test harness (SQLite, fake `ServiceGatewayClientV1`, mock AuthZ, static policy client, router builder, SSE reader, DB helpers) in `G/tests/common/mod.rs`

## Phase 3: User Story 1 — Converse privately (P1) 🎯 MVP

**Independent test**: create chat, stream against fake provider, verify SSE + DB + list ordering.

### Tests (write first)
- [X] T020 [P] [US1] Chat CRUD/list/validation/order tests in `G/tests/chats_api_test.rs`
- [X] T021 [P] [US1] Messages list/contract tests in `G/tests/messages_api_test.rs`
- [X] T022 [P] [US1] Streaming, SSE contract, error events, persistence tests in `G/tests/stream_test.rs`
- [X] T023 [P] [US1] Replay/conflict/parallel-turn tests in `G/tests/idempotency_test.rs`
- [X] T024 [P] [US1] Isolation/authz tests (other user, other tenant, PDP deny 403, PDP failure 503) in `G/tests/authz_isolation_test.rs`
- [X] T025 [P] [US1] OpenAI Responses SSE translation unit tests in `G/src/infra/llm/openai_responses_tests.rs`

### Implementation
- [X] T026 [US1] Repositories (chats, messages, turns, message_attachments, reactions read) in `G/src/infra/db/repo/`
- [X] T027 [US1] Chat service (create/get/list/update/delete, default model) `G/src/domain/services/chat_service.rs`
- [X] T028 [US1] Message listing service with attachments/my_reaction enrichment `G/src/domain/services/message_service.rs`
- [X] T029 [US1] LLM abstraction + OpenAI Responses adapter (request builder, SSE parser, error mapping) `G/src/infra/llm/`
- [X] T030 [US1] Provider resolver + OAGW provisioning `G/src/infra/llm/{resolver.rs,provisioning.rs}`
- [X] T031 [US1] Stream service (idempotency, guard, preflight, reserve tx, provider task, finalization, replay, cancellation) `G/src/domain/services/stream_service.rs`, `finalization.rs`
- [X] T032 [US1] Context assembly & truncation `G/src/domain/context.rs` + tests `context_tests.rs`
- [X] T033 [US1] DTOs, handlers, SSE writer, routes for chats/messages/stream/turn status `G/src/api/rest/`

## Phase 4: User Story 2 — Cost limits (P1)

### Tests
- [X] T034 [P] [US2] Quota enforcement/downgrade/settlement tests in `G/tests/quota_test.rs`
- [X] T035 [P] [US2] Quota cascade unit tests in `G/src/domain/quota_tests.rs`
- [X] T036 [P] [US2] Models/reactions/quota-status API tests in `G/tests/models_reactions_quota_api_test.rs`
- [X] T037 [P] [US2] Context budget / tool availability tests in `G/tests/context_budget_test.rs`

### Implementation
- [X] T038 [US2] Quota service (preflight cascade, reserve + re-check, settlement, warnings, status) `G/src/domain/quota.rs`
- [X] T039 [US2] Models API, quota status API, reactions service + routes `G/src/domain/services/{model_service.rs,reaction_service.rs}`, `G/src/api/rest/handlers/`

## Phase 5: User Story 3 — Correct the last exchange (P2)

- [X] T040 [P] [US3] Turn mutation & lifecycle tests in `G/tests/turns_test.rs`
- [X] T041 [US3] Turn service (preview, mutation commit, summary invalidation, audit) `G/src/domain/services/turn_service.rs`; retry/edit/delete routes

## Phase 6: User Story 4 — Documents and images (P2)

- [X] T042 [P] [US4] Attachment upload/get/delete/indexing/limits/cleanup tests in `G/tests/attachments_test.rs`
- [X] T043 [P] [US4] Thumbnail unit tests `G/src/infra/thumbnail_tests.rs`
- [X] T044 [US4] Storage clients (Files, Vector Stores) via OAGW `G/src/infra/llm/storage.rs`
- [X] T045 [US4] Attachment service (multipart streaming, limits, purposes, vector store protocol, background indexing) `G/src/domain/services/attachment_service.rs`; thumbnail `G/src/infra/thumbnail.rs`; routes

## Phase 7: User Story 5 — Web search, reactions, models (P3)

- [X] T046 [P] [US5] Web search tests (tool events, citations, per-turn limit, daily quota, kill switch) in `G/tests/web_search_test.rs`
- [X] T047 [US5] Web search/code-interpreter tool gating and per-turn limits in stream service

## Phase 8: User Story 6 — Background processing (P3)

- [X] T048 [P] [US6] Outbox handlers, chat cleanup, thread summary, orphan watchdog, upload reaper tests in `G/tests/background_test.rs`
- [X] T049 [US6] Outbox handlers (usage, audit, attachment cleanup, chat cleanup, thread summary) `G/src/infra/outbox/handlers/`
- [X] T050 [US6] Orphan watchdog and upload reaper `G/src/infra/workers/`

## Phase 9: Polish

- [X] T051 [P] Error-contract tests (422/415/400 extractor reasons, sanitization end-to-end) in `G/tests/errors_test.rs`
- [X] T052 Build example server with required features; run full test suite
- [X] T053 Manual smoke run of the server against a mock provider (quickstart.md)

## Dependencies & Execution Order

- Setup → Foundational → US1 → (US2, US3, US4, US5, US6 in priority order; US2–US6 depend on US1's stream service).
- Within a story: tests first (red), then implementation (green).

## Parallel Opportunities

- T004–T006, T012–T014 are independent files.
- All test-writing tasks marked [P] within a story can be authored in parallel.

## Implementation Strategy

MVP = Phase 1–3 (chat + streaming + idempotency). Then quota (US2), mutations (US3), attachments (US4), web search (US5), background (US6), polish.

## Implementation notes (recorded by /speckit-implement)

All tasks above are complete. Where the final layout differs from the planned file names:

| Planned | Actual |
|---|---|
| `G/src/domain/services/chat_service.rs` | `G/src/domain/services/chats.rs` |
| `G/src/domain/services/message_service.rs` | `G/src/domain/services/messages.rs` |
| `G/src/domain/services/stream_service.rs`, `finalization.rs` | `G/src/domain/services/stream.rs`, `finalize.rs` |
| `G/src/domain/services/turn_service.rs` | `G/src/domain/services/turns.rs` |
| `G/src/domain/services/attachment_service.rs` | `G/src/domain/services/attachments.rs` |
| `G/src/domain/services/{model_service,reaction_service}.rs` | `G/src/domain/services/misc.rs` |
| `G/src/infra/llm/resolver.rs` | `ProviderResolver` in `G/src/infra/llm/mod.rs` |
| `G/src/infra/outbox/handlers/` | `G/src/infra/outbox/handlers.rs` + domain logic in `G/src/domain/services/background.rs` |
| `G/src/infra/thumbnail_tests.rs` | inline `#[cfg(test)]` module in `G/src/infra/thumbnail.rs` (< 100 lines) |

Additional work found during implementation:

- `G/tests/provisioning_test.rs`: OAGW provisioning (S2S exchange, upstream/route shapes, deferral, on-demand retry, reconcile loop).
- `G/src/api/rest/error_tests.rs`: table-driven pin of the REST error mapping (ADR-0004 confirmation).
- SQLite robustness: write transactions take the write lock up front (`repo::acquire_write_lock`) so concurrent writers wait on the busy handler instead of failing with `SQLITE_BUSY`/`BUSY_SNAPSHOT`.
- OData cursors: datetime cursor keys carry the exact SQLite storage text (`infra/db/odata.rs`), fixing duplicate rows at page boundaries.
- On-demand provisioning: a provider/storage call to a still-pending upstream triggers an immediate provisioning attempt (bounded 3 s) in addition to the DESIGN reconcile schedule.

Validation: `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk --offline` (unit + 15 integration test files) passes; the example server builds with
`--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`; a manual smoke run against an aiohttp mock provider exercised streaming, replay, retry/cancel, attachments, reactions, quota, isolation and chat cleanup.

## Phase 10: Convergence

- [X] T054 Make `cargo clippy -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk --all-targets` clean under the workspace lint configuration without behavior changes per Constitution II (partial)
- [X] T055 [P] Add an integration test that a ready XLSX attachment adds the `code_interpreter` tool with its provider file ids (and the `include` of outputs) to the provider request in `G/tests/attachments_test.rs` per FR-019 (partial)
- [X] T056 [P] Add integration tests for the daily code-interpreter quota (429 `code_interpreter`) and the per-turn limit (`code_interpreter_calls_exceeded`) in `G/tests/web_search_test.rs` per FR-005 (partial)
- [X] T057 [P] Add a test that a thread-summary task is dead-lettered after `thread_summary_worker.max_attempts` failures in `G/tests/background_test.rs` per FR-031 (partial)
- [X] T058 [P] Add a test that `$select` is accepted and ignored (full items returned) on chat and message lists in `G/tests/chats_api_test.rs` per FR-007 (partial)
- [X] T059 [P] Add a test that the catalog `general_config.max_file_size_mb` caps uploads below the gear limit in `G/tests/attachments_test.rs` per FR-018 (partial)
- [X] T060 [P] Add a test that quota periods with limit ≤ 0 are omitted from the status API and `done.quota_warnings` in `G/tests/models_reactions_quota_api_test.rs` per FR-023 (partial)
- [X] T061 Record the justification for on-demand OAGW provisioning (request-path retry of deferred upstreams, in addition to the DESIGN reconcile schedule) in `specs/001-mini-chat-gear/research.md` per plan: provisioning decision (unrequested)

Convergence notes: T055–T060 tests live in `G/tests/convergence_test.rs`. T060 found that a limit ≤ 0 is omitted from the status and warnings but still blocks enforcement (nothing fits); the test asserts that. `stream_test.rs::ping_before_first_content` now waits for the ping before releasing content, so it is deterministic under parallel load.
