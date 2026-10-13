## Why

`gears/mini-chat/` contains the documented `mini-chat` gear (multi-tenant AI chat) with its implementation removed: the crate only declares two empty plugin modules, nothing registers the gear, nothing serves its API, and its SDK crate does not exist. The contract (PRD, DESIGN, ADRs, acceptance checklist in `gears/mini-chat/docs/`) is complete and a black-box acceptance suite checks the gear through HTTP/SSE, its database and the requests it sends to an OpenAI-compatible LLM provider, so the gear must be rebuilt to match that contract.

## What Changes

- New SDK crate `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`): plugin specs and client traits for the model-policy plugin (`MiniChatModelPolicyPluginClientV1`: policy version, policy snapshot with model catalog and kill switches, user limits, `publish_usage`) and the audit plugin (`MiniChatAuditPluginClientV1`), plus the shared models (`ModelCatalogEntry`, `EstimationBudgets`, `KillSwitches`, `TierLimits`, `UserLimits`, `UsageEvent`, audit events). Added to the workspace; the gear depends on it.
- The `mini-chat` gear in `gears/mini-chat/mini-chat` registered with the runtime (deps: types-registry, authn-resolver, authz-resolver, oagw; capabilities: db, rest, stateful), configured from `gears.mini-chat.config` with the keys and defaults of DESIGN Appendix B.
- REST + SSE API under `/mini-chat/v1`: chats CRUD (OData list), messages list (OData), `messages:stream` (SSE), attachments upload/get/delete, turn status/retry/edit/delete, models list/get, reactions put/delete, quota status. Errors follow the canonical `Problem` contract (ADR-0004); SSE errors use `{code, message}`.
- Database schema (SQLite and PostgreSQL migrations) for `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions`, plus the shared outbox tables.
- Streaming turn pipeline: idempotency/replay, parallel-turn guard, quota preflight with premium→standard cascade and kill switches, context assembly and truncation within the token budget, reserve transaction, OpenAI Responses adapter over OAGW (also Chat Completions, vLLM and Anthropic adapters), SSE relay without buffering, CAS-guarded finalization with quota settlement and outbox usage/audit events.
- Turn mutations (retry/edit/delete of the latest terminal turn), attachments (Files API, per-chat vector store, thumbnails, limits, background indexing), tools (file_search, web_search, code_interpreter) with citations and per-turn/daily limits.
- Background work: outbox handlers (usage, audit, attachment cleanup, chat cleanup, thread summary), orphan watchdog, upload reaper; OAGW upstream/route provisioning at start.
- Bundled plugins `static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin` in `infra::plugins::static_model_policy` / `static_audit`.
- Automated tests covering every item of `acceptance-criteria.md`.

## Capabilities

### New Capabilities
- `mini-chat-sdk`: SDK crate with the plugin contracts and shared models used by the gear and its plugins.
- `mini-chat-chats`: chat lifecycle, model selection/locking, listing with OData and pagination, owner/tenant isolation and authorization.
- `mini-chat-messages`: message listing, message DTO contract (identity, attachments, reactions), reactions API.
- `mini-chat-streaming`: send-message SSE contract, preflight validation, idempotency/replay, parallel-turn guard, context assembly, provider requests, finalization and error sanitization.
- `mini-chat-turns`: turn status API, turn state machine, retry/edit/delete mutations.
- `mini-chat-attachments`: upload/get/delete of documents and images, limits, indexing lifecycle, tool availability, cleanup and upload reaper.
- `mini-chat-quota`: quota preflight/reserve/settlement, tier downgrade, credits, tool quotas, quota status API, models API, usage outbox.
- `mini-chat-background`: outbox handlers (usage, audit, cleanup, thread summary), orphan watchdog, OAGW provisioning, bundled plugins.

### Modified Capabilities

(none — no existing specs in `openspec/specs/`)

## Impact

- New crate `gears/mini-chat/mini-chat-sdk`; root `Cargo.toml` gains the workspace member and a `mini-chat-sdk` workspace dependency.
- `gears/mini-chat/mini-chat` gains its full implementation; its `Cargo.toml` depends on the SDK (no new third-party crates beyond the lockfile).
- `apps/cf-gears-example-server` keeps importing `mini_chat`, `infra::plugins::static_audit` and `infra::plugins::static_model_policy`; the server built with `--features mini-chat,static-authn,static-authz,single-tenant,static-credstore` serves the gear.
- Outbound traffic only through OAGW; credentials stay in credstore.
