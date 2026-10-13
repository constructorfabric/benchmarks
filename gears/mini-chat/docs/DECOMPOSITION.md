---
description: "Decomposition of the mini-chat gear design into seven dependency-ordered features (foundation, chat CRUD, quota and billing, streaming turns, attachments, turn mutations, background workers), each traced to PRD, DESIGN and ADR IDs."
---

# Decomposition: Mini Chat

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-mini-chat-status-overall`

## Table of Contents

1. [1. Overview](#1-overview)
2. [2. Entries](#2-entries)
   - [2.1 Gear Foundation - HIGH](#21-gear-foundation---high)
   - [2.2 Chat CRUD, Messages and Reactions - HIGH](#22-chat-crud-messages-and-reactions---high)
   - [2.3 Quota and Billing - HIGH](#23-quota-and-billing---high)
   - [2.4 Streaming Turns - HIGH](#24-streaming-turns---high)
   - [2.5 Attachments - MEDIUM](#25-attachments---medium)
   - [2.6 Turn Mutations - MEDIUM](#26-turn-mutations---medium)
   - [2.7 Background Workers - HIGH](#27-background-workers---high)
3. [3. Feature Dependencies](#3-feature-dependencies)

## 1. Overview

The Mini Chat DESIGN is split into seven features ordered by dependency. The split follows the design's layering and lifecycle boundaries: a foundation that every other feature builds on (SDK, gear lifecycle, configuration, schema, authorization, error contract, plugins, provider registry, Models API), then the read/write chat surface, the quota and billing engine, the streaming turn pipeline that consumes both, the attachment pipeline that plugs into streaming, the tail-only turn mutations that reuse the full send pipeline, and finally the background workers that reconcile and clean up after all of them.

Each feature is a cohesive, independently testable slice with an explicit definition of done: each area of `acceptance-criteria.md` is mapped to the feature that owns it, and an area shared by two features (Principles & Constraints, Messages API, Web Search) is split by item with the owned items named (stated in each entry's Scope "Acceptance" line), and every item of `acceptance-criteria.md` MUST be implemented and tested. Every PRD functional and non-functional requirement and every DESIGN principle, constraint, component, sequence and table is referenced by at least one feature. Items that the accepted ADRs mark as Future, deferred or not implemented in P1 (MCP support per `cpt-cf-mini-chat-adr-mcp-deferred`, document summary per `cpt-cf-mini-chat-adr-document-retrieval-scope`, temporary chats, group chats per `cpt-cf-mini-chat-adr-group-chat-usage-attribution`, hard-purge per `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`) are traced to the feature whose boundary they touch, and that feature's scope states only the P1 behaviour the documents define for them (reserved columns, absent tables, tools never injected); no feature implements a deferred capability.

**Task-level overrides** (these win over any conflicting statement in the supplied documents):

- The public SDK crate lives at `gears/mini-chat/mini-chat-sdk/` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`), is added to the workspace, and the gear crate depends on it.
- The gear crate stays at `gears/mini-chat/mini-chat/` (package `cf-gears-mini-chat`, lib `mini_chat`) and keeps the modules `infra::plugins::static_audit` and `infra::plugins::static_model_policy`, which the example server imports.
- Where `config/mini-chat.yaml` or platform docs disagree with `gears/mini-chat/docs/`, the gear docs win.
- Every item of `acceptance-criteria.md` is implemented and tested by the feature that owns its area.

**Parallelization**: after `cpt-cf-mini-chat-feature-gear-foundation`, the features `cpt-cf-mini-chat-feature-chat-crud` and `cpt-cf-mini-chat-feature-quota-billing` can be developed in parallel. The remaining features are sequential because each one extends the pipeline delivered by its predecessors.

## 2. Entries

### 2.1 [Gear Foundation](features/0001-gear-foundation.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-gear-foundation`

- **Purpose**: Establish the gear skeleton every other feature builds on: the public SDK crate, gear registration and lifecycle, validated configuration, the complete P1 database schema, tenant/owner scoping through the authorization PEP, the canonical error contract, the bundled static model-policy and audit plugins, the provider registry with OAGW upstream provisioning, and the read-only Models API.

- **Depends On**: None

- **Scope**:
  - SDK crate `gears/mini-chat/mini-chat-sdk/` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) added to the workspace and depended on by the gear: public models (model catalog entry, kill switches, user limits, policy snapshot, usage event, turn audit event) and the plugin client traits for the model-policy plugin and the audit plugin.
  - Gear crate `gears/mini-chat/mini-chat/` (package `cf-gears-mini-chat`, lib `mini_chat`) registered as the `mini-chat` ToolKit gear with the `init` / `start` / `stop` lifecycle and REST registration under the configured URL prefix; the modules `infra::plugins::static_audit` and `infra::plugins::static_model_policy` are kept.
  - Gear configuration keys, defaults and validation per DESIGN Appendix B.1 (unknown keys rejected, removed keys fail startup, deprecated keys warned per `cpt-cf-mini-chat-adr-runtime-consistency-limitations`); gear docs win over `config/mini-chat.yaml` and platform docs.
  - Database schema and migrations (PostgreSQL and SQLite) for every P1 table and the domain model entities; no MCP tables and no `tenant_closure` projection are created.
  - Tenant/owner scoping and the authorization PEP per DESIGN §3.8 (`cpt-cf-mini-chat-design-authz-pep`): resource types, per-operation authorization matrix, `AccessScope` compiled to Secure ORM, fail-closed behaviour (404 hidden, 403 deny, 503 PDP failure).
  - License gate on every route (interim base-license feature per `cpt-cf-mini-chat-adr-quota-policy-scope`).
  - Canonical error contract per `cpt-cf-mini-chat-adr-canonical-error-contract`: domain errors mapped to `Problem` categories, statuses and machine-readable reasons, shared by all later features.
  - Static model-policy plugin (model catalog, kill switches, default tier limits) and static audit plugin (logs events), resolved lazily through types-registry; model-policy and audit gateways; audit outbox queue and handler with the drop/retry/dead-letter delivery rules.
  - Outbox pipeline wiring, leader elector preparation, metrics registry, and bounded worker shutdown in the gear lifecycle.
  - Provider registry and OAGW upstream/route provisioning at gear start per `cpt-cf-mini-chat-adr-llm-provider-as-library`, `cpt-cf-mini-chat-adr-internal-transport` and `cpt-cf-mini-chat-adr-multi-provider-adapters` (provider entries, tenant overrides, S2S client credentials, deferred retries).
  - Models API: list and get, visibility algorithm (globally enabled catalog entries only), non-exposure of internal fields (`cpt-cf-mini-chat-design-model-catalog`).
  - MCP (`cpt-cf-mini-chat-adr-mcp-deferred`): the MCP registry, hub discovery, role access, admin API, `McpPool`, `McpService` and MCP tables are Future; this feature traces them and confirms that no MCP module, endpoint, table or migration is created.
  - Acceptance: Principles & Constraints (tenant and owner isolation); Models API; Authorization.

- **Out of scope**:
  - Chat, message, reaction, attachment, turn and quota endpoints (later features).
  - Quota arithmetic, reserve and settlement logic (`cpt-cf-mini-chat-feature-quota-billing`).
  - Provider adapter request/stream translation (`cpt-cf-mini-chat-feature-streaming-turns`).
  - Any MCP implementation, policy-snapshot caching and `POST /internal/policy:notify` (Future per ADRs).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-license-gate`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-model-selection`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-audit`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-server-registry`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-hub-discovery`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-role-access`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-tenant-isolation`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-authz-alignment`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-observability-supportability`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-tenant-scoped`
  - [ ] `p1` - `cpt-cf-mini-chat-principle-owner-only-content`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-openai-compatible`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-no-credentials`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-license-gate`

- **Domain Model Entities**:
  - Chat
  - Message
  - Attachment
  - ThreadSummary
  - ChatVectorStore
  - QuotaUsage
  - MessageReaction
  - AuditEvent

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`
  - [ ] `p1` - `cpt-cf-mini-chat-component-authz-integration`
  - [ ] `p1` - `cpt-cf-mini-chat-component-llm-provider`
  - [ ] `p1` - `cpt-cf-mini-chat-component-mcp-pool`
  - [ ] `p1` - `cpt-cf-mini-chat-component-mcp-service`

- **API**:
  - GET /v1/models (`cpt-cf-mini-chat-interface-models-api`)
  - GET /v1/models/{id}
  - Public REST surface registration and error mapping (`cpt-cf-mini-chat-interface-public-api`)
  - MCP admin API is Future and not exposed (`cpt-cf-mini-chat-interface-mcp-api`)

- **Sequences**:

  None

- **Data**:

  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chats`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-messages`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-turns`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-attachments`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-message-attachments`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-thread-summaries`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-vector-stores`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-quota-usage`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-message-reactions`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-tenant-closure-ref`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-mcp-servers`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-mcp-server-tools`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-role-mcp-servers`

### 2.2 [Chat CRUD, Messages and Reactions](features/0002-chat-crud.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-chat-crud`

- **Purpose**: Deliver the owner-scoped chat management surface: create, get, list, rename and delete chats with model and title validation, the paginated message history API with its response contract, and like/dislike reactions on assistant messages.

- **Depends On**: `cpt-cf-mini-chat-feature-gear-foundation`

- **Scope**:
  - Chat create with model validation and the deterministic default-model algorithm; the chat's model is immutable once set.
  - Chat get (metadata and `message_count`, no embedded messages), rename (title only, title validation), list with OData filter/order, cursor pagination, malformed-input rejection and ordering by most recent activity.
  - Chat delete as a soft delete of the chat row that returns 204, makes all sub-resources unreachable, and enqueues provider cleanup work (the cleanup handlers belong to `cpt-cf-mini-chat-feature-background-workers`).
  - Messages list with OData filter/order, cursor pagination and limit clamping; message response contract with the always-present `request_id`, `attachments` (from `message_attachments`, non-deleted only) and `my_reaction` fields.
  - Message reactions: set and remove on assistant messages only, idempotent remove, one reaction per user per message (`cpt-cf-mini-chat-interface-message-reaction`).
  - Temporary chats are deferred to P2: the reserved `is_temporary` column is never modified and no toggle endpoint is exposed.
  - Use cases: `cpt-cf-mini-chat-usecase-delete-chat`.
  - Acceptance: Principles & Constraints (a chat's model is immutable once set); Chat CRUD; Messages API (list, response contract); Reactions API.

- **Out of scope**:
  - Sending messages and turn persistence (`cpt-cf-mini-chat-feature-streaming-turns`).
  - Provider-side cleanup after deletion (`cpt-cf-mini-chat-feature-background-workers`).
  - Temporary chat toggle and 24h auto-deletion (P2); hard-purge of soft-deleted rows (not implemented per ADR-0009).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-chat-crud`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-model-selection`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-conversation-history`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-message-reactions`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-chat-deletion-cleanup`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-temporary-chat`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-tenant-isolation`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-authz-alignment`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-tenant-scoped`
  - [ ] `p1` - `cpt-cf-mini-chat-principle-owner-only-content`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-model-locked-per-chat`

- **Domain Model Entities**:
  - Chat
  - Message
  - MessageReaction

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`
  - [ ] `p1` - `cpt-cf-mini-chat-component-authz-integration`

- **API**:
  - POST /v1/chats
  - GET /v1/chats
  - GET /v1/chats/{id}
  - PATCH /v1/chats/{id}
  - DELETE /v1/chats/{id}
  - GET /v1/chats/{id}/messages
  - PUT /v1/chats/{id}/messages/{msg_id}/reaction
  - DELETE /v1/chats/{id}/messages/{msg_id}/reaction

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-chat-deletion-cleanup`

- **Data**:

  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chats`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-messages`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-message-attachments`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-message-reactions`

### 2.3 [Quota and Billing](features/0003-quota-billing.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-quota-billing`

- **Purpose**: Implement DESIGN §5 quota enforcement and billing integration so that every provider call is preceded by a reserve, every reserved invocation settles exactly once, and usage is published reliably for downstream billing.

- **Depends On**: `cpt-cf-mini-chat-feature-gear-foundation`

- **Scope**:
  - Credit arithmetic with overflow protection and the canonical credit formula over model multipliers from the policy snapshot; policy version resolution and persistence per turn.
  - Token estimation strategy (input, images, tool and web-search surcharges, reserved credits, `max_output_tokens` bound).
  - Preflight reserve, period enforcement (daily and monthly, UTC calendar boundaries), `total` and `tier:premium` buckets, tier downgrade cascade with kill switches and model disablement, reserve write with in-transaction limit re-check, 429 rejection when all tiers are exhausted.
  - Settlement and finalization contract (owned here as the quota service API): terminal outcome taxonomy, billing outcome derivation, settlement formulas (actual, estimated, released), overshoot cap, bounded debit for aborted streams, and the CAS-guarded finalization transaction helper that settles quota and enqueues the usage event exactly once in one transaction (`cpt-cf-mini-chat-design-quota-usage-accounting`, `cpt-cf-mini-chat-design-throttling-tiers`, `cpt-cf-mini-chat-design-nonterminal-reconciliation`, `cpt-cf-mini-chat-design-terminal-error-reconciliation`). This feature does not decide when a turn finalizes or with which terminal outcome: `cpt-cf-mini-chat-feature-streaming-turns` owns the turn state machine and calls these helpers.
  - Outbox usage publication through the model-policy plugin with dedupe keys and UUID normalization; system-task usage events (`requester_type = system`, no user debit).
  - Quota warnings and the quota status API (per tier and period, `warning` / `exhausted` flags, next reset).
  - Web-search and code-interpreter daily quota rules and per-turn limits as quota decisions (checked only when the tool is used by the effective model). For `cpt-cf-mini-chat-fr-code-interpreter` this feature owns only the quota facet; tool enablement is owned by `cpt-cf-mini-chat-feature-streaming-turns`.
  - Cost metrics (`cpt-cf-mini-chat-fr-cost-metrics`), billing group: token and credit values per request (input tokens, output tokens, reserved, settled and released credits, billing outcome) and the quota and usage outbox metrics. The per-request latency and tool-call metrics belong to `cpt-cf-mini-chat-feature-streaming-turns`.
  - Sequence note for `cpt-cf-mini-chat-seq-send-message`: this feature covers only the quota steps (preflight evaluation, reserve write, settlement and usage event); the end-to-end flow is covered by `cpt-cf-mini-chat-feature-streaming-turns`.
  - Group chats are deferred (P2+); the per-sender attribution model of `cpt-cf-mini-chat-adr-group-chat-usage-attribution` is preserved by keying every bucket on the requesting user.
  - Acceptance: Principles & Constraints (quota checked before any outbound provider call); Quota Status API; Quota Enforcement; Settlement & Finalization; Web Search (accounting and quota limits).

- **Out of scope**:
  - The streaming pipeline that calls preflight and finalization (`cpt-cf-mini-chat-feature-streaming-turns`).
  - Orphan watchdog scheduling (`cpt-cf-mini-chat-feature-background-workers`).
  - Image-input daily quota and image byte counters, policy snapshot caching, tenant operational bucket for system tasks (not implemented per ADR-0008).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-quota-enforcement`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-quota-billing-architecture`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-web-search`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-code-interpreter`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-group-chats`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-cost-metrics`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-cost-control`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-tenant-scoped`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-quota-before-outbound`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-model-locked-per-chat`

- **Domain Model Entities**:
  - QuotaUsage
  - Chat turn (reserve, policy version and settlement fields)

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-quota-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`

- **API**:
  - GET /v1/quota/status

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-send-message`

- **Data**:

  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-quota-usage`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-turns`

### 2.4 [Streaming Turns](features/0004-streaming-turns.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-streaming-turns`

- **Purpose**: Deliver the core chat turn: a send-message SSE endpoint that validates, reserves quota, assembles context, calls the provider through OAGW, relays events without buffering, and finalizes and persists the turn with idempotent replay, a single running turn per chat, cancellation and a turn status API.

- **Depends On**: `cpt-cf-mini-chat-feature-chat-crud`, `cpt-cf-mini-chat-feature-quota-billing`

- **Scope**:
  - Send-message SSE endpoint correlated by `request_id`, with the full event contract (`stream_started`, `delta`, `tool`, `citations`, `done`, `error`, `ping`) and its ordering; `done` exposes usage and the quota/downgrade outcome without internal identifiers; `error` is terminal and sanitized.
  - Provider adapters (OpenAI Responses wire format via OAGW, plus the Chat Completions, vLLM Responses and Anthropic Messages kinds) and provider event translation; bounded channels and no buffering.
  - Context assembly and deterministic truncation within the context budget (system prompt, thread summary, recent history, current message); `INPUT_TOO_LONG` and `CONTEXT_BUDGET_EXCEEDED` rejection.
  - Tool availability and guidance (this feature owns the tool-enablement decision in the provider request): web search (explicit enablement, kill switch, model support, system prompt guard, citations), file search enabled when ready documents exist in the chat vector store (owned facet of `cpt-cf-mini-chat-fr-file-search`), code interpreter enabled when ready XLSX attachments exist, with kill switch and model support (owned facet of `cpt-cf-mini-chat-fr-code-interpreter`), knowledge search when enabled (`cpt-cf-mini-chat-design-web-search`, `cpt-cf-mini-chat-design-code-interpreter`, `cpt-cf-mini-chat-design-code-interpreter-config`). The decision is built on attachment state owned by `cpt-cf-mini-chat-feature-attachments`.
  - Send-pipeline preflight hook (owned here) with validation of content, limits and `attachment_ids` (same tenant, user and chat, status ready, no duplicates, count limits, image count and vision capability of the effective model) before any provider call, returning `Problem` errors without opening a stream.
  - Testing note: this feature can be tested with attachment rows seeded directly in the database, while the upload path arrives in `cpt-cf-mini-chat-feature-attachments`.
  - Turn state machine (`running` to `completed` / `cancelled` / `failed`) including partial and null-content cases (`cpt-cf-mini-chat-design-turn-lifecycle`); this feature decides when to finalize and with which terminal outcome (completed, failed with an error code, cancelled on disconnect), persists the assistant message, and emits the terminal SSE event only after the finalization commit. Settlement itself is done by the quota helpers of `cpt-cf-mini-chat-feature-quota-billing`, which this feature calls. Message count and chronological ordering across turns.
  - Idempotency and side-effect-free replay by `request_id`, `request_id` conflict rejection, replay checked before the parallel-turn guard, one running turn per chat.
  - Turn status API and the UX recovery contract (`cpt-cf-mini-chat-interface-turn-status`, `cpt-cf-mini-chat-contract-sse-streaming`).
  - Cancellation on client disconnect propagated to the provider call; the bounded debit is computed and settled by the finalization helper of `cpt-cf-mini-chat-feature-quota-billing`.
  - Error mapping to the canonical error contract consistently across REST and SSE, and sanitization of provider-originated details.
  - Turn-finalization audit events (enqueued by this feature in the finalization flow) and per-request cost metrics (`cpt-cf-mini-chat-fr-cost-metrics`), request group: model, tool call counts (file search, web search, code interpreter), time to first token and total latency. The token and credit values come from `cpt-cf-mini-chat-feature-quota-billing`.
  - Sequence note for `cpt-cf-mini-chat-seq-send-message`: this feature covers the end-to-end flow. The quota reserve and settle steps are delegated to `cpt-cf-mini-chat-feature-quota-billing`, and `cpt-cf-mini-chat-feature-turn-mutations` only reuses the pipeline.
  - MCP tool discovery and execution are Future (`cpt-cf-mini-chat-adr-mcp-deferred`): MCP tools are never injected into the tool list.
  - Use cases: `cpt-cf-mini-chat-usecase-send-message`, `cpt-cf-mini-chat-usecase-reconnect-turn-status`.
  - Acceptance: Principles & Constraints (context window budget; streaming never buffered); Messages API (message count and chronological ordering across turns); Streaming: Send Message; SSE Event Contract; Idempotency & Replay; Parallel Turn Enforcement; Turn Lifecycle; Context Assembly; Error Mapping & Sanitization; Web Search (tool reporting and citations).

- **Out of scope**:
  - Attachment upload, indexing, lifecycle and citation file ID resolution data (`cpt-cf-mini-chat-feature-attachments`).
  - Retry, edit and delete of turns (`cpt-cf-mini-chat-feature-turn-mutations`).
  - Orphan turn recovery and thread summary generation (`cpt-cf-mini-chat-feature-background-workers`).
  - MCP tool injection and execution (Future).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-chat-streaming`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-conversation-history`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-streaming-cancellation`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-token-budget`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-web-search`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-code-interpreter`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-file-search`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-ux-recovery`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-audit`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-cost-metrics`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-tool-discovery`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-tool-execution`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-streaming-latency`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-tenant-isolation`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-observability-supportability`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-resilience-recovery`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-streaming-first`
  - [ ] `p1` - `cpt-cf-mini-chat-principle-summary-over-history`
  - [ ] `p1` - `cpt-cf-mini-chat-principle-linear-conversation`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-openai-compatible`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-model-image-capability`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-context-budget`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-no-buffering`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-bounded-channels`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-quota-before-outbound`

- **Domain Model Entities**:
  - Chat turn
  - Message
  - ContextPlan
  - AuditEvent

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-llm-provider`
  - [ ] `p1` - `cpt-cf-mini-chat-component-quota-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`

- **API**:
  - POST /v1/chats/{id}/messages:stream
  - GET /v1/chats/{id}/turns/{request_id}

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-send-message`
  - [ ] `p1` - `cpt-cf-mini-chat-seq-cancellation`

- **Data**:

  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-turns`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-messages`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-message-attachments`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-thread-summaries`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chats`

### 2.5 [Attachments](features/0005-attachments.md) - MEDIUM

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-attachments`

- **Purpose**: Let users ground conversations in their own documents, spreadsheets and images: upload with validation and limits, provider file storage, a per-chat vector store with asynchronous indexing, file search and image input in turns, citations, deletion and recovery of abandoned uploads.

- **Depends On**: `cpt-cf-mini-chat-feature-chat-crud`, `cpt-cf-mini-chat-feature-streaming-turns`

- **Scope**:
  - Upload, get and delete of documents and images with size, MIME type, purpose routing (`for_file_search`, `for_code_interpreter`), kill switch, chat-model capability and per-chat count and storage limits; bounded concurrent uploads; image thumbnails.
  - Provider file upload through OAGW (including the secondary Anthropic copy for images) and one vector store per chat (`cpt-cf-mini-chat-design-vector-store-scope`).
  - Synchronous indexing within the request deadline, background continuation with failure and timeout handling, `indexing_failed` outcome.
  - Ready-attachment state and file ID data for the send pipeline: this feature provides the ready-attachment state (including readiness of the first document and of XLSX files) that the send pipeline of `cpt-cf-mini-chat-feature-streaming-turns` validates (`attachment_ids` preflight) and uses to enable file search and code interpreter. It owns the attachment-side facets of `cpt-cf-mini-chat-fr-file-search` (vector store, indexing, readiness) and `cpt-cf-mini-chat-fr-code-interpreter` (spreadsheet purpose and provider file), and consumes the send pipeline rather than owning it. Image inputs as multimodal content; citation file ID to attachment ID and title resolution data (`cpt-cf-mini-chat-design-rag-quality-controls`).
  - Attachment deletion (soft delete, 409 when referenced by a sent message, idempotent re-delete) with provider file cleanup through the attachment cleanup outbox handler.
  - Abandoned-upload reaper (`upload_abandoned`) under leader election, skipping rows owned by chat cleanup.
  - Document summary on upload is not implemented (`cpt-cf-mini-chat-adr-document-retrieval-scope`): `doc_summary` stays `null`.
  - Use cases: `cpt-cf-mini-chat-usecase-upload-document`, `cpt-cf-mini-chat-usecase-upload-image`, `cpt-cf-mini-chat-usecase-doc-search`.
  - Acceptance: Attachments.

- **Out of scope**:
  - Chat vector store deletion on chat deletion (`cpt-cf-mini-chat-feature-background-workers`).
  - Document summaries, per-chat chunk cap, immediate exclusion of deleted documents from file search, document search in Anthropic chats (not implemented per ADR-0007).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-file-upload`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-image-upload`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-file-search`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-code-interpreter`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-per-chat-doc-limits`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-attachment-deletion`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-doc-summary`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-rag-scalability`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-tenant-isolation`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-owner-only-content`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-openai-compatible`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-model-image-capability`

- **Domain Model Entities**:
  - Attachment
  - ChatVectorStore

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-llm-provider`
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`

- **API**:
  - POST /v1/chats/{id}/attachments
  - GET /v1/chats/{id}/attachments/{attachment_id}
  - DELETE /v1/chats/{id}/attachments/{attachment_id}

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-file-upload`

- **Data**:

  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-attachments`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-vector-stores`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-message-attachments`

### 2.6 [Turn Mutations](features/0006-turn-mutations.md) - MEDIUM

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-turn-mutations`

- **Purpose**: Let users retry, edit or delete the latest turn of a chat while keeping the conversation strictly linear, reusing the full send pipeline and keeping summaries and audit trail consistent.

- **Depends On**: `cpt-cf-mini-chat-feature-streaming-turns`, `cpt-cf-mini-chat-feature-attachments`

- **Scope**:
  - Retry, edit and delete restricted to the latest, terminal turn of the requesting user (deterministic latest-turn selection, `NOT_LATEST_TURN`, non-terminal rejection) (`cpt-cf-mini-chat-design-turn-mutations`).
  - Retry and edit run the full send pipeline (validation, quota preflight before replacement, context budget, attachment and image checks) with a new `request_id` and respond with an SSE stream; setup failures after the replacement commit mark the new turn failed.
  - Deterministic resolution of concurrent mutations (`GENERATION_IN_PROGRESS`).
  - Carrying forward attachment associations (excluding deleted attachments) and the requested tool usage to the replacement turn.
  - Summary interaction: deleting a summary that covers the mutated turn and clearing compression flags in the mutation transaction.
  - Audit events `turn_retry`, `turn_edit`, `turn_delete` enqueued in the mutation transaction.
  - Sequence note for `cpt-cf-mini-chat-seq-send-message`: this feature only reuses the pipeline delivered by `cpt-cf-mini-chat-feature-streaming-turns` and `cpt-cf-mini-chat-feature-quota-billing`; it does not own any step of the sequence.
  - Use cases: `cpt-cf-mini-chat-usecase-retry-turn`, `cpt-cf-mini-chat-usecase-edit-turn`, `cpt-cf-mini-chat-usecase-delete-turn`.
  - Acceptance: Turn Mutations.

- **Out of scope**:
  - Editing or deleting arbitrary historical messages, branching and multi-version conversations (P2).
  - Summary regeneration after invalidation (`cpt-cf-mini-chat-feature-background-workers`).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-turn-mutations`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-thread-summary`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-audit`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-linear-conversation`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-model-image-capability`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-context-budget`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-quota-before-outbound`

- **Domain Model Entities**:
  - Chat turn
  - Message
  - ThreadSummary
  - AuditEvent

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-quota-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`

- **API**:
  - POST /v1/chats/{id}/turns/{request_id}/retry
  - PATCH /v1/chats/{id}/turns/{request_id}
  - DELETE /v1/chats/{id}/turns/{request_id}

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-send-message`

- **Data**:

  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-turns`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-messages`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-message-attachments`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-thread-summaries`

### 2.7 [Background Workers](features/0007-background-workers.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-background-workers`

- **Purpose**: Close the lifecycle loops that run outside a request: reliable provider-side cleanup after chat deletion, recovery of turns abandoned by crashed instances, and thread summary generation that keeps long conversations within budget.

- **Depends On**: `cpt-cf-mini-chat-feature-turn-mutations`, `cpt-cf-mini-chat-feature-attachments`, `cpt-cf-mini-chat-feature-quota-billing`

- **Scope**:
  - Chat deletion cleanup through the chat cleanup outbox handler: deletion of provider files and the chat vector store with retries, attachment cleanup state machine and ordering, best-effort removal target.
  - Orphan-turn watchdog: stale running turns transitioned to failed from durable progress time, bounded debit settlement and usage/audit outbox emission through the CAS guard, single-actor guarantee under leader election.
  - Thread summary generation through the thread summary outbox handler: trigger evaluation in the finalization transaction, summarization call as a system task with its usage event, failure keeping the previous summary, bounded retries and dead-lettering, commit guarded against mutation-driven invalidation.
  - Use cases: `cpt-cf-mini-chat-usecase-delete-chat`.
  - Acceptance: Cleanup & Recovery.

- **Out of scope**:
  - Hard-purge of soft-deleted rows and chat-deletion audit events (not implemented per ADR-0009).
  - Temporary chat auto-deletion (P2); summary quality gate (P2+); kill-switch checks and operational billing for system tasks (Future per ADR-0008).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-chat-deletion-cleanup`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-thread-summary`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-quota-billing-architecture`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-data-retention`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-resilience-recovery`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-observability-supportability`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-summary-over-history`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-no-credentials`

- **Domain Model Entities**:
  - Chat
  - Chat turn
  - Attachment
  - ChatVectorStore
  - ThreadSummary

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-orphan-watchdog`
  - [ ] `p1` - `cpt-cf-mini-chat-component-llm-provider`
  - [ ] `p1` - `cpt-cf-mini-chat-component-quota-service`
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`

- **API**:
  - None (background workers driven by the outbox and periodic schedules)

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-chat-deletion-cleanup`
  - [ ] `p1` - `cpt-cf-mini-chat-seq-thread-summary`

- **Data**:

  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chats`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-turns`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-attachments`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-chat-vector-stores`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-thread-summaries`
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-messages`

---

## 3. Feature Dependencies

```text
cpt-cf-mini-chat-feature-gear-foundation
    ↓
    ├─→ cpt-cf-mini-chat-feature-chat-crud ─────┐
    └─→ cpt-cf-mini-chat-feature-quota-billing ─┤
                                                ↓
                cpt-cf-mini-chat-feature-streaming-turns
                                                ↓
                cpt-cf-mini-chat-feature-attachments
                                                ↓
                cpt-cf-mini-chat-feature-turn-mutations
                                                ↓
                cpt-cf-mini-chat-feature-background-workers
```

**Dependency Rationale**:

- `cpt-cf-mini-chat-feature-chat-crud` requires `cpt-cf-mini-chat-feature-gear-foundation`: chats, messages and reactions need the schema, the authorization PEP, the error contract and the model catalog from the policy plugin.
- `cpt-cf-mini-chat-feature-quota-billing` requires `cpt-cf-mini-chat-feature-gear-foundation`: quota needs the `quota_usage` and `chat_turns` schema, the policy plugin (catalog, kill switches, limits, usage publication) and the outbox pipeline.
- `cpt-cf-mini-chat-feature-chat-crud` and `cpt-cf-mini-chat-feature-quota-billing` are independent of each other and can be developed in parallel.
- `cpt-cf-mini-chat-feature-streaming-turns` requires `cpt-cf-mini-chat-feature-chat-crud` and `cpt-cf-mini-chat-feature-quota-billing`: a turn runs inside an owner-scoped chat and must reserve before and settle after every provider call.
- `cpt-cf-mini-chat-feature-attachments` requires `cpt-cf-mini-chat-feature-chat-crud` and `cpt-cf-mini-chat-feature-streaming-turns`: uploads belong to a chat, and file search, image inputs, code interpreter files and citations are delivered through the turn pipeline.
- `cpt-cf-mini-chat-feature-turn-mutations` requires `cpt-cf-mini-chat-feature-streaming-turns` and `cpt-cf-mini-chat-feature-attachments`: retry and edit reuse the full send pipeline and carry forward attachment associations.
- `cpt-cf-mini-chat-feature-background-workers` requires `cpt-cf-mini-chat-feature-turn-mutations`, `cpt-cf-mini-chat-feature-attachments` and `cpt-cf-mini-chat-feature-quota-billing`: cleanup deletes provider files and vector stores created by attachments, the watchdog settles through the quota finalization path, and the summary commit must respect mutation-driven invalidation.
