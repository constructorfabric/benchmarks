## 1. Workspace and SDK

- [x] 1.1 Create `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), add it to workspace members and `[workspace.dependencies]`, make the gear depend on it
- [x] 1.2 SDK models: `PolicySnapshot`, `ModelCatalogEntry` (+ `EstimationBudgets`, `ModelGeneralConfig`, `ModelApiParams`, `ModelToolSupport`, `ModelPreference`, `ModelTier`), `KillSwitches`, `UserLimits`, `TierLimits`, `UsageEvent`, `UsageTokens`
- [x] 1.3 SDK plugin contracts: `MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`, audit event types, plugin GTS spec types, error enums

## 2. Gear skeleton, config and persistence

- [x] 2.1 Gear config with all DESIGN Appendix B keys, defaults, `deny_unknown_fields` where documented and startup validation
- [x] 2.2 Gear registration (`db`, `rest`, `stateful`), init/start/stop, ClientHub wiring, linking of oagw/authn/authz/types-registry gears
- [x] 2.3 Migrations for all tables (DESIGN §3.7) plus outbox migrations, SQLite and PostgreSQL
- [x] 2.4 SeaORM entities with Secure ORM scoping and repositories (chats, messages, turns, attachments, message_attachments, thread_summaries, chat_vector_stores, quota_usage, message_reactions)
- [x] 2.5 Domain error type and mapping to `CanonicalError` / `Problem` (ADR-0004), resource types, PEP via PolicyEnforcer (fail-closed 403/503)

## 3. Bundled plugins

- [x] 3.1 `static-mini-chat-model-policy-plugin` gear (catalog, kill switches, default limits, multiplier validation, publish_usage logs)
- [x] 3.2 `static-mini-chat-audit-plugin` gear (enabled flag, logs events)
- [x] 3.3 Policy and audit gateways resolving plugins via types-registry by vendor/priority

## 4. REST API (non-streaming)

- [x] 4.1 Chat CRUD with title/model validation, default model, Location header, soft delete + chat-cleanup outbox
- [x] 4.2 Chat list and message list with OData filter/orderby, cursor pagination, limit clamp, error mapping
- [x] 4.3 Message DTO enrichment (attachments, my_reaction, optional model/tokens) and request_id invariant
- [x] 4.4 Reactions PUT/DELETE
- [x] 4.5 Models API and quota status API
- [x] 4.6 Turn status endpoint and turn delete

## 5. Streaming pipeline

- [x] 5.1 Token estimation, credits arithmetic, downgrade cascade, tool quota checks, reserve with re-check
- [x] 5.2 Context assembly (system prompt, guards, summary, recent messages with truncation, images, tools) and budget checks
- [x] 5.3 OAGW provider client and OpenAI Responses adapter (request building, SSE parsing, error mapping); Chat Completions, vLLM and Anthropic adapters
- [x] 5.4 Send-message handler: preflight order, idempotency/replay, parallel guard, reserve transaction, provider task, bounded channel, pings, cancellation on disconnect
- [x] 5.5 Finalization: CAS, assistant message persistence, settlement (actual/estimated/overshoot), usage + audit outbox, thread-summary trigger, done/error gating, quota_warnings
- [x] 5.6 Tool events, citations mapping, per-turn web search / code interpreter limits, error sanitizer
- [x] 5.7 Retry and edit (preview, preflight, mutation commit, setup failure marking, streaming via shared pipeline)

## 6. Attachments

- [x] 6.1 Multipart upload handler with validation order, MIME inference, filename rules, size limits, concurrency limit
- [x] 6.2 Provider Files API upload, vector store creation protocol, indexing poll with 25 s deadline and background continuation, thumbnails
- [x] 6.3 Get / delete attachment with lock check and cleanup outbox

## 7. Background work

- [x] 7.1 OAGW upstream/route provisioning with S2S credentials and background retry
- [x] 7.2 Outbox handlers: usage, audit, attachment cleanup, chat cleanup, thread summary
- [x] 7.3 Orphan watchdog and upload reaper

## 8. Tests

- [x] 8.1 Rust unit tests: credits/estimation, cascade, context truncation, sanitizer, billing outcome derivation, SSE translation, config validation, quota warnings
- [x] 8.2 Python e2e harness (server launcher with own config + mock OpenAI-compatible provider) under `testing/e2e/mini_chat_impl/`
- [x] 8.3 E2E tests: principles & constraints, chat CRUD, messages API, streaming, SSE contract, idempotency & replay, parallel turns
- [x] 8.4 E2E tests: turn mutations, turn lifecycle, attachments, models, reactions, quota status/enforcement/settlement
- [x] 8.5 E2E tests: context assembly, error mapping & sanitization, web search, cleanup & recovery, thread summary, authorization
- [x] 8.6 Build the server with the check features, run all tests, fix failures
