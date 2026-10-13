# Tasks: Mini Chat Gear and SDK

**Input**: Design documents from `/specs/001-mini-chat-gear/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: REQUIRED by the spec (FR-017). Each acceptance-criteria (AC) item has a test, and test
tasks come before the implementation tasks of their story (TDD).

Paths: `G` = `gears/mini-chat/mini-chat`, `SDK` = `gears/mini-chat/mini-chat-sdk`.

## Phase 1: Setup

- [X] T001 Create SDK crate `SDK/Cargo.toml` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) and `SDK/src/lib.rs`; add `gears/mini-chat/mini-chat-sdk` to workspace members and `mini-chat-sdk` to `[workspace.dependencies]` in `Cargo.toml`
- [X] T002 Add `mini-chat-sdk`, dev-deps (`tower`, `http-body-util`, `toolkit-db` sqlite) to `G/Cargo.toml`; create the module skeleton `G/src/{lib.rs,gear.rs,config.rs,api,domain,infra}` per plan.md
- [X] T003 [P] Implement SDK models in `SDK/src/models.rs` (ModelCatalogEntry, EstimationBudgets, ModelGeneralConfig, ToolSupport, KillSwitches, PolicySnapshot, UserLimits, TierLimits, ModelTier, UsageEvent, UsageTokens), `SDK/src/audit.rs`, `SDK/src/plugin_api.rs`, `SDK/src/gts.rs`
- [X] T004 [P] SDK unit tests (serde shapes, defaults, KillSwitches strictness) in `SDK/src/models.rs`

## Phase 2: Foundational (blocking)

- [X] T005 Gear config with Appendix B defaults and validation in `G/src/config.rs` (+ unit tests)
- [X] T006 Dual-dialect migrations for all 9 tables in `G/src/infra/db/migrations/` and SeaORM `Scopable` entities in `G/src/infra/db/entities/`
- [X] T007 [P] DomainError taxonomy `G/src/domain/error.rs` and canonical mapping `G/src/api/rest/error.rs` (ADR-0004 table, resource types) + unit tests
- [X] T008 [P] AuthZ helpers (`ResourceType`s chat/model/user_quota, actions, `ensure_owner`, 403/503 mapping) in `G/src/domain/authz.rs`
- [X] T009 [P] Static model-policy plugin gear `G/src/infra/plugins/static_model_policy/` and static audit plugin gear `G/src/infra/plugins/static_audit/`; plugin gateways (`GtsPluginSelector`) in `G/src/infra/policy.rs`
- [X] T010 [P] Credit arithmetic, estimation and period helpers `G/src/domain/credits.rs` + unit tests (DESIGN §5.3–5.5)
- [X] T011 [P] Provider-id sanitizer `G/src/domain/sanitize.rs` + unit tests
- [X] T012 OAGW-backed LLM layer: provider resolver, SSE decoder, `openai_responses` adapter, files and vector stores in `G/src/infra/llm/` + unit tests of the SSE decoder and event translation
- [X] T013 Outbox wiring (queues, enqueue helpers, partitioning) `G/src/infra/outbox.rs`
- [X] T014 Gear lifecycle `G/src/gear.rs` (init: config, db, clients, services; register_rest; start: S2S, OAGW provisioning `G/src/infra/oagw.rs`, outbox, workers; stop)
- [X] T015 Integration test harness `G/tests/common/mod.rs`: in-memory SQLite + migrations, mock PDP (allow / deny / fail), fake OpenAI-compatible `ServiceGatewayClientV1` recording requests, router builder, SSE parser, auth contexts for 2 users × 2 tenants

## Phase 3: US1 – Chats & history (P1)

AC: Chat CRUD (3 items), Messages API (list, response contract), Principles (isolation, model immutable), Authorization.

- [X] T016 [P] [US1] Tests `G/tests/chats.rs`: create default/explicit/invalid model, title rules, Location, get/rename/delete, double delete 404, PATCH ignores model, list ordering/filter/orderby/limit clamp/limit=0/bad cursor/unknown field, `$select` accepted
- [X] T017 [P] [US1] Tests `G/tests/authz.rs`: cross-user and cross-tenant 404 on every chat-scoped endpoint, PDP deny 403, PDP failure 503 + Retry-After, invalid path UUID 400, malformed JSON 400 / schema 422 / content-type 415
- [X] T018 [US1] Chat service + repo (create/list/get/update/delete, message_count, default model algorithm) `G/src/domain/service/chats.rs`
- [X] T019 [US1] Messages list service (OData, attachments batch, my_reaction batch) `G/src/domain/service/messages.rs`
- [X] T020 [US1] REST DTOs + handlers + routes for chats and messages `G/src/api/rest/{dto.rs,handlers/chats.rs,handlers/messages.rs,routes.rs}`

## Phase 4: US2 – Send message & stream (P1)

AC: Streaming Send Message (3), SSE Event Contract (3), Settlement & Finalization (2), Context Assembly (2), Principles (context budget, no buffering, quota before provider), Error Mapping & Sanitization (2).

- [X] T021 [P] [US2] Unit tests for context assembly/truncation and guards in `G/src/domain/context.rs`
- [X] T022 [P] [US2] Unit tests for quota cascade, reserve/settle math, billing derivation, warnings in `G/src/domain/quota.rs` / `G/src/domain/billing.rs`
- [X] T023 [P] [US2] Tests `G/tests/streaming.rs`: event order, request_id echo/generation, persisted rows (messages, chat_turns, quota_usage, outbox usage + audit), chat updated_at bump, preflight validation errors with no provider call, provider error / timeout / 429 / sanitized message, incomplete → done, disconnect → cancelled with partial text + estimated settlement, ping before content, deltas relayed before provider finishes, INPUT_TOO_LONG, CONTEXT_BUDGET_EXCEEDED, provider request body (instructions, history, summary preamble, user, metadata, max_output_tokens)
- [X] T024 [US2] Context plan implementation `G/src/domain/context.rs`
- [X] T025 [US2] Quota service (preflight cascade, tool daily quotas, reserve with recheck, settlement, warnings) `G/src/domain/quota.rs` and billing derivation `G/src/domain/billing.rs`
- [X] T026 [US2] Stream service: validation → idempotency → parallel guard → preflight → context → provider → reserve txn → SSE relay task → CAS finalization (+usage/audit outbox, thread-summary trigger) `G/src/domain/service/stream.rs`
- [X] T027 [US2] SSE handler + route `G/src/api/rest/handlers/stream.rs`, `G/src/api/rest/sse.rs`

## Phase 5: US3 – Replay, parallel guard, turn status (P1)

AC: Idempotency & Replay (3), Parallel Turn Enforcement (2), Turn Lifecycle (2).

- [X] T028 [P] [US3] Tests `G/tests/idempotency.rs`: replay events + no provider call/quota/outbox change, conflicts for running/failed/cancelled/deleted, turn_already_running, replay while another turn runs, new turn accepted after terminal, turn status in all states incl. 404 deleted/foreign, orphan watchdog finalization
- [X] T029 [US3] Replay path + turn status service `G/src/domain/service/stream.rs`, `G/src/domain/service/turns.rs`; handler `G/src/api/rest/handlers/turns.rs`
- [X] T030 [US3] Orphan watchdog + leader elector `G/src/infra/workers.rs`

## Phase 6: US4 – Turn mutations (P2)

AC: Turn Mutations (4), Cleanup & Recovery (summary invalidation).

- [X] T031 [P] [US4] Tests `G/tests/mutations.rs`: retry/edit/delete latest terminal; NOT_LATEST_TURN, running → 400 STATE, GENERATION_IN_PROGRESS race, quota 429 leaves old turn, new request_id + replaced_by_request_id, attachments copied (deleted excluded), images re-sent, summary deleted + uncompressed, audit events, foreign requester 403/404
- [X] T032 [US4] Mutation service (preview, preflight, commit, setup failure marking) `G/src/domain/service/turns.rs` + handlers

## Phase 7: US5 – Attachments (P2)

AC: Attachments (4), Web Search/tools (file_search inclusion), Principles (isolation).

- [X] T033 [P] [US5] Tests `G/tests/attachments.rs`: doc upload ready (files + vector store requests), image upload with thumbnail, xlsx code interpreter + kill switch/capability rejection, size/type/multipart errors, per-chat document & storage 429, provider failure 503 + failed row, indexing failure, uploaded-at-deadline background completion, GET/DELETE lifecycle, 409 attachment_locked, idempotent re-delete, foreign uploader 404, upload reaper, file_search / input_image / code_interpreter in provider request, citations mapping
- [X] T034 [US5] Attachment service (MIME rules, limits, provider upload, vector store protocol, polling, background indexing, thumbnails) `G/src/domain/service/attachments.rs`, `G/src/infra/thumbnail.rs`
- [X] T035 [US5] Upload reaper `G/src/infra/workers.rs`; attachment handlers `G/src/api/rest/handlers/attachments.rs`

## Phase 8: US6 – Models, reactions, quotas, web search (P2)

AC: Models API, Reactions API, Quota Status API, Quota Enforcement (3), Web Search.

- [X] T036 [P] [US6] Tests `G/tests/models_reactions.rs` (models visibility/non-exposure, reactions upsert/delete/idempotent/user-message 400/invalid value 400) and `G/tests/quota.rs` (status shape + consistency with usage, downgrade with reason, model_disabled, all-tier 429, credits per model/tier, web search tool + guard + citations + per-turn limit + daily 429, code interpreter daily 429, kill switches)
- [X] T037 [US6] Models + reactions + quota status services and handlers `G/src/domain/service/{models.rs,reactions.rs,quota_status.rs}`, `G/src/api/rest/handlers/{models.rs,reactions.rs,quota.rs}`

## Phase 9: US7 – Background reliability (P3)

AC: Cleanup & Recovery (2), Settlement & Finalization (published once).

- [X] T038 [P] [US7] Tests `G/tests/cleanup.rs`: chat deletion cleanup handler (files, vector store, cleanup_status, retries, max attempts), attachment cleanup handler, usage handler publishes once, audit handler delivery/drop/reject, thread summary handler (success, provider failure keeps summary, CAS, frontier deleted) and trigger
- [X] T039 [US7] Outbox handlers (usage, audit, attachment cleanup, chat cleanup, thread summary) `G/src/infra/outbox.rs`, `G/src/domain/service/summary.rs`

## Phase 10: Polish

- [X] T040 Build the example server with the required features; fix warnings/clippy-relevant issues
- [X] T041 Smoke run per quickstart.md with a mock provider (`G/tests/smoke/mock_openai.py`)
- [X] T042 Run `cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat`; ensure every AC item maps to a passing test (AC→test map in `G/tests/README.md`)

## Dependencies

Setup → Foundational → US1 → US2 → US3 → (US4, US5, US6 in parallel) → US7 → Polish.
US2 depends on US1 (chats). US3/US4 depend on US2 (stream pipeline). US5 is independent of US3/US4.

## Parallel examples

- Phase 2: T007, T008, T009, T010, T011 touch different files.
- US1: T016 and T017 (test files) in parallel.
- After US3: US4 (T031–T032), US5 (T033–T035), US6 (T036–T037) by different implementers.

## Implementation strategy

MVP = Phases 1–4 (chat CRUD + streaming). Then replay/status, then mutations, attachments,
quotas/models/reactions, then background handlers. Each phase ends with its tests green.

## Phase 11: Convergence

- [X] T043 Add a unit test in `G/src/infra/db/migrations/m0001_initial.rs` asserting the rendered Postgres DDL (UUID/TIMESTAMPTZ/JSONB/BYTEA types, one-running-turn partial unique index, no placeholders left) per FR-014 (partial)
- [X] T044 Add an integration test in `G/tests/chats.rs` for send and upload on a chat whose model was removed from the catalog (400 `INVALID_MODEL`) per Spec Edge Cases (partial)
- [X] T045 Add an integration test in `G/tests/quota.rs` for images on a turn downgraded to a non-vision model (400 `VISION_NOT_SUPPORTED`, no provider call) per Spec Edge Cases (partial)
- [X] T046 Assert the exact schema-mismatch status (422) in `G/tests/authz.rs::invalid_path_and_body_errors` per Spec Edge Cases (partial)
- [X] T047 Add a unit test in `G/src/domain/quota.rs` that periods with limit ≤ 0 are skipped by quota status / warnings per Spec Edge Cases (partial)
- [X] T048 Record the file-backed WAL SQLite test database decision (R9) in `specs/001-mini-chat-gear/research.md` per plan: Testing (partial)
- [X] T049 Split `G/src/api/rest/handlers.rs` into `G/src/api/rest/handlers/{chats,messages,stream,turns,attachments,reactions,models,quota}.rs` plus `G/src/api/rest/sse.rs` per plan: Project Structure (partial)
