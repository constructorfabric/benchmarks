## Why

The `mini-chat` gear (multi-tenant AI chat with SSE streaming, attachments and quotas) ships in this workspace with its implementation removed: `gears/mini-chat/mini-chat` only declares two empty plugin modules, nothing registers the gear, nothing serves its API, and its SDK crate does not exist. The authors' contract (`gears/mini-chat/docs/PRD.md`, `DESIGN.md`, `ADR/*`, `acceptance-criteria.md`) and the generated OpenAPI (`docs/api/api.json`) fully describe the expected behaviour, and a black-box acceptance suite checks the server against them. The gear must be rebuilt so the example server built with `mini-chat,static-authn,static-authz,single-tenant,static-credstore` serves that contract.

## What Changes

- New SDK crate `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), added to the workspace and depended on by the gear. It holds the plugin contracts (`MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`), their GTS plugin specs, and the shared domain types (`PolicySnapshot`, `ModelCatalogEntry`, `EstimationBudgets`, `KillSwitches`, `UserLimits`/`TierLimits`, `UsageEvent`, audit event types).
- The gear crate `gears/mini-chat/mini-chat` gains the full implementation:
  - gear registration (`mini-chat`, deps types-registry / authn-resolver / authz-resolver / oagw; db, rest, stateful capabilities), config loading with validation and defaults from DESIGN Appendix B;
  - SeaORM migrations for `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions` (+ outbox tables), SQLite and Postgres;
  - REST + SSE API under `/mini-chat/v1`: chats CRUD, messages list, `messages:stream`, turn status/retry/edit/delete, attachments upload/get/delete, reactions, models, quota status;
  - streaming pipeline: preflight (idempotency/replay → parallel-turn guard → kill switches → quota cascade → input/image guards → context assembly → provider resolution) → reserve transaction → provider relay via OAGW → CAS finalization with quota settlement and outbox emission;
  - quota service (credit arithmetic, premium→standard cascade, reserve/settle, tool daily quotas, warnings), canonical error mapping (RFC 9457 `Problem`) and SSE `{code,message}` errors with provider-id sanitization;
  - `openai_responses` adapter (plus chat-completions / vLLM / Anthropic adapters), OAGW upstream/route provisioning, file and vector-store clients (OpenAI/Azure);
  - background work: outbox handlers (usage, audit, attachment cleanup, chat cleanup, thread summary), orphan watchdog, upload reaper;
  - bundled plugin gears `static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin` in `mini_chat::infra::plugins::{static_model_policy, static_audit}`.
- Automated tests (unit + in-process integration over SQLite with a mock provider) covering every item of `acceptance-criteria.md`.

## Capabilities

### New Capabilities
- `mini-chat-sdk`: SDK crate with plugin contracts and shared policy/usage/audit types.
- `mini-chat-chats`: chat CRUD, model selection/locking, list filtering/ordering/pagination.
- `mini-chat-messages`: message listing, message response contract, reactions.
- `mini-chat-streaming`: send-message SSE stream, SSE event contract, idempotency/replay, parallel-turn guard, turn lifecycle, turn status and mutations (retry/edit/delete), context assembly, provider request shape, web search/tools.
- `mini-chat-attachments`: upload/get/delete of documents and images, indexing lifecycle, limits, thumbnails, upload reaper.
- `mini-chat-quota`: models API, quota status API, reserve/settle quota enforcement, tier downgrade, credit accounting, usage publication.
- `mini-chat-background`: outbox handlers (usage, audit, attachment/chat cleanup, thread summary), orphan watchdog.
- `mini-chat-errors-authz`: canonical error contract, sanitization, PEP authorization and tenant/owner isolation.

### Modified Capabilities
<!-- none: openspec/specs is empty -->

## Impact

- Code: `gears/mini-chat/mini-chat/**` (rewritten), new `gears/mini-chat/mini-chat-sdk/**`, root `Cargo.toml` workspace members/deps.
- APIs: new public REST/SSE endpoints under `/mini-chat/v1` (as in `docs/api/api.json`).
- Dependencies: only crates already present in `Cargo.lock` (offline build).
- Systems: uses api-gateway, authn/authz resolvers, types-registry, OAGW and the toolkit DB outbox; the example server binary links the gear via the existing `mini-chat` feature.
