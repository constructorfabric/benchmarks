## 1. SDK crate and workspace

- [x] 1.1 Create `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), add it to the workspace members and workspace dependencies, and make the gear depend on it
- [x] 1.2 Define SDK models: `ModelCatalogEntry` (+ `EstimationBudgets`, `ModelGeneralConfig`, api params, features, tool support, endpoints, preference, tier), `KillSwitches`, `PolicySnapshot`, `PolicyVersionInfo`, `TierLimits`, `UserLimits`, `UsageEvent`/`UsageTokens`, audit event types
- [x] 1.3 Define plugin specs and client traits `MiniChatModelPolicyPluginClientV1` and `MiniChatAuditPluginClientV1` (GTS spec types, error enums)
- [x] 1.4 Unit tests for SDK serde defaults and usage-event serialization (system task omission rules)

## 2. Gear skeleton, config and plugins

- [x] 2.1 Gear registration (`mini-chat`, deps types-registry/authn-resolver/authz-resolver/oagw, capabilities db/rest/stateful) with init/start/stop lifecycle
- [x] 2.2 Config structs with Appendix B keys/defaults, unknown-key rejection rules and startup validation (streaming, quota, outbox, context, rag, thumbnail, workers, providers, knowledge_search, estimation_budgets deprecations)
- [x] 2.3 Static model policy plugin gear (catalog, kill switches, default limits, multiplier validation) and static audit plugin gear, registered in types-registry and ClientHub
- [x] 2.4 Policy and audit gateways resolving plugins by vendor
- [x] 2.5 Unit tests for config defaults/validation and static plugin behaviour

## 3. Persistence

- [x] 3.1 Migrations (SQLite + Postgres) for chats, messages, chat_turns, attachments, message_attachments, thread_summaries, chat_vector_stores, quota_usage, message_reactions with constraints and indexes; include outbox migrations
- [x] 3.2 SeaORM entities with Secure ORM scoping
- [x] 3.3 Repositories: chats (OData list/pagination), messages (OData list, recent-messages query, batch attachments/reactions), turns (CAS ops, latest turn, orphan scan), attachments, vector stores, quota usage, reactions, thread summaries

## 4. REST API basics

- [x] 4.1 Error mapping (domain error → canonical Problem per ADR-0004) and authz PEP helper (PDP evaluate → AccessScope, 403/503/404 semantics)
- [x] 4.2 Chats CRUD + list (OData, clamp, cursor) handlers, DTOs and routes with license/auth requirements
- [x] 4.3 Messages list, reactions PUT/DELETE, turn status GET
- [x] 4.4 Models list/get and quota status endpoints

## 5. LLM provider layer

- [x] 5.1 Transport port + OAGW implementation (JSON streaming, JSON, multipart upload, GET/DELETE) and provider resolver (provider_id + tenant overrides → adapter kind, alias, api_path)
- [x] 5.2 OpenAI Responses adapter: request builder (instructions, input, images, tools, include, max_tool_calls, user, metadata, api params/extra_body) and SSE parser to internal events (deltas, tools, annotations/citations, completed/incomplete/failed/error, usage)
- [x] 5.3 Chat Completions, vLLM Responses and Anthropic Messages adapters (request + stream parsing)
- [x] 5.4 Error classification (429 → rate_limited, timeouts → provider_timeout, other → provider_error) and message sanitization
- [x] 5.5 Files / vector store clients for `openai` and `azure` storage kinds
- [x] 5.6 OAGW upstream/route provisioning at start with background retry

## 6. Streaming turn pipeline

- [x] 6.1 Quota service: estimation, checked credits, cascade with kill switches and per-candidate reserve, tool daily quotas, reserve increment + re-check, settlement (actual/estimated/overshoot cap), warnings and status
- [x] 6.2 Context assembly: budget, mandatory items, summary, recent messages truncation by whole turns, tool selection and guards, images
- [x] 6.3 Stream service: validation order, replay/conflict before parallel guard, preflight, reserve transaction (user message, attachments validation, turn insert, chat bump), provider task, SSE relay with ping, bounded channel, cancellation, tool limits, last_progress_at refresh
- [x] 6.4 Finalization service: CAS per outcome, assistant message persistence, billing outcome derivation, settlement, usage + audit outbox, thread-summary trigger, terminal SSE gating, `message_persistence_failed` / `finalization_failed` / `stream_interrupted`
- [x] 6.5 Replay path (separate, side-effect free)

## 7. Turn mutations

- [x] 7.1 Turn service: mutation preview (latest, terminal, ownership), commit transaction (soft-delete old turn + messages, replaced_by, new user message + attachments copy, new running turn, summary invalidation, chat bump, audit), delete
- [x] 7.2 Retry/edit endpoints reusing the stream pipeline with post-commit setup failure marking

## 8. Attachments

- [x] 8.1 Upload handler: pre-body chat/model resolution, multipart streaming with size limit, MIME inference/validation, purposes/kill switch/capability filtering, concurrency limit
- [x] 8.2 Upload service: row insert with per-chat limits, provider upload, vector store get-or-create protocol, indexing poll with 25 s deadline and background continuation, thumbnail generation, failure states
- [x] 8.3 Get/delete attachment with locking check, idempotency and cleanup outbox event

## 9. Background work

- [x] 9.1 Outbox pipeline start/stop with five queues and handlers: usage (publish_usage), audit (bounded retries), attachment cleanup, chat cleanup (attachments then vector store), thread summary (range, prompt, CAS commit, system usage event)
- [x] 9.2 Orphan watchdog (scan, CAS with stale re-check, estimated settlement, aborted usage event)
- [x] 9.3 Upload reaper

## 10. Tests for acceptance criteria

- [x] 10.1 Test harness: router over temp SQLite DB, mock PDP/security contexts for multiple users/tenants, scripted fake LLM transport capturing requests, SSE client helpers
- [x] 10.2 Tests: principles (isolation, context budget, no buffering, model immutability, quota before outbound)
- [x] 10.3 Tests: chat CRUD, list filtering/ordering/pagination/malformed input, ordering by activity
- [x] 10.4 Tests: messages list, message DTO contract, message count/chronology, reactions
- [x] 10.5 Tests: streaming send, preflight validation, persistence on completion, SSE event contract, done payload, error event sanitization
- [x] 10.6 Tests: idempotency/replay, conflicts, replay before parallel guard, parallel turn enforcement
- [x] 10.7 Tests: turn mutations (latest/terminal, pipeline + new request id, concurrency, attachment/tool carry-forward), turn lifecycle (state machine, partial/null content)
- [x] 10.8 Tests: attachments (lifecycle, limits, indexing incl. failure/timeout, tool availability, cleanup and reaper)
- [x] 10.9 Tests: models API, quota status, quota enforcement (reserve, downgrade, credits), settlement and usage publication, web search, context assembly
- [x] 10.10 Tests: error mapping, cleanup and recovery (chat deletion cleanup, thread summary generation/failure/invalidation, orphan watchdog), authorization

## 11. Verification

- [x] 11.1 Build the example server with the required features and run the gear's test suite
- [x] 11.2 Smoke-run the server against `config/mini-chat.yaml`-style config with a local mock provider and exercise the main flows
