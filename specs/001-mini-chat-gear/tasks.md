---
description: "Task list for the mini-chat gear and SDK"
---

# Tasks: Mini-Chat Gear (multi-tenant AI chat) and its SDK

**Input**: Design documents from `/specs/001-mini-chat-gear/`

**Prerequisites**: plan.md, spec.md, research.md, research-design-notes.md, data-model.md, contracts/

**Tests**: REQUIRED by the spec (FR-017). Every phase lists its test tasks first (TDD): write the test, see it
fail (or not compile), then implement. Gear path prefix `G = gears/mini-chat/mini-chat`, SDK prefix
`S = gears/mini-chat/mini-chat-sdk`. Integration tests live in `G/tests/` (never in `testing/e2e/suites/mini_chat/`).

## Format: `[ID] [P?] [Story] Description`

---

## Phase 1: Setup (Shared Infrastructure)

- [X] T001 Create SDK crate `S/Cargo.toml` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`, deps serde, serde_json, uuid, time, async-trait, thiserror, schemars, gts, toolkit, toolkit-gts) and `S/src/lib.rs`
- [X] T002 Add `gears/mini-chat/mini-chat-sdk` to workspace members and `mini-chat-sdk` to `[workspace.dependencies]` in `Cargo.toml`; add `mini-chat-sdk`, `schemars`, dev-deps (`tower`, `http-body-util`, `types-registry-sdk` test-util) in `G/Cargo.toml`
- [X] T003 Create gear module skeleton (`G/src/lib.rs` with `api`, `config`, `domain`, `gear`, `infra` modules; keep `infra::plugins::{static_audit, static_model_policy}`; force-link `oagw`, `authn_resolver`, `authz_resolver`, `types_registry`)

---

## Phase 2: Foundational (Blocking Prerequisites)

### Tests first
- [X] T004 [P] SDK serde tests for `ModelCatalogEntry` defaults/required fields, `KillSwitches` (all fields required), `UsageEvent` JSON shape (omitted `user_id`/`turn_id` for system tasks) in `S/src/models_tests.rs`
- [X] T005 [P] Config parsing/validation tests (defaults from contracts/config.md, deny_unknown_fields, ranges, `${VAR}` expansion, config/mini-chat.yaml gear section parses) in `G/src/config_tests.rs`
- [X] T006 [P] Pure arithmetic tests: `credits_micro` (per-component ceil, bounds, zero multiplier), text token estimate, period starts / next reset, warning/exhausted percentages in `G/src/domain/quota_math_tests.rs`
- [X] T007 [P] Sanitizer tests (`resp_`, `chatcmpl-`, `msg_`, `file-…`≥12, `vs_`, URLs, `sk-`, Bearer; keeps `file-based`, `file_search`) in `G/src/domain/sanitize_tests.rs`
- [X] T008 [P] Migration test: all tables/indexes created on SQLite, UUID columns bound as 16-byte BLOBs, unique indexes (running-turn, chat_id+request_id) enforced in `G/src/infra/db/migrations_tests.rs`

### Implementation
- [X] T009 [P] SDK models: catalog, estimation budgets, general config, kill switches, policy snapshot/version, user/tier limits, usage event, audit events, tool calls in `S/src/models.rs`
- [X] T010 [P] SDK plugin APIs `MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1` + errors in `S/src/plugin_api.rs`; GTS specs `MiniChatModelPolicyPluginSpecV1`, `MiniChatAuditPluginSpecV1` in `S/src/gts.rs`
- [X] T011 Gear config `MiniChatConfig` with all sections + `validate()` in `G/src/config.rs`
- [X] T012 [P] Quota math (`credits_micro_checked`, estimation, periods, warnings) in `G/src/domain/quota_math.rs`
- [X] T013 [P] Sanitizer in `G/src/domain/sanitize.rs`
- [X] T014 Migrations (chats, messages, chat_turns, attachments, message_attachments, thread_summaries, chat_vector_stores, quota_usage, message_reactions; raw SQL per backend) in `G/src/infra/db/migrations.rs`
- [X] T015 SeaORM entities with `Scopable` in `G/src/infra/db/entities.rs`
- [X] T016 Domain error enum + `From<DomainError> for CanonicalError` mapping (resource types, reasons, Retry-After) in `G/src/domain/error.rs`
- [X] T017 Static model policy plugin gear (`static-mini-chat-model-policy-plugin`, config, validation, snapshot v1, limits, publish_usage logs) in `G/src/infra/plugins/static_model_policy.rs`; static audit plugin gear in `G/src/infra/plugins/static_audit.rs`
- [X] T018 Plugin gateways (lazy types-registry resolution by vendor/priority, not-found not cached) in `G/src/infra/plugin_gateway.rs`
- [X] T019 AuthZ PEP helper (resource types, actions, owner filter, error mapping) in `G/src/domain/authz.rs`
- [X] T020 Test harness: in-memory SQLite + migrations, mock AuthZ (allow/deny/fail), static policy plugin, fake provider transport (scripted Responses SSE + recorded requests), router builder in `G/tests/common/mod.rs`
- [X] T021 Gear wiring (`#[toolkit::gear]`, init/serve/stop, migrations incl. outbox, REST registration) in `G/src/gear.rs`

**Checkpoint**: crate compiles, server builds, foundation tests pass.

---

## Phase 3: User Story 1 - Manage chats (P1)

**Goal**: chat CRUD + list with OData. **Independent test**: REST CRUD via router.

### Tests first
- [X] T022 [P] [US1] Chat CRUD tests: create (default model, explicit model, INVALID_MODEL unknown/disabled, INVALID_TITLE whitespace/256 chars, trim, Location header, title omitted when null), get, patch (unknown fields ignored, model unchanged, 422 missing title), delete (204 then 404, sub-resources 404) in `G/tests/chats.rs`
- [X] T023 [P] [US1] List tests: ordering updated_at desc, $filter contains(title), $orderby, cursor pagination, limit clamp 100, limit=0 INVALID_LIMIT, bad cursor INVALID_CURSOR, unknown field → 400 with OData resource type in `G/tests/chats.rs`
- [X] T024 [P] [US1] Isolation tests: other user / other tenant chat → 404; PDP deny → 403 AUTHZ_DENIED; PDP failure → 503 Retry-After 5 in `G/tests/authz.rs`

### Implementation
- [X] T025 [US1] Chat repository (insert, get scoped, list via paginate_odata, rename, soft delete + cleanup enqueue) in `G/src/infra/db/repo.rs`
- [X] T026 [US1] Chat service (validation order, default model algorithm, PEP) in `G/src/domain/service/chats.rs`
- [X] T027 [US1] DTOs (contracts/openapi-mini-chat.json shapes) in `G/src/api/dto.rs`
- [X] T028 [US1] Chat routes + handlers (throttling binding on create, license feature, error sets) in `G/src/api/routes.rs`, `G/src/api/handlers.rs`

---

## Phase 4: User Story 2 - Send message and stream (P1)

**Goal**: SSE streaming turn with quota, context, finalization. **Independent test**: fake provider stream.

### Tests first
- [X] T029 [P] [US2] Streaming tests: event order stream_started→delta…→done, request_id correlation (client + generated), deltas forwarded before provider finishes (gated fake), assistant message + usage persisted, done payload (usage, effective/selected model, quota_decision, quota_warnings) without message id in `G/tests/streaming.rs`
- [X] T030 [P] [US2] Preflight tests (no provider call recorded): EMPTY_CONTENT, invalid/duplicate/foreign/not-ready attachment_ids, too many ids, TOO_MANY_IMAGES, INPUT_TOO_LONG, CONTEXT_BUDGET_EXCEEDED, kill switches web_search/images, VISION_NOT_SUPPORTED after downgrade, quota 429 before provider call, chat model removed → INVALID_MODEL in `G/tests/preflight.rs`
- [X] T031 [P] [US2] Idempotency & parallel tests: replay (is_new_turn false, one delta, done, no provider call, quota rows unchanged, no outbox), request_id_conflict for failed/cancelled/deleted, running turn 409 turn_already_running, replay checked before running guard, new turn accepted after terminal in `G/tests/idempotency.rs`
- [X] T032 [P] [US2] Provider error tests: response.failed → SSE error provider_error sanitized, HTTP 429 → rate_limited, stream end without terminal → provider_error, error event terminal (nothing after), turn failed + settlement estimated/actual in `G/tests/streaming.rs`
- [X] T033 [P] [US2] Unit tests for OpenAI Responses SSE translation (delta, tool start/done for file_search/web_search/code_interpreter, citations mapping, completed/incomplete/failed/error) and request builder (model, instructions, input, tools, max_output_tokens, max_tool_calls, user, metadata, stream) in `G/src/infra/llm/responses_tests.rs`
- [X] T034 [P] [US2] Context assembly unit tests (order, summary preamble as user msg, truncation drops oldest whole turns, summary dropped when not fitting, mandatory overflow error, guard texts, messages_truncated) in `G/src/domain/context_tests.rs`
- [X] T035 [P] [US2] Cancellation test: client drop mid-stream → turn cancelled, partial text persisted, estimated settlement, no error event in `G/tests/streaming.rs`

### Implementation
- [X] T036 [US2] Provider config/resolver + OAGW transport (proxy_request, error classification) in `G/src/infra/llm/provider.rs`, `G/src/infra/llm/oagw_transport.rs`
- [X] T037 [US2] OpenAI Responses adapter (request builder, SSE parser → internal events, summary non-streaming call) in `G/src/infra/llm/responses.rs`
- [X] T038 [US2] Quota service (snapshot+limits fetch, cascade, daily tool quotas, reserve txn with re-check, settlement, warnings, status) in `G/src/domain/service/quota.rs`
- [X] T039 [US2] Context assembly in `G/src/domain/context.rs`
- [X] T040 [US2] Stream service: setup pipeline (normative order), reserve txn (user msg, attachments, turn), provider task with bounded channel, ping, tool limits, progress heartbeat, cancellation in `G/src/domain/service/stream.rs`
- [X] T041 [US2] Finalization (CAS, assistant message, settlement, usage + audit outbox, thread-summary trigger) in `G/src/domain/service/finalize.rs`
- [X] T042 [US2] Replay path (separate, read-only) in `G/src/domain/service/stream.rs`
- [X] T043 [US2] SSE encoding + stream handler (pre-stream errors as Problem; spawned setup) in `G/src/api/sse.rs`, `G/src/api/handlers.rs`
- [X] T044 [US2] OAGW provisioning at start (S2S, upstream/route create, AlreadyExists reuse, deferred retry) in `G/src/infra/oagw_provisioning.rs`

---

## Phase 5: User Story 3 - History, turn status, reactions (P1)

### Tests first
- [X] T045 [P] [US3] Message list tests: chronological default, $filter role/id, $orderby, pagination, message fields (request_id, attachments array always, my_reaction always, model/tokens omitted when absent/0), message_count across turns in `G/tests/messages.rs`
- [X] T046 [P] [US3] Turn status tests: running/done/error/cancelled mapping, error_code, assistant_message_id, deleted turn 404 in `G/tests/turns.rs`
- [X] T047 [P] [US3] Reaction tests: like/dislike upsert, response body, delete idempotent 204, user message 400 reaction_target, invalid value 400 INVALID_REACTION (before authz), missing field 422, unknown message 404, my_reaction visible in list in `G/tests/reactions.rs`

### Implementation
- [X] T048 [US3] Message listing (paginate_odata, batch attachments + reactions) in `G/src/domain/service/messages.rs`
- [X] T049 [US3] Turn status + reactions services and routes in `G/src/domain/service/turns.rs`, `G/src/domain/service/reactions.rs`, `G/src/api/routes.rs`

---

## Phase 6: User Story 4 - Retry / edit / delete last turn (P2)

### Tests first
- [X] T050 [P] [US4] Mutation tests: only latest terminal turn (running 400 turn_state, not latest 409 NOT_LATEST_TURN, deleted 409), new server request_id in stream_started, previous turn soft-deleted after preflight, quota rejection leaves previous turn intact, attachments carried (deleted excluded), web_search flag reused, updated_at bumped, delete 204, concurrent mutation one wins / other 409, foreign requester 403, summary invalidated in `G/tests/mutations.rs`

### Implementation
- [X] T051 [US4] Turn mutation service (preview, preflight, commit txn, setup failure marks turn failed, audit events) in `G/src/domain/service/turns.rs` and routes in `G/src/api/routes.rs`

---

## Phase 7: User Story 5 - Attachments (P2)

### Tests first
- [X] T052 [P] [US5] Upload tests with fake storage: document ready (vector store created once, file added, polled), image ready + thumbnail, XLSX code interpreter (kill switch/capability rejection), size limits FILE_TOO_LARGE, unsupported type, doc count 429 document_limit, storage 429 storage_limit, provider failure 503 + row failed, indexing still running → uploaded then background ready, multipart errors in `G/tests/attachments.rs`
- [X] T053 [P] [US5] Attachment get/delete tests: 204 + idempotent, referenced → 409 attachment_locked, other uploader 404, outbox cleanup enqueued; attachment cleanup handler (2xx/404 done, failures retry → failed at max attempts) and upload reaper (abandoned rows failed + cleanup) in `G/tests/attachments.rs`
- [X] T054 [P] [US5] Tool availability tests: file_search tool sent only with ready docs (vector_store_ids, max_num_results), images as input_image file ids, code_interpreter container file ids, citations map provider file id → attachment id/filename in `G/tests/tools.rs`

### Implementation
- [X] T055 [US5] File storage / vector store clients (OpenAI + Azure paths, multipart upload) in `G/src/infra/llm/storage.rs`
- [X] T056 [US5] Attachment service (validation, purposes, limits, vector store protocol, indexing poll + background task, thumbnail) in `G/src/domain/service/attachments.rs`
- [X] T057 [US5] Upload/get/delete handlers (multipart parsing before/after model check) in `G/src/api/handlers.rs`
- [X] T058 [US5] Upload reaper + attachment cleanup handler in `G/src/infra/workers.rs`, `G/src/infra/outbox.rs`

---

## Phase 8: User Story 6 - Models and quota (P2)

### Tests first
- [X] T059 [P] [US6] Models API tests: enabled only, no internal fields, description omitted when empty, get disabled → 404 model type in `G/tests/models_quota.rs`
- [X] T060 [P] [US6] Quota tests: status tiers premium/total, used = spent+reserved, warning/exhausted, next_reset; premium exhausted → downgrade (quota_decision downgrade, downgrade_from, premium_quota_exhausted, messages.model = effective); all exhausted → 429 tokens; credits per model multipliers; reserve re-check; settlement exactly once per terminal outcome (completed actual, failed estimated, cancelled estimated), overshoot cap; usage outbox payload + dedupe key; orphan watchdog finalizes stale turns in `G/tests/models_quota.rs`, `G/tests/settlement.rs`

### Implementation
- [X] T061 [US6] Models service + routes; quota status route in `G/src/domain/service/models.rs`, `G/src/api/routes.rs`
- [X] T062 [US6] Usage + audit outbox handlers (publish via plugin, attempts bound) in `G/src/infra/outbox.rs`
- [X] T063 [US6] Orphan watchdog in `G/src/infra/workers.rs`

---

## Phase 9: User Story 7 - Web search, thread summary, cleanup (P3)

### Tests first
- [X] T064 [P] [US7] Web search tests: tool + guard sent when enabled & supported, not when unsupported, kill switch 400, daily quota 429 web_search, per-turn limit → SSE error web_search_calls_exceeded, web citations relayed, web_search_calls accounted in quota_usage in `G/tests/web_search.rs`
- [X] T065 [P] [US7] Thread summary tests: trigger (threshold, truncation) enqueues task, handler summarizes range, CAS frontier, is_compressed, system usage event, provider failure keeps previous summary, mutation invalidates summary, summary used in next context with thread_summary_applied in `G/tests/summary.rs`
- [X] T066 [P] [US7] Chat deletion cleanup tests: soft delete enqueues cleanup, handler deletes files then vector store (404 ok), retries on failure in `G/tests/cleanup.rs`

### Implementation
- [X] T067 [US7] Thread summary handler in `G/src/domain/service/summary.rs`
- [X] T068 [US7] Chat cleanup handler in `G/src/infra/outbox.rs`

---

## Phase 10: Polish & Cross-Cutting

- [X] T069 Error contract sweep test: every REST error category/reason from contracts/errors.md asserted at least once (Problem shape, no top-level code) in `G/tests/errors.rs`
- [X] T070 Build the example server with the required features; run `cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat`; manual smoke run per quickstart.md
- [X] T071 Metrics instruments (no-op safe) in `G/src/infra/metrics.rs`

---

## Acceptance-criteria → test traceability

| Acceptance item (acceptance-criteria.md) | Tests |
|---|---|
| Tenant and owner isolation on every resource | T024, T053 (other uploader), T046, T047 |
| Context budget (input message + assembled request) | T030 (INPUT_TOO_LONG, CONTEXT_BUDGET_EXCEEDED), T034 |
| Streaming never buffered | T029 (gated fake provider) |
| Chat model immutable | T022 (PATCH ignores model), T060 (effective vs selected) |
| Quota checked before provider call | T030 (429, no provider request), T060 |
| Chat CRUD lifecycle + model/title validation | T022 |
| List filtering/ordering/pagination + malformed input | T023 |
| Chat ordering reflects activity | T023, T050 (updated_at bump) |
| Messages list filtering/ordering/pagination | T045 |
| Message response contract fields | T045 |
| Message count & chronological ordering | T045 |
| Send-message streams, request id correlation | T029 |
| Preflight before provider call | T030 |
| Assistant message + usage persisted | T029, T060 |
| Full SSE event contract & ordering | T029, T033, T054, T064, T035 (ping covered in T029) |
| Done exposes usage/quota/downgrade w/o internal ids | T029, T060 |
| Error event terminal + sanitized | T032, T007 |
| Replay side-effect free | T031 |
| Conflicting request id reuse rejected | T031 |
| Replay before parallel guard | T031 |
| One turn per chat | T031 |
| New turn after terminal | T031 |
| Retry/edit/delete only latest terminal | T050 |
| Mutation full pipeline + new request id | T050 |
| Concurrent mutations deterministic | T050 |
| Mutations carry attachments & tool usage | T050 |
| Turn state machine | T046, T032, T035 |
| Partial / null content on cancel/failure | T035, T032 |
| Attachment upload/get/delete + limits | T052, T053 |
| Async indexing incl. failure/timeout | T052 |
| Attachments available to provider tools | T054 |
| Cleanup & abandoned upload recovery | T053, T066 |
| Models API | T059 |
| Reactions | T047 |
| Quota status accurate | T060 |
| Reserve-before-execute | T060, T030 |
| Tier downgrade | T060 |
| Credits per model/tier | T006, T060 |
| Settle exactly once | T060 |
| Usage published once | T060 |
| Context assembly deterministic within budget | T034 |
| Tool availability & guidance | T034, T054, T064 |
| Canonical error contract REST + SSE | T069, T032 |
| Provider error sanitization | T007, T032 |
| Web search reported/cited/accounted/limited | T064 |
| Chat deletion cleanup | T066 |
| Thread summary lifecycle | T065 |
| Authorization scoped to owner | T024 |

## Dependencies & Execution Order

- Phase 1 → Phase 2 (blocking) → US1 → US2 → US3 → US4/US5/US6 → US7 → Polish.
- US4 depends on US2 (stream pipeline); US5 tool tests depend on US2; US6 settlement tests depend on US2.
- [P] tasks within a phase touch different files and can run in parallel.

## Parallel Example (Phase 2 tests)

T004, T005, T006, T007, T008 can be written in parallel (distinct files).

## Implementation Strategy

MVP = Phases 1–4 (chats + streaming); then history/reactions, mutations, attachments, quota APIs, web
search/summary/cleanup; finally the error sweep and server build verification.

## Phase 11: Convergence

- [X] T072 Make `cargo clippy -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat --all-targets` pass under the workspace lint set (pedantic deny) without behavior changes per plan: platform conventions / Constitution III (partial)
- [X] T073 Add an integration test for the urgent thread-summary trigger (context truncation) and oldest-whole-turn dropping in the provider request in `G/tests/summary.rs` per US7/AC2, FR-013 (partial)
- [X] T074 Re-run the quickstart live smoke (example server + mock provider, server started/stopped by pid) after the SQLite write-lock and metrics changes per SC-006 (partial)
