## Why

The `mini-chat` gear (multi-tenant AI chat) ships in `gears/mini-chat/` with its implementation removed: the crate only declares two empty plugin modules, nothing registers the gear or serves its API, and its SDK crate does not exist. The example server (`cf-gears-example-server --features mini-chat,...`) therefore builds without any chat functionality, while `gears/mini-chat/docs/` (PRD, DESIGN, ADRs, acceptance checklist) fully specify the behaviour the platform expects. This change restores the gear so it satisfies that contract end to end.

## What Changes

- Create the SDK crate `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), add it to the workspace and make the gear depend on it. It holds the shared contract types: model catalog (`ModelCatalogEntry`, `EstimationBudgets`, `ModelGeneralConfig`, `ModelToolSupport`, …), `PolicySnapshot`, `KillSwitches`, `UserLimits`/`TierLimits`, `UsageEvent`, audit event types, and the two plugin contracts (`MiniChatModelPolicyPluginClientV1` + spec, `MiniChatAuditPluginClientV1` + spec).
- Implement the `mini-chat` gear in `gears/mini-chat/mini-chat`:
  - Gear registration (`deps = [types_registry, authn_resolver, authz_resolver, oagw]`, capabilities db/rest/stateful), configuration (`gears.mini-chat.config`, keys and defaults of DESIGN Appendix B, unknown keys rejected where specified) and lifecycle (init/start/stop).
  - Persistence: SeaORM entities + migrations (SQLite and Postgres) for `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions`, plus the shared outbox tables.
  - REST/SSE API under `/mini-chat/v1`: chats CRUD + list (OData, cursor pagination), messages list, `messages:stream` (SSE), attachments upload/get/delete, turn status, retry/edit/delete last turn, models list/get, reactions put/delete, quota status.
  - Domain services: PEP authorization via AuthZ resolver (owner-only scoping, fail-closed), streaming relay without buffering (bounded channels, ping, cancellation on disconnect), idempotency/replay, parallel-turn guard, turn state machine with CAS finalization, quota preflight/cascade/reserve/settlement with credit arithmetic, quota warnings, usage + audit events through the transactional outbox, context assembly and truncation, tool assembly (file_search, web_search, code_interpreter), citation mapping, provider-id sanitization, canonical `Problem` error mapping.
  - LLM provider library: OpenAI Responses adapter (request building incl. `user`/`metadata`/tools/multimodal input, SSE event translation, error mapping), file + vector store clients (OpenAI and Azure storage kinds) via OAGW; OAGW upstream/route provisioning at start.
  - Background work: outbox handlers (usage, audit, attachment cleanup, chat cleanup, thread summary), orphan watchdog, upload reaper, background indexing.
  - Bundled plugins `static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin` (modules `infra::plugins::static_model_policy` / `static_audit`, already imported by the example server).
- Automated tests (unit + in-process integration with SQLite and a mock OpenAI-compatible provider) covering every item of `gears/mini-chat/docs/acceptance-criteria.md`.

## Capabilities

### New Capabilities
- `mini-chat-gear-runtime`: gear registration, configuration keys/defaults/validation, lifecycle, SDK crate, bundled policy/audit plugins, OAGW provisioning, persistence schema.
- `mini-chat-chats`: chat create/get/list/update/delete, model and title validation, model locking, OData list with ordering/filters/pagination, activity ordering, cleanup on deletion.
- `mini-chat-messages`: message listing contract (identity, attachments, reactions, tokens), chronological ordering, message counts, reactions API.
- `mini-chat-streaming`: send-message SSE endpoint, preflight validation, SSE event contract and ordering, idempotency/replay, parallel-turn guard, turn lifecycle and status API, finalization and persistence, error mapping and sanitization.
- `mini-chat-turn-mutations`: retry/edit/delete of the latest terminal turn through the full send pipeline, deterministic concurrency, attachment carry-forward, audit events.
- `mini-chat-attachments`: upload/get/delete for documents and images, limits, indexing lifecycle (sync + background), thumbnails, tool routing, upload reaper, attachment and chat cleanup handlers.
- `mini-chat-quota`: reserve-before-execute, tier cascade and downgrade, credit arithmetic, settlement exactly once (actual/estimated/released), tool quotas, usage outbox events, quota status API and warnings, orphan watchdog settlement.
- `mini-chat-context`: context plan assembly and deterministic truncation, budget checks, tool availability and guards, provider request shape, thread summary trigger/generation/CAS commit/invalidation, web search accounting and citations.

### Modified Capabilities
<!-- none: openspec/specs/ is empty -->

## Impact

- New crate `gears/mini-chat/mini-chat-sdk`; root `Cargo.toml` workspace members (+ workspace dependency entry); `Cargo.lock` gains the local crate (no new external crates — only lockfile-listed dependencies are used).
- `gears/mini-chat/mini-chat` gains its full implementation; `src/lib.rs` keeps the `infra::plugins::static_audit` / `static_model_policy` paths the example server imports.
- Server binary `cf-gears-example-server` with features `mini-chat,static-authn,static-authz,single-tenant,static-credstore` exposes the `/mini-chat/v1/*` API, creates the mini-chat SQLite DB and calls the configured OpenAI-compatible provider through OAGW.
- No changes to other gears' behaviour.
