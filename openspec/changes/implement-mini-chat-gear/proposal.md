## Why

The `mini-chat` gear (multi-tenant AI chat with SSE streaming, attachments and credit quotas) exists in the workspace only as a stub: `gears/mini-chat/mini-chat/src/lib.rs` declares two empty plugin modules, nothing registers the gear, nothing serves its API, and its SDK crate is missing. The authoritative contract (`gears/mini-chat/docs/PRD.md`, `DESIGN.md`, accepted ADRs and `acceptance-criteria.md`) is complete, so the gear can be rebuilt against it and checked black-box through HTTP/SSE, the gear database and the requests it sends to an OpenAI-compatible provider.

## What Changes

- New SDK crate `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), added to the workspace and depended on by the gear. It defines the plugin contracts and shared models: `MiniChatModelPolicyPluginClientV1` (policy snapshot, model catalog, kill switches, user limits, `publish_usage`), `MiniChatAuditPluginClientV1`, their GTS plugin specs, `UsageEvent`, audit event types and error types.
- The `mini-chat` gear (`gears/mini-chat/mini-chat`) is implemented as a ToolKit gear with `db`, `rest` and `stateful` capabilities depending on `types-registry`, `authn-resolver`, `authz-resolver` and `oagw`:
  - REST API under `/mini-chat/v1`: chats CRUD, message list (OData + cursor pagination), `messages:stream` (SSE), attachments upload/get/delete, turn status, retry/edit/delete last turn, models list/get, reactions set/remove, quota status.
  - SSE contract: `stream_started`, `ping`, `delta`, `tool`, `citations`, `done`, `error`, with idempotent replay and a one-running-turn-per-chat guard.
  - Canonical RFC 9457 `Problem` error contract for REST (ADR-0004) and sanitized `{code, message}` SSE errors.
  - Persistence (SQLite and PostgreSQL migrations) for `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions`, plus a gear-owned transactional outbox.
  - Quota service: credit arithmetic, reserve → execute → settle with premium → standard downgrade cascade, kill switches, daily web-search / code-interpreter quotas, CAS-guarded finalization with exactly-once usage + audit outbox events.
  - Knowledge search (`search_knowledge` function tool, agentic loop, Azure vector-store retriever) when enabled.
  - Context assembly with deterministic truncation, tool selection (`file_search`, `web_search`, `code_interpreter`) and guards; OpenAI Responses adapter through the OAGW in-process proxy, with OAGW upstream/route provisioning at start.
  - Background work: orphan-turn watchdog, upload reaper, outbox handlers for usage, audit, attachment cleanup, chat cleanup and thread summary.
  - Bundled plugins `static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin` in `infra::plugins::{static_model_policy, static_audit}`.
- Automated tests (unit + in-process integration with a mock OpenAI-compatible provider) covering every item of `acceptance-criteria.md`.

## Capabilities

### New Capabilities
- `mini-chat-sdk`: SDK crate, plugin contracts (model policy, audit), shared models and bundled static plugins.
- `mini-chat-chats`: chat CRUD, list filtering/ordering/pagination, model selection and immutability, Models API, tenant/owner isolation and authorization.
- `mini-chat-messages`: message listing contract, message counts and ordering, reactions API.
- `mini-chat-streaming`: send-message SSE pipeline, preflight validation, SSE event contract and ordering, idempotency/replay, parallel-turn guard, turn lifecycle and turn status, error mapping and sanitization, provider request shape.
- `mini-chat-turn-mutations`: retry / edit / delete of the latest terminal turn.
- `mini-chat-attachments`: upload/get/delete of documents and images, indexing lifecycle, limits, thumbnails, tool availability, upload reaper.
- `mini-chat-quota`: quota preflight, downgrade cascade, reserve/settlement, credits accounting, usage/audit outbox events, quota status API, web search quota.
- `mini-chat-context`: context plan assembly, budget enforcement and truncation, thread summary trigger/worker and mutation-driven invalidation.
- `mini-chat-cleanup`: chat-deletion and attachment cleanup of provider resources, orphan watchdog.

### Modified Capabilities
<!-- none: no existing specs in openspec/specs -->

## Impact

- New crate `gears/mini-chat/mini-chat-sdk`; root `Cargo.toml` workspace members and `[workspace.dependencies]` gain it; `gears/mini-chat/mini-chat/Cargo.toml` depends on it.
- `gears/mini-chat/mini-chat/src/**` is written from scratch (existing `infra::plugins::{static_audit, static_model_policy}` paths are preserved because `apps/cf-gears-example-server` imports them).
- The example server built with `--features mini-chat,static-authn,static-authz,single-tenant,static-credstore` now serves the gear; configuration keys follow `DESIGN.md` Appendix B (`config/mini-chat.yaml`).
- Outbound provider traffic goes only through OAGW; no new third-party dependencies beyond those in `Cargo.lock`.
