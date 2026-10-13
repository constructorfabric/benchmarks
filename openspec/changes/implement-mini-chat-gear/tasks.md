## 1. Workspace and SDK crate

- [x] 1.1 Create `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), add it to workspace members and `[workspace.dependencies]`, and make `cf-gears-mini-chat` depend on it
- [x] 1.2 SDK models: `ModelCatalogEntry` (+ `EstimationBudgets`, `ModelGeneralConfig`, `ModelToolSupport`, `ModelApiParams`, `ModelPreference`, `ModelTier`), `KillSwitches`, `PolicySnapshot`, `PolicyVersionInfo`, `TierLimits`, `UserLimits`, `UsageEvent`, `UsageTokens`, audit event types
- [x] 1.3 SDK plugin traits `MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`, GTS plugin specs and error types

## 2. Gear foundation

- [x] 2.1 Gear config (`config.rs`) with all DESIGN Appendix B sections, defaults, `deny_unknown_fields` where required, `${VAR}` expansion and init-time validation
- [x] 2.2 DB migrations (SQLite + PostgreSQL) for all DESIGN §3.7 tables, constraints and indexes; SeaORM entities
- [x] 2.3 Domain error type with canonical Problem mapping (ADR-0004) and SSE error codes
- [x] 2.4 Gear registration (`#[toolkit::gear]` with deps types-registry, authn-resolver, authz-resolver, oagw; capabilities db, rest, stateful), init/start/stop lifecycle
- [x] 2.5 Static model policy plugin and static audit plugin gears; policy and audit gateways resolving plugins via types-registry by vendor
- [x] 2.6 AuthZ PEP helper (chat, model, user-quota resource types; 403 deny / 503 PDP failure) and owner/tenant scoping

## 3. REST API (non-streaming)

- [x] 3.1 Chats CRUD + list with OData filter/order/cursor pagination, `Location` header, title/model validation
- [x] 3.2 Messages list with attachments summaries, `my_reaction`, request_id guard
- [x] 3.3 Models API list/get (enabled only, projection without internal fields)
- [x] 3.4 Reactions PUT/DELETE
- [x] 3.5 Turn status GET
- [x] 3.6 Quota status GET

## 4. Quota and billing

- [x] 4.1 Checked credit arithmetic and token estimation helpers
- [x] 4.2 Quota preflight: cascade, kill switches, per-candidate reserve, web search / code interpreter daily quotas
- [x] 4.3 Reserve write + re-check inside the turn transaction
- [x] 4.4 Settlement (actual / estimated / overshoot cap) and billing outcome derivation
- [x] 4.5 Usage and audit outbox events with dedupe keys; outbox queues and handlers (usage → policy plugin `publish_usage`, audit → audit plugin)

## 5. LLM provider and OAGW

- [x] 5.1 OAGW provisioning of upstreams/routes for every provider entry at start (S2S token via authn-resolver client credentials), with background retry
- [x] 5.2 OpenAI Responses adapter: request builder (instructions, input, tools, include, metadata, user, max_output_tokens, api_params), SSE parser and event translation, error mapping (429, 504/timeout, failed, error)
- [x] 5.3 Files API, vector stores API (create/add/poll/delete) through OAGW for `openai` and `azure` storage kinds
- [x] 5.4 Non-streaming summary call

## 6. Streaming turn pipeline

- [x] 6.1 Context assembly (system prompt + guards, summary, recent messages with snapshot boundary, current message + images) and budget truncation
- [x] 6.2 `messages:stream` handler: preflight order, idempotency/replay, parallel-turn guard, reserve transaction, SSE relay with ping, bounded channel, cancellation
- [x] 6.3 Provider task event handling: deltas, tool events and limits, citations mapping, progress heartbeat, sanitization
- [x] 6.4 Finalization: CAS completed/failed/cancelled, assistant message persistence, settlement + outbox in one transaction, terminal event gating, thread-summary trigger
- [x] 6.5 Turn mutations: retry, edit, delete (preview → preflight → commit → setup → stream), audit events, summary invalidation

## 7. Attachments

- [x] 7.1 Upload: pre-body checks, multipart parsing, MIME inference/allow-list, size limits, per-chat limits, kill switches, concurrency limit
- [x] 7.2 Provider upload, vector store creation protocol, indexing poll with 25 s deadline and background continuation, image thumbnail (WebP)
- [x] 7.3 Get and delete attachment (attachment_locked, idempotent delete with cleanup outbox)
- [x] 7.4 Upload reaper worker

## 8. Background workers and cleanup

- [x] 8.1 Chat cleanup and attachment cleanup outbox handlers
- [x] 8.2 Orphan watchdog
- [x] 8.3 Thread summary outbox handler (range selection, prompt, CAS commit, system usage event)

## 9. Automated tests

- [x] 9.1 Rust unit tests: credits/overflow, estimation, sanitization, SSE parser/translation, cascade decisions, truncation, billing outcome derivation, config validation, citations mapping
- [x] 9.2 Black-box harness `gears/mini-chat/e2e/` (server launcher, config, mock OpenAI provider, SSE client, DB helpers)
- [x] 9.3 Black-box tests: principles (isolation, budget, no buffering, model immutability, quota before outbound)
- [x] 9.4 Black-box tests: chat CRUD, list filter/order/pagination, ordering by activity
- [x] 9.5 Black-box tests: messages API, message contract, counts
- [x] 9.6 Black-box tests: streaming, preflight, persistence, SSE contract, done/error events
- [x] 9.7 Black-box tests: idempotency/replay, parallel turns, turn lifecycle, turn status
- [x] 9.8 Black-box tests: turn mutations (retry/edit/delete, concurrency, carry-forward)
- [x] 9.9 Black-box tests: attachments lifecycle, indexing, tool availability, cleanup/reaper
- [x] 9.10 Black-box tests: models, reactions, quota status, quota enforcement/downgrade/settlement, usage outbox
- [x] 9.11 Black-box tests: context assembly, error mapping/sanitization, web search, chat deletion cleanup, thread summary, authorization
- [x] 9.12 Build the server with the required features and run all tests green

## 10. Review fixes and knowledge search

- [x] 10.1 Knowledge search: `search_knowledge` tool and guard, Azure vector-store retriever, agentic loop with call and iteration limits, `unexpected_tool_use`
- [x] 10.2 Send-path ordering: idempotency check right after loading the chat; image-count limit before the quota preflight
- [x] 10.3 Stored assistant content equals streamed deltas; `usage: null` in usage events when the provider reported none
- [x] 10.4 OData: quoted UUID `id` filters; `$orderby=title` pagination with untitled chats
- [x] 10.5 Smaller fixes: required `model_catalog` in the static policy section, summary-model startup check, pings while the provider stream opens, stable retry/edit image order, `updated_at` bumps, cleanup-guarded upload updates, `cleanup_updated_at`
- [x] 10.6 Tests for every fix (Rust unit tests and black-box tests `test_knowledge_search.py`, `test_review_fixes.py`)
