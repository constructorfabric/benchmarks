## Why

`gears/mini-chat/` ships the specification of the multi-tenant AI chat gear (`PRD.md`, `DESIGN.md`, ADR-0001..0010, `acceptance-criteria.md`) but the implementation was removed: `mini-chat/src/lib.rs` declares two empty modules, nothing registers the gear or serves its API, and the SDK crate does not exist. The example server built with `--features mini-chat,...` therefore exposes no chat API, and the hidden acceptance suite (HTTP/SSE, database and mock-LLM checks) cannot pass. This change rebuilds the gear and its SDK so that the running system matches the documented contract.

## What Changes

- New SDK crate `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), added to the workspace and depended on by the gear: model-policy plugin contract (`MiniChatModelPolicyPluginClientV1`, `PolicySnapshot`, `ModelCatalogEntry`, `KillSwitches`, `UserLimits`, `TierLimits`, `UsageEvent`), audit plugin contract (`MiniChatAuditPluginClientV1`, `TurnAuditEvent`, mutation audit events), GTS plugin spec types.
- New gear `mini-chat` (ToolKit gear; `db`, `rest`, `stateful`) under url prefix `/mini-chat`:
  - Chat CRUD (`/v1/chats`, OData `$filter`/`$orderby`, cursor pagination, title/model validation, model lock, `updated_at` activity ordering, soft delete).
  - Messages list with OData, attachments summaries, `my_reaction`; reactions `PUT`/`DELETE`.
  - SSE streaming send (`messages:stream`): preflight validation, idempotency/replay, parallel-turn guard, quota reserve, context assembly and truncation, provider call through OAGW (OpenAI Responses adapter plus Chat Completions / vLLM / Anthropic adapters), SSE event contract (`stream_started`, `ping`, `delta`, `tool`, `citations`, `done`, `error`), CAS finalization, settlement, outbox usage and audit events, cancellation on disconnect.
  - Turn status, retry, edit, delete of the latest turn with preflight-before-mutation ordering.
  - Attachments upload (multipart, MIME allowlist, size and per-chat limits, provider Files API, per-chat vector store, indexing poll, thumbnails), get, delete with outbox cleanup.
  - Models API, quota status API.
  - Canonical `Problem` error contract (ADR-0004) and SSE `{code,message}` errors with provider-id sanitization.
  - Background work: outbox handlers (usage, audit, attachment cleanup, chat cleanup, thread summary), orphan watchdog, upload reaper, OAGW upstream/route provisioning with S2S credentials.
  - Bundled plugins `static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin` (modules `infra::plugins::static_model_policy` / `static_audit`).
- Automated tests (Rust unit/integration tests and a Python e2e suite outside `testing/e2e/suites/mini_chat/`) that cover every item of `acceptance-criteria.md`.

## Capabilities

### New Capabilities
- `mini-chat-chats`: chat lifecycle (create/get/list/update/delete), model and title validation, model lock, list filtering/ordering/pagination, activity ordering.
- `mini-chat-messages`: message listing contract (identity, attachments, reactions, counts, ordering) and assistant-message reactions.
- `mini-chat-streaming`: send-message SSE pipeline, SSE event contract, idempotency and replay, parallel-turn guard, turn lifecycle and status, context assembly, tools (web search, file search, code interpreter), cancellation, provider error sanitization.
- `mini-chat-turn-mutations`: retry/edit/delete of the latest terminal turn, ordering, concurrency, carried-forward attachments.
- `mini-chat-attachments`: upload/get/delete of documents and images, validation, indexing lifecycle, provider tool availability, cleanup and abandoned-upload recovery.
- `mini-chat-models`: read-only model catalog API.
- `mini-chat-quota`: quota status API, reserve-before-execute enforcement, tier downgrade, credit accounting, settlement and reliable usage publication.
- `mini-chat-background`: chat-deletion cleanup, thread summary generation/invalidation, orphan watchdog, audit delivery, OAGW provisioning, bundled plugins.
- `mini-chat-errors-authz`: canonical error contract, owner/tenant isolation and PEP authorization rules.

### Modified Capabilities
<!-- none: openspec/specs/ is empty -->

## Impact

- Code: `gears/mini-chat/mini-chat/**`, new `gears/mini-chat/mini-chat-sdk/**`, root `Cargo.toml` (workspace member + `mini-chat-sdk` workspace dependency), `Cargo.lock` (path crate only; no new registry dependencies).
- APIs: new REST/SSE endpoints under `/mini-chat/v1/...` as documented in DESIGN §3.3 and `docs/api/api.json`.
- Database: new mini-chat tables (`chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions`) plus the shared outbox tables, SQLite and PostgreSQL.
- Runtime dependencies: types-registry, authn-resolver (S2S), authz-resolver (PDP), OAGW (provider egress), credstore (secrets via OAGW).
