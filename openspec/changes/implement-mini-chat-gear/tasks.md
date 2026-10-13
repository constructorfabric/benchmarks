## 1. Foundation

- [x] 1.1 Create `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) with catalog/policy/limits/usage/audit types and the two plugin client traits + GTS spec types; add it to workspace members and `[workspace.dependencies]`; make the gear depend on it
- [x] 1.2 Gear `Cargo.toml`: add needed lockfile-listed deps (sdk, toolkit-odata-macros, async-stream/tokio-stream, dev-deps)
- [x] 1.3 `config.rs`: full `MiniChatConfig` (Appendix B keys/defaults, deny_unknown_fields where required, `${VAR}` expansion) with `validate()`
- [x] 1.4 Domain error type and `DomainError → CanonicalError` mapping (resource scopes chat/message/turn/attachment/model, all REST reasons of ADR-0004)
- [x] 1.5 SeaORM entities (Scopable) for the nine tables and migrations (SQLite + Postgres) incl. outbox migrations; monotonic fixed-precision clock helper
- [x] 1.6 Ports: `PolicyPort` (lazy plugin resolution), `AuthzPort` (PolicyEnforcer + owner predicate), `LlmProvider`, `RagStorage`, outbox enqueuer; shared `AppServices` container
- [x] 1.7 Gear registration (`#[toolkit::gear]`, deps incl. oagw), init/serve/stop wiring, bundled plugin gears `static_model_policy` / `static_audit`

## 2. Chats, messages, models, reactions

- [x] 2.1 Chat service + handlers: create (title/model validation, default model, Location), get, list (OData filter/orderby/cursor, limits), patch title, delete (soft-delete + attachment cleanup marks + chat-cleanup outbox)
- [x] 2.2 Message list service + handler (OData, attachments batch join, my_reaction batch, null request_id guard, optional token fields)
- [x] 2.3 Reactions put/delete (validation before authz, assistant-only, upsert, idempotent delete)
- [x] 2.4 Models list/get (enabled only, non-exposure)

## 3. Quota and billing

- [x] 3.1 Pure helpers: credits_micro (checked), text/image/surcharge estimation, period starts/next resets, warnings
- [x] 3.2 Preflight cascade (kill switches, candidate selection, bucket availability per candidate reserve, tool daily quotas) returning a decision
- [x] 3.3 Reserve write with re-check, settlement (actual/estimated/released, overshoot cap, telemetry counters), billing outcome derivation
- [x] 3.4 Quota status endpoint and `done.quota_warnings`
- [x] 3.5 Usage outbox payload (`UsageEvent`, dedupe key) + usage handler publishing to the policy plugin; audit events + audit handler (attempt cap, dropped when no plugin)
- [x] 3.6 Orphan watchdog (stale-progress scan, CAS, estimated settlement, usage+audit outbox)

## 4. LLM provider and OAGW

- [x] 4.1 Provider resolver (provider entry + tenant override → alias, api_path, storage kind), OAGW provisioning at start with background retry
- [x] 4.2 OpenAI Responses request builder (input, instructions, tools, include, user, metadata, api params, extra_body, max_output_tokens, max_tool_calls)
- [x] 4.3 Streaming SSE parser → internal provider events (deltas, tool start/done, annotations, completed/incomplete/failed/error, usage incl. cache/reasoning), HTTP error mapping (429 → rate_limited, timeout → provider_timeout)
- [x] 4.4 Non-streaming call for thread summary; files API and vector store client (openai/azure storage kinds) through OAGW
- [x] 4.5 Provider-id/URL/credential sanitizer

## 5. Streaming turns

- [x] 5.1 Context plan assembly (instructions + guards, summary preamble, recent messages with boundary/frontier, images) and deterministic truncation / budget errors
- [x] 5.2 Send pipeline: validation, idempotency/replay, parallel guard, preflight, image guards, input limit, context, reserve transaction (reserve+recheck, user message, attachment validation, running turn)
- [x] 5.3 Provider task + SSE relay: stream_started, ping before content, delta/tool forwarding, tool call limits, progress heartbeat, citations, cancellation on disconnect
- [x] 5.4 CAS finalization (completed/failed/cancelled, partial content, message persistence failure), settlement + usage/audit/summary outbox in one transaction, terminal event gating
- [x] 5.5 Turn status endpoint
- [x] 5.6 Turn mutations: retry/edit/delete (preview, preflight, mutation commit, summary invalidation, audit events, setup failure marking)

## 6. Attachments

- [x] 6.1 Upload handler (pre-body model resolution, multipart parsing, MIME inference, size limits, purposes, kill switches, per-chat limits, concurrency limit)
- [x] 6.2 Provider upload, vector store lazy creation protocol, indexing poll (25 s) and background indexing task, thumbnails
- [x] 6.3 Get/delete attachment (uploader check, locked check, idempotent delete + outbox)
- [x] 6.4 Attachment cleanup and chat cleanup outbox handlers; upload reaper

## 7. Thread summary

- [x] 7.1 Trigger evaluation and enqueue in finalization; summary handler (range load, prompt, non-streaming call, parse, CAS commit, system usage event, attempts)

## 8. Tests (cover every acceptance-criteria item)

- [x] 8.1 Test harness: in-memory SQLite with migrations, fake policy/authz/provider/storage, axum router helper with injected SecurityContext
- [x] 8.2 Unit tests: credits, estimation, cascade, warnings, truncation, sanitizer, SSE parser, request builder, billing derivation, config validation
- [x] 8.3 Integration tests: chat CRUD/list/ordering, messages contract, reactions, models, quota status, authorization isolation
- [x] 8.4 Integration tests: streaming (events/order/ping/done/error), preflight, idempotency/replay, parallel guard, lifecycle/cancel, settlement, web search, citations
- [x] 8.5 Integration tests: turn mutations, attachments lifecycle/cleanup/reaper, thread summary, orphan watchdog
- [x] 8.6 `ACCEPTANCE_COVERAGE.md` mapping each acceptance item to tests; all tests pass

## 9. Verification

- [x] 9.1 Build the example server with the required features; run the server against a mock OpenAI-compatible provider and smoke-test the main flows
- [x] 9.2 Sync delta specs into `openspec/specs` and verify implementation against them
