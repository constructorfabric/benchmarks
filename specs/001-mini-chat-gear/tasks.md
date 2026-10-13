---
description: "Task list for the mini-chat gear and SDK"
---

# Tasks: Mini Chat Gear

**Input**: Design documents from `/specs/001-mini-chat-gear/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: REQUIRED by the specification (FR-090..FR-092): every acceptance-criteria item must be
covered by passing automated tests. Within each story, test tasks come first (write → see them fail
→ implement → pass).

**Organization**: grouped by user story (spec.md). Paths are relative to the repository root;
`G` = `gears/mini-chat/mini-chat`, `S` = `gears/mini-chat/mini-chat-sdk`, `BB` =
`gears/mini-chat/mini-chat/tests/blackbox`.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel (different files, no dependency on an incomplete task)
- **[Story]**: US1..US7 (spec.md user stories)

---

## Phase 1: Setup (Shared Infrastructure)

- [X] T001 Create SDK crate `S/Cargo.toml` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), add it to `[workspace] members` and `[workspace.dependencies]` in `Cargo.toml`
- [X] T002 [P] SDK models: catalog / policy / limits types in `S/src/models.rs`
- [X] T003 [P] SDK events: `UsageEvent`, `TurnAuditEvent`, `TurnMutationAuditEvent`, `MiniChatAuditEvent` in `S/src/events.rs`
- [X] T004 [P] SDK errors + plugin client traits in `S/src/error.rs`, `S/src/plugin_api.rs`
- [X] T005 [P] SDK GTS plugin specs + resource type ids in `S/src/gts.rs`; unit tests in `S/src/models_tests.rs`
- [X] T006 Add `mini-chat-sdk`, `schemars`, `oagw-sdk` features, `time`, `uuid` (v4), `sea-orm` deps to `G/Cargo.toml`; create module skeleton `G/src/{lib.rs,gear.rs,config.rs,domain/mod.rs,infra/mod.rs,api/mod.rs}`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: config, persistence, errors, authz, policy gateway, outbox, provider plumbing, plugin
gears, gear wiring. No user story can work without these.

### Tests first

- [X] T007 [P] Config parsing/validation tests (defaults of Appendix B, unknown-key rejection, range checks, `${VAR}` expansion, provider validation) in `G/src/config_tests.rs`
- [X] T008 [P] Credit arithmetic tests (per-component ceil, bounds, overflow, zero multiplier) in `G/src/domain/credits_tests.rs`
- [X] T009 [P] Sanitizer tests (provider ids `resp_`, `chatcmpl-`, `cmpl-`, `msg_`, `file-`/`file_`/`assistant-`/`vs_` ≥12 chars, URLs, `sk-` keys, `Bearer` tokens; `file-based`, `file_search` untouched) in `G/src/domain/sanitize_tests.rs`
- [X] T010 [P] Provider SSE parser tests (event line vs `data.type`, multi-line data, CRLF, chunk boundaries) in `G/src/infra/llm/sse_parser_tests.rs`
- [X] T011 [P] Error mapping tests (every DomainError → category/status/reason/resource_type per ADR-0004) in `G/src/domain/error_tests.rs`

### Implementation

- [X] T012 Gear configuration `MiniChatConfig` with all sections/defaults/validation (Appendix B) in `G/src/config.rs`
- [X] T013 [P] Credit arithmetic `credits_micro_checked`, `ceil_div` in `G/src/domain/credits.rs`
- [X] T014 [P] Sanitizer in `G/src/domain/sanitize.rs`
- [X] T015 [P] DomainError + `From<DomainError> for CanonicalError` (resource_error scopes for chat/message/turn/attachment/model) in `G/src/domain/error.rs`
- [X] T016 SeaORM entities (chats, messages, chat_turns, attachments, message_attachments, thread_summaries, chat_vector_stores, quota_usage, message_reactions) with `Scopable` in `G/src/infra/db/entity/*.rs`
- [X] T017 Migrations (PG + SQLite DDL, indexes, CHECKs of DESIGN §3.7) + outbox migrations in `G/src/infra/db/migrations.rs`
- [X] T018 PEP helper (PolicyEnforcer → AccessScope + owner predicate; 403/503 mapping) in `G/src/domain/authz.rs`
- [X] T019 Policy gateway (types-registry selection, `snapshot()`, `snapshot_version()`, `user_limits()`, `resolve_chat_model()`) in `G/src/infra/policy_gateway.rs`
- [X] T020 [P] Audit gateway (lookup per delivery when absent, cache when found, reset when client missing) in `G/src/infra/audit_gateway.rs`
- [X] T021 Outbox enqueuer (queue names, partitions, payload type, `PayloadTooLarge` mapping) and pipeline builder in `G/src/infra/outbox.rs`
- [X] T022 Provider SSE parser in `G/src/infra/llm/sse_parser.rs`
- [X] T023 Provider resolver (provider entries, tenant overrides, alias overrides, storage dispatch, path building) in `G/src/infra/llm/resolver.rs`
- [X] T024 OAGW provisioning (S2S exchange, upstreams/routes per entry and override, AlreadyExists reuse, deferred reconcile loop) in `G/src/infra/oagw_provision.rs`
- [X] T025 [P] Static model-policy plugin gear `static-mini-chat-model-policy-plugin` (config: vendor, priority, model_catalog, kill_switches, default limits; multiplier validation; `publish_usage` logs) in `G/src/infra/plugins/static_model_policy/`
- [X] T026 [P] Static audit plugin gear `static-mini-chat-audit-plugin` (vendor, priority, enabled) in `G/src/infra/plugins/static_audit/`
- [X] T027 Gear wiring `#[toolkit::gear(name="mini-chat", ...)]`: init (config, db, gateways, services, outbox start), `register_rest`, `serve` (provisioning, workers), migrations in `G/src/gear.rs`
- [X] T028 Test support: fake OAGW gateway (scripted responses, request log), fake policy plugin, authz fakes (allow / deny / fail), in-memory SQLite harness building the router in `G/src/test_support.rs`
- [X] T029 Build gate: `cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`

**Checkpoint**: server boots with `config/mini-chat.yaml`, migrations applied, plugins registered.

---

## Phase 3: User Story 1 — Private chat with streamed answers (P1) 🎯 MVP

**Goal**: chat CRUD, message list, `messages:stream` with the full SSE contract, persistence,
models API needed for chat creation.

**Independent Test**: create chat → stream → list messages → rename → delete (router tests + black-box).

### Tests for User Story 1

- [X] T030 [P] [US1] Router tests: create/get/list/update/delete chats, title/model validation, `Location`, title omission, 404 masking, non-UUID path, JSON rejections, ordering by activity, `$filter`/`$orderby`/cursor/limit validation in `G/src/api/rest/chats_tests.rs`
- [X] T031 [P] [US1] Router tests: messages list (contract fields, reactions null, attachments array, filter/order/pagination, message_count, chronological order) in `G/src/api/rest/messages_tests.rs`
- [X] T032 [P] [US1] Router tests: stream happy path (event order, request_id correlation, deltas relayed one by one, done payload, persisted assistant message, provider request body: model/instructions/input/max_output_tokens/user/metadata/stream) and SSE error mapping (`provider_error` sanitized, `rate_limited`, `provider_timeout`, `response.failed`, `response.incomplete`, empty content) in `G/src/api/rest/stream_tests.rs`
- [X] T033 [P] [US1] Unit tests: OpenAI Responses adapter request building + event translation (deltas, tools, citations/annotations, usage, failed/incomplete/error) in `G/src/infra/llm/openai_responses_tests.rs`
- [X] T034 [P] [US1] Black-box suite scaffolding (mock provider, server launcher by pid, config writer, SSE client, DB reader) + chat/stream tests in `BB/conftest.py`, `BB/mock_provider.py`, `BB/test_chats_and_stream.py`

### Implementation for User Story 1

- [X] T035 [US1] Repositories for chats/messages/reactions/turns read paths in `G/src/infra/db/repo/*.rs`
- [X] T036 [US1] Chat service (create/get/list/update/delete with outbox chat-cleanup enqueue, list messages with attachments + my_reaction batch enrichment) in `G/src/domain/chat_service.rs`
- [X] T037 [US1] DTOs mirroring `docs/api/api.json` in `G/src/api/rest/dto.rs`
- [X] T038 [US1] OpenAI Responses adapter (request body, tools, include, extra_body merge, event mapping) + minimal Chat Completions / vLLM / Anthropic adapters in `G/src/infra/llm/{openai_responses,chat_completions,vllm_responses,anthropic_messages}.rs`
- [X] T039 [US1] Token estimation + context assembly (system prompt + guards, summary preamble, recent messages with snapshot boundary, truncation, budget checks) in `G/src/domain/{estimate,context}.rs`
- [X] T040 [US1] Stream service: preflight, reserve transaction (quota reserve + user message + attachments + turn), provider task with bounded channel, cancellation, tool limits, finalization transaction (assistant message, CAS, settlement, usage + audit outbox, thread-summary trigger), terminal emission guard in `G/src/domain/stream/*.rs`
- [X] T041 [US1] SSE writer (ping before first content, keep-alive comments, `stream_interrupted` synthesis, close after terminal) in `G/src/api/rest/sse.rs`
- [X] T042 [US1] Routes + handlers for chats, messages, stream (spawned setup task) in `G/src/api/rest/{routes,handlers}.rs`

**Checkpoint**: US1 tests pass.

---

## Phase 4: User Story 2 — Reliable turns (P1)

### Tests for User Story 2

- [X] T043 [P] [US2] Router tests: replay (no provider call, no quota/outbox change, replay events), `request_id_conflict` for running/failed/cancelled/deleted turns, replay before parallel guard, `turn_already_running`, new turn after terminal, turn status endpoint mapping (`done`/`error`/`cancelled`/`running`, error_code, assistant_message_id, 404 deleted) in `G/src/api/rest/turns_tests.rs`
- [X] T044 [P] [US2] Tests: cancellation (client drop → `cancelled`, partial content persisted, null content when empty, estimated settlement, aborted usage event) and orphan watchdog (stale scan, CAS with progress re-check, estimated settlement, usage event `aborted`, NULL reserve skip) in `G/src/domain/stream/lifecycle_tests.rs` and `G/src/domain/workers/orphan_watchdog_tests.rs`
- [X] T045 [P] [US2] Black-box: replay, 409s, turn status, disconnect → cancelled in `BB/test_turns.py`

### Implementation for User Story 2

- [X] T046 [US2] Replay path (separate code path without settlement access) in `G/src/domain/stream/replay.rs`
- [X] T047 [US2] Turn status read + handler in `G/src/domain/chat_service.rs`, `G/src/api/rest/handlers.rs`
- [X] T048 [US2] Orphan watchdog worker (leader elector no-op / k8s) in `G/src/domain/workers/orphan_watchdog.rs`

---

## Phase 5: User Story 3 — Retry / edit / delete last turn (P2)

### Tests for User Story 3

- [X] T049 [P] [US3] Router tests: NOT_LATEST_TURN, running → 400 turn_state, other requester → 403, retry/edit new request_id + replaced_by_request_id, edit EMPTY_CONTENT, delete 204, quota/kill-switch rejection leaves previous turn, concurrent mutation → GENERATION_IN_PROGRESS/NOT_LATEST_TURN, attachments + web_search flag carried forward, summary invalidation, mutation audit events, setup failure marks turn failed in `G/src/api/rest/mutations_tests.rs`
- [X] T050 [P] [US3] Black-box mutations in `BB/test_mutations.py`

### Implementation for User Story 3

- [X] T051 [US3] Turn service (preview, commit transaction with CAS-safe insert, delete, summary invalidation, audit enqueue) in `G/src/domain/turn_service.rs`
- [X] T052 [US3] Retry/edit streaming via the shared stream pipeline (mutation preflight, post-commit setup, unstarted-turn failure) in `G/src/domain/stream/service.rs`; routes in `G/src/api/rest/routes.rs`

---

## Phase 6: User Story 4 — Documents and images (P2)

### Tests for User Story 4

- [X] T053 [P] [US4] Router tests: upload validation (boundary, multipart, missing file/content type, MIME, size, XLSX without code interpreter, disable_images, document/storage limits, INVALID_MODEL), document indexing ready/uploaded/failed, image thumbnail, get/delete (404 foreign uploader, 409 attachment_locked, 204 idempotent, cleanup outbox payload), attachment_ids validation, provider tools (file_search vector store, code_interpreter file ids, input_image), image guards (TOO_MANY_IMAGES, VISION_NOT_SUPPORTED, FEATURE_DISABLED), citations mapping in `G/src/api/rest/attachments_tests.rs`
- [X] T054 [P] [US4] Tests: thumbnail generation limits in `G/src/domain/thumbnail_tests.rs`; upload reaper + background indexing + cleanup handlers in `G/src/domain/workers/cleanup_tests.rs`
- [X] T055 [P] [US4] Black-box attachments in `BB/test_attachments.py`

### Implementation for User Story 4

- [X] T056 [US4] File / vector-store clients (openai / azure prefixes, multipart upload, polling) in `G/src/infra/llm/storage.rs`
- [X] T057 [US4] Attachment service (upload pipeline, vector-store creation protocol, background indexing, get, delete) + thumbnails in `G/src/domain/{attachment_service,thumbnail}.rs`
- [X] T058 [US4] Upload reaper, attachment cleanup handler, chat cleanup handler in `G/src/domain/workers/{upload_reaper,cleanup}.rs`
- [X] T059 [US4] Upload/get/delete attachment routes in `G/src/api/rest/routes.rs`

---

## Phase 7: User Story 5 — Usage limits (P2)

### Tests for User Story 5

- [X] T060 [P] [US5] Unit tests: estimation, candidate reserve, cascade (premium→standard, kill switches, model_disabled, missing model, no enabled model, overflow → unavailable), daily tool quotas, settlement math (actual, overshoot within/over tolerance, estimated, released), billing outcome derivation table, quota warnings in `G/src/domain/quota_tests.rs`, `G/src/domain/billing_tests.rs`
- [X] T061 [P] [US5] Router tests: 429 with quota scope tokens/web_search/code_interpreter, downgrade metadata in done, reserve rows during stream, settlement rows after done, exactly one usage outbox event per turn, web search tool/guard/citations/calls_exceeded, quota status endpoint in `G/src/api/rest/quota_tests.rs`
- [X] T062 [P] [US5] Black-box quota in `BB/test_quota.py`

### Implementation for User Story 5

- [X] T063 [US5] Quota service (period starts, bucket reads, cascade, reserve + re-check, settle, status, warnings) in `G/src/domain/quota.rs`; billing derivation in `G/src/domain/billing.rs`
- [X] T064 [US5] Usage outbox handler (publish via policy plugin) and audit outbox handler in `G/src/domain/workers/{usage_handler,audit_handler}.rs`

---

## Phase 8: User Story 6 — Models, reactions, quota status (P3)

- [X] T065 [P] [US6] Router tests: models list/get (enabled only, no internal fields, 404 disabled), reactions (assistant only, idempotent PUT/DELETE, INVALID_REACTION before authz, 404 message) in `G/src/api/rest/models_reactions_tests.rs`
- [X] T066 [US6] Models, reactions, quota status handlers in `G/src/api/rest/handlers.rs`, services in `G/src/domain/chat_service.rs`

---

## Phase 9: User Story 7 — Long conversations (P3)

- [X] T067 [P] [US7] Tests: context truncation determinism, summary preamble, trigger (proactive/urgent/not_needed), handler (range selection, prompt format, response parsing, CAS commit, frontier_deleted, empty_summary retry, max_attempts reject, model_unavailable reject, system usage event) in `G/src/domain/context_tests.rs`, `G/src/domain/workers/thread_summary_tests.rs`
- [X] T068 [US7] Thread summary trigger in finalization + outbox handler in `G/src/domain/workers/thread_summary.rs`
- [X] T069 [P] [US7] Black-box thread summary on tiny-context model in `BB/test_thread_summary.py`

---

## Phase 10: Polish & Cross-Cutting Concerns

- [X] T070 Authorization cross-checks: foreign tenant/user on every route (404), PDP deny (403), PDP failure (503 + Retry-After) in `G/src/api/rest/authz_tests.rs`
- [X] T071 Traceability table: each acceptance-criteria item → tests in `specs/001-mini-chat-gear/traceability.md`
- [X] T072 Run full verification: build gate, `cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat`, black-box suite (quickstart.md)

---

## Dependencies & Execution Order

- Phase 1 → Phase 2 (blocking) → US1 → US2 (needs stream pipeline) → US3, US4, US5 (parallel-capable,
  all extend the stream pipeline) → US6, US7 → Polish.
- Within each story: tests → repositories/models → services → handlers/routes.

## Parallel Opportunities

- T002–T005, T007–T011, T013–T015, T020, T025–T026 touch distinct files.
- Story test tasks marked [P] are independent files.
- Black-box test files per story are independent.

## Implementation Strategy

MVP = Phases 1–3 (create chat + stream + history). Then US2 (reliability), US5 (quota correctness),
US3/US4, US6, US7. Each checkpoint runs the story's router tests and black-box tests.

## Phase 11: Convergence

- [X] T073 Map provider function-tool use (Responses `function_call` items, Chat Completions `tool_calls`, Anthropic `tool_use`) when knowledge search is off to a failed turn with SSE `error{code: "unexpected_tool_use"}` and actual/estimated settlement, with router tests in `gears/mini-chat/mini-chat/src/api/rest/stream_tests.rs` per FR-033 / FR-081 (partial)
- [X] T074 Implement knowledge search (off by default): `search_knowledge` function tool + `knowledge_search.guard`, Azure vector-store retriever via OAGW, agentic loop capped at `max_calls_per_message` retrievals ("search limit reached" output beyond) and `max_calls_per_message + 2` iterations (`agentic_iterations_exceeded`), with tests in `gears/mini-chat/mini-chat/src/api/rest/knowledge_search_tests.rs` per FR-080 (missing)
- [X] T075 Emit the stored `thread_summaries.token_estimate` in `stream_started.thread_summary_applied` (DESIGN §SSE `stream_started`) and assert it in `gears/mini-chat/mini-chat/src/api/rest/summary_tests.rs` per FR-033 (contradicts)
- [X] T076 Make `cf-gears-mini-chat` and `cf-gears-mini-chat-sdk` pass `cargo clippy` under the workspace lints without behaviour changes (all tests stay green) per Constitution technical constraints (contradicts)
