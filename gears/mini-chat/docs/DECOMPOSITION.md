---
description: "Decomposes the Mini Chat gear design (gears/mini-chat/docs/DESIGN.md) into seven dependency-ordered, independently testable features, each traced to PRD, DESIGN, ADR and acceptance-criteria items."
---

# Decomposition: Mini Chat


<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation - HIGH](#21-gear-foundation---high)
  - [2.2 Chat Management - HIGH](#22-chat-management---high)
  - [2.3 Quota and Billing - HIGH](#23-quota-and-billing---high)
  - [2.4 Message Streaming - HIGH](#24-message-streaming---high)
  - [2.5 Attachments and File Search - HIGH](#25-attachments-and-file-search---high)
  - [2.6 Turn Mutations and Reactions - HIGH](#26-turn-mutations-and-reactions---high)
  - [2.7 Background Maintenance - HIGH](#27-background-maintenance---high)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-mini-chat-status-decomposition`

## 1. Overview

This document decomposes the Mini Chat technical design (`gears/mini-chat/docs/DESIGN.md`) into seven FEATURE entries. Each entry is traced to the PRD functional and non-functional requirements, the DESIGN principles, constraints, components, sequences, interfaces and database tables, the accepted ADRs (ADR-0001 to ADR-0010), and the mandatory checklist in `gears/mini-chat/docs/acceptance-criteria.md`. Every design element is owned by exactly one feature, so coverage is complete and no element is dropped silently. Elements that the design records as not implemented in P1 or deferred to P2+ (MCP, document summary, temporary chats, group chats) are attached to the closest feature and stated as deferred, not implemented.

**Decomposition strategy**:

- Vertical slices ordered by dependency. A shared foundation comes first (SDK crate, gear lifecycle, configuration, schema, error contract, authorization, model policy and audit plugins, provider registry, Models API). The slices that build on it follow: chat management, quota and billing, the streaming send-message pipeline, attachments and file search, turn mutations and reactions, and background maintenance.
- The quota and billing slice precedes the streaming slice because the design requires reserve-before-execute and exactly-once settlement for every provider call. The streaming slice consumes the reserve and settlement contracts instead of redefining them.
- Each feature has a verifiable definition of done in the acceptance-criteria items it owns. Each of those items is mandatory, so each is both implemented and tested.
- `gears/mini-chat/docs/` (PRD, DESIGN, ADRs, acceptance criteria) is the source of truth. Where `config/mini-chat.yaml` or platform documents under `docs/` disagree, `gears/mini-chat/docs/` wins, and configuration keys and defaults are those of DESIGN Appendix B.

**Crate layout (applies to all features)**:

- The SDK crate lives at `gears/mini-chat/mini-chat-sdk/` with package name `cf-gears-mini-chat-sdk` and library name `mini_chat_sdk`. It is a workspace member.
- The gear crate (`gears/mini-chat/mini-chat/`, package `cf-gears-mini-chat`, library `mini_chat`) depends on the SDK crate.
- The gear crate keeps exposing `mini_chat::infra::plugins::static_audit` and `mini_chat::infra::plugins::static_model_policy`, which the example server imports.
- The gear is delivered through `apps/cf-gears-example-server` built with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`. Its observable surface is HTTP/SSE, the gear database, and the requests sent to an OpenAI-compatible provider.

**Ordering at a glance**: `gear-foundation` first; `chat-management` and `quota-and-billing` next (parallel); then `message-streaming`; then `attachments-and-file-search`; then `turn-mutations-and-reactions`; `background-maintenance` last. See section 3 for the dependency graph.

## 2. Entries

### 2.1 [Gear Foundation](feature-gear-foundation/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-gear-foundation`

- **Purpose**: Provide everything every other feature relies on: the SDK crate and gear crate wiring, gear registration and lifecycle, configuration, the full P1 database schema, the canonical error contract, owner and tenant scoped authorization, the model catalog and policy and audit plugins, the provider registry with OAGW provisioning, and the read-only Models API. Without it no other feature can be implemented or tested over HTTP.

- **Depends On**: None

- **Scope**:
  - SDK crate `cf-gears-mini-chat-sdk` (library `mini_chat_sdk`) at `gears/mini-chat/mini-chat-sdk/`, added to the workspace, with the gear crate `cf-gears-mini-chat` (library `mini_chat`) depending on it. The plugin client contracts (model policy and audit) and shared types live in the SDK crate.
  - Gear registration and the `init`, `start` and `stop` lifecycle phases, REST route registration under `url_prefix`, and the OpenAPI declaration of the public API.
  - Gear configuration with the keys, defaults and validation of DESIGN Appendix B.1, including rejection of unknown keys and removed keys (ADR-0010).
  - Database schema and migrations for all P1 tables (SQLite and Postgres), the secure-ORM scoped persistence layer, and the outbox pipeline queues (usage, attachment cleanup, chat cleanup, thread summary, audit). Handlers of the queues belong to the features that own the behavior.
  - Canonical error contract (ADR-0004): REST errors as canonical Problem objects with status following the category, and the `{code, message}` shape reserved for the SSE error event.
  - Authorization as Policy Enforcement Point: per-operation authorization matrix, owner and tenant scoping of every resource through the access scope, fail-closed behavior on authorization failure, and the token scope handling.
  - Model catalog and policy snapshot access through the model-policy plugin (bundled `static_model_policy`) and the model-policy gateway; kill switches and per-user limits as supplied by the plugin.
  - Audit gateway and bundled audit plugin (`static_audit`), the audit outbox queue and its delivery, retry, drop and dead-letter semantics (ADR-0009). Emission of finalization and mutation events is done by the features that own those operations.
  - Provider registry, provider entries, tenant overrides and OAGW upstream and route provisioning at gear start with deferred retry, using credentials only by reference (ADR-0005). The per-provider adapters are completed in `cpt-cf-mini-chat-feature-message-streaming`.
  - Interim base-license gate (ADR-0008) at the api-gateway boundary.
  - Metrics infrastructure, request correlation, structured logging and tracing conventions used by all features.
  - Public Models API: list and get, returning only globally enabled catalog entries without internal fields. This realizes the Models API functional driver of the DESIGN (the DESIGN drivers table names that requirement, but the PRD defines no separate ID for it, so it is traced through `cpt-cf-mini-chat-interface-models-api`).
  - Actors served: `cpt-cf-mini-chat-actor-chat-user`, `cpt-cf-mini-chat-actor-admin` (aggregated usage and operational metrics only, never chat content), `cpt-cf-mini-chat-actor-cleanup-scheduler` (exercised in `cpt-cf-mini-chat-feature-background-maintenance`).
  - ADRs traced: `cpt-cf-mini-chat-adr-canonical-error-contract`, `cpt-cf-mini-chat-adr-multi-provider-adapters`, `cpt-cf-mini-chat-adr-mcp-deferred`, `cpt-cf-mini-chat-adr-quota-policy-scope` (policy and licensing parts), `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (audit transport part).

- **Out of scope**:
  - MCP server support, MCP tables and MCP endpoints: deferred, not implemented in P1 (ADR-0006). The five MCP requirements, the MCP interface, the MCP components, the three MCP tables, the MCP design section and the MCP use cases listed below are tracked here only as deferred.
  - PolicySnapshot cache, persistence and policy-change notification endpoint (ADR-0008).
  - Content redaction of audit events and chat-deletion audit events (ADR-0009).
  - Per-feature behavior built on top of the foundation (chat CRUD, quota, streaming, attachments, mutations, background work).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-license-gate`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-audit`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-tenant-isolation`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-authz-alignment`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-observability-supportability`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-server-registry` (deferred, not implemented; ADR-0006)
  - [ ] `p2` - `cpt-cf-mini-chat-fr-mcp-hub-discovery` (deferred, not implemented; ADR-0006)
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-role-access` (deferred, not implemented; ADR-0006)
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-tool-discovery` (deferred, not implemented; ADR-0006)
  - [ ] `p1` - `cpt-cf-mini-chat-fr-mcp-tool-execution` (deferred, not implemented; ADR-0006)

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-tenant-scoped`
  - [ ] `p1` - `cpt-cf-mini-chat-principle-owner-only-content`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-openai-compatible`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-no-credentials`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-license-gate`

- **Domain Model Entities**:
  - AuditEvent
  - ModelCatalogEntry and PolicySnapshot (as supplied by the model-policy plugin)
  - Provider entry and tenant override

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-service` (gear registration, lifecycle and the domain service as policy enforcement point; orchestration of features built on top)
  - [ ] `p1` - `cpt-cf-mini-chat-component-chat-store`
  - [ ] `p1` - `cpt-cf-mini-chat-component-authz-integration`
  - [ ] `p1` - `cpt-cf-mini-chat-design-authz-pep`
  - [ ] `p1` - `cpt-cf-mini-chat-design-model-catalog`
  - [ ] `p1` - `cpt-cf-mini-chat-component-mcp-pool` (deferred, not implemented; ADR-0006)
  - [ ] `p1` - `cpt-cf-mini-chat-component-mcp-service` (deferred, not implemented; ADR-0006)
  - [ ] `p1` - `cpt-cf-mini-chat-design-mcp-servers` (deferred, not implemented; ADR-0006)

- **API**:
  - `cpt-cf-mini-chat-interface-public-api` (route registration, OpenAPI, error contract, authorization matrix shared by all endpoints; each endpoint is owned by the feature named in its own entry)
  - `cpt-cf-mini-chat-interface-models-api`
  - GET /v1/models
  - GET /v1/models/{id}
  - `cpt-cf-mini-chat-interface-mcp-api` (deferred, not implemented; ADR-0006)

- **Sequences**:

  - None. The foundation introduces no request sequence of its own; gear start and OAGW provisioning are lifecycle behavior described in the DESIGN component model (gear lifecycle and OAGW provisioning).

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
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-tenant-closure-ref` (not required for chat content access; no local table is created)
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-mcp-servers` (deferred, not implemented; no migration creates it)
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-mcp-server-tools` (deferred, not implemented; no migration creates it)
  - [ ] `p1` - `cpt-cf-mini-chat-dbtable-role-mcp-servers` (deferred, not implemented; no migration creates it)

- **Use Cases (deferred)**:
  - `cpt-cf-mini-chat-usecase-mcp-tool-execution`, `cpt-cf-mini-chat-usecase-assign-mcp-server-role`, `cpt-cf-mini-chat-usecase-revoke-mcp-server-role` (deferred, not implemented; ADR-0006)

- **Acceptance Criteria Covered**:
  - Tenant and owner isolation enforced on every resource
  - Read-only model list/get reflects only enabled catalog entries, without exposing internal fields
  - All errors map to the canonical error contract, consistently across REST and streaming
  - Every operation is scoped to its owner; cross-tenant or foreign access is rejected consistently

### 2.2 [Chat Management](feature-chat-management/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-chat-management`

- **Purpose**: Let users create, read, list, rename and delete their chats with a model chosen at creation and locked for the life of the chat. This is the container every conversation, attachment and turn lives in, and it is testable over plain REST without any provider call.

- **Depends On**: `cpt-cf-mini-chat-feature-gear-foundation`

- **Scope**:
  - Create chat with optional title and optional model, validated against the enabled catalog; default model resolution; 201 with the chat detail and the location header.
  - Get chat (metadata and message count, no embedded messages), update title, list chats.
  - List filtering, ordering and cursor pagination with validation of malformed filter, order, cursor and limit input.
  - Chat ordering by most recent activity, with the activity timestamp updated by create, rename, delete and by message activity written by other features.
  - Model selection at creation and immutability of the model afterwards.
  - Chat DELETE as one transaction: soft-delete of the chat (it disappears from get and list), marking its attachments `cleanup_status = pending`, and enqueue of the durable chat-cleanup outbox message (chat-cleanup enqueue contract: `tenant_id`, `chat_id`, stable `system_request_id`, `reason`, `chat_deleted_at`). The chat-cleanup outbox handler that consumes this message is owned by `cpt-cf-mini-chat-feature-background-maintenance`; the `cleanup_status` state machine is owned by `cpt-cf-mini-chat-feature-attachments-and-file-search`.
  - Use case: `cpt-cf-mini-chat-usecase-delete-chat` (single owner; its asynchronous cleanup step is the chat-cleanup handler of `cpt-cf-mini-chat-feature-background-maintenance`, touchpoint: the chat-cleanup outbox message).
  - ADR traced: `cpt-cf-mini-chat-adr-group-chat-usage-attribution` (group chats deferred).

- **Out of scope**:
  - Temporary chats with 24-hour cleanup and the temporary toggle endpoint: deferred to P2, not implemented.
  - Group chats and chat sharing: deferred to P2+, not implemented (ADR-0003).
  - Message content, streaming, attachments (other features).
  - Provider resource cleanup after deletion: the chat-cleanup outbox handler, vector store cleanup ordering and retries (`cpt-cf-mini-chat-feature-background-maintenance`).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-chat-crud`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-model-selection`
  - [ ] `p2` - `cpt-cf-mini-chat-fr-temporary-chat` (deferred, not implemented)
  - [ ] `p2` - `cpt-cf-mini-chat-fr-group-chats` (deferred, not implemented; ADR-0003)

- **Design Principles Covered**:

  - None. The principles that apply to chats (`cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`) are owned by `cpt-cf-mini-chat-feature-gear-foundation` and verified here through chat isolation tests.

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-model-locked-per-chat`

- **Domain Model Entities**:
  - Chat

- **Design Components**:

  - None. The chat operations run on components owned by `cpt-cf-mini-chat-feature-gear-foundation` (domain service, persistence layer, authorization integration).

- **API**:
  - POST /v1/chats
  - GET /v1/chats
  - GET /v1/chats/{id}
  - PATCH /v1/chats/{id}
  - DELETE /v1/chats/{id}
  - POST /v1/chats/{id}:temporary (P2, deferred, not implemented)

- **Sequences**:

  - None. Chat CRUD has no sequence in the DESIGN; the deletion cleanup sequence belongs to `cpt-cf-mini-chat-feature-background-maintenance`, and this feature performs its synchronous first step (the soft-delete transaction with the outbox enqueue).

- **Data**:

  - None. The `chats` table is created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns its read and write behavior.

- **Use Cases (deferred)**:
  - `cpt-cf-mini-chat-usecase-temporary-chat-cleanup` (P2, deferred, not implemented)

- **Acceptance Criteria Covered**:
  - A chat's model is immutable once set
  - Create / get / list / update / delete lifecycle for chats, including model and title validation
  - List endpoint supports filtering, ordering, and pagination, with validation of malformed input
  - Chat ordering reflects most recent activity

### 2.3 [Quota and Billing](feature-quota-and-billing/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-quota-and-billing`

- **Purpose**: Bound and account for LLM cost. It provides credit arithmetic, token estimation, the preflight reserve with tier downgrade, CAS-guarded exactly-once settlement for every terminal outcome, reliable usage publication through the outbox, and the quota status API. It is built before streaming so that no provider call can happen without a reserve.

- **Depends On**: `cpt-cf-mini-chat-feature-gear-foundation`

- **Scope**:
  - Credit arithmetic with overflow protection, reserve and settlement glossary, two-tier throttling (daily and monthly periods, `total` and `tier:premium` buckets), period reset semantics.
  - Two-phase token estimation: input estimate, image and tool surcharges, reserved credits, output cap and limit checks.
  - Preflight reserve written before any outbound provider call, tier availability across all periods and buckets, downgrade cascade (premium to standard to reject with 429 `resource_exhausted`), effective model computed once.
  - Policy version persisted per turn and carried into the usage outbox payload.
  - Finalization settlement contract (settlement and usage-outbox enqueue, invoked inside the turn-finalization transaction composed by `cpt-cf-mini-chat-feature-message-streaming`): terminal outcome taxonomy, CAS on the turn state, deterministic actual and estimated settlement, billing outcome derivation, charged-token formula for aborted streams, reconciliation of non-terminal and terminal-error cases, first-terminal-wins race resolution, reserve-uncommitted invariant.
  - Usage outbox publication with dedupe keys, UUID normalization and enqueue validation, delivered to the model-policy plugin (`publish_usage`) exactly once per turn.
  - Daily web search and code interpreter quota contract: the preflight daily quota check against the `quota_usage` call counters (`web_search_calls`, `code_interpreter_calls`), the tool surcharge in the reservation, the counter increments at settlement, and the mapping of an exhausted daily quota to 429 `resource_exhausted` with the tool quota scope. The send pipeline invokes this contract; tool-start detection and the per-message limit are owned by `cpt-cf-mini-chat-feature-message-streaming`.
  - Warning thresholds, the quota status endpoint and `done.quota_warnings` inputs.
  - Cost metrics series and the quota-related alerts.
  - ADR traced: `cpt-cf-mini-chat-adr-quota-policy-scope`.

- **Out of scope**:
  - The HTTP/SSE send pipeline that invokes reserve and settlement (`cpt-cf-mini-chat-feature-message-streaming`).
  - Billing of knowledge-search agentic iterations, per-user daily image quota and image byte cap, PolicySnapshot cache and notification, tenant operational bucket for system tasks, `ai_chat` license feature (not implemented in P1; ADR-0008).
  - Per-user daily `file_search` limit (not implemented in P1; ADR-0007).
  - Group chat usage attribution (deferred; ADR-0003).
  - Centralized policy gRPC interface, credit status UI proxy, cross-service transactional guarantees (deferred to P2+).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-quota-enforcement`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-quota-billing-architecture`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-cost-metrics`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-cost-control`

- **Design Principles Covered**:

  - None. No design principle is specific to quota; cost bounds are expressed as constraints and requirements.

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-quota-before-outbound`

- **Domain Model Entities**:
  - QuotaUsage
  - UserLimits and tier definitions
  - TurnExecution billing state and usage event

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-quota-service`
  - [ ] `p1` - `cpt-cf-mini-chat-design-quota-usage-accounting`
  - [ ] `p1` - `cpt-cf-mini-chat-design-throttling-tiers`
  - [ ] `p1` - `cpt-cf-mini-chat-design-nonterminal-reconciliation`
  - [ ] `p1` - `cpt-cf-mini-chat-design-terminal-error-reconciliation`
  - [ ] `p1` - `cpt-cf-mini-chat-flow-quota-enforced-chat-turn`
  - [ ] `p1` - `cpt-cf-mini-chat-algo-quota-billing-token-estimation`
  - [ ] `p1` - `cpt-cf-mini-chat-dod-policy-version-per-turn`
  - [ ] `p1` - `cpt-cf-mini-chat-dod-preflight-reserve-and-downgrade`
  - [ ] `p1` - `cpt-cf-mini-chat-dod-cas-guarded-settlement-and-outbox`
  - [ ] `p1` - `cpt-cf-mini-chat-featstatus-quota-and-billing-implemented`

- **API**:
  - GET /v1/quota/status

- **Sequences**:

  - None. The quota flow is described by `cpt-cf-mini-chat-flow-quota-enforced-chat-turn` (listed under Design Components); the user-visible send sequence `cpt-cf-mini-chat-seq-send-message` is owned by `cpt-cf-mini-chat-feature-message-streaming`.

- **Data**:

  - None. The `quota_usage` table is created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns its reserve, commit and read behavior.

- **Acceptance Criteria Covered**:
  - Quota is checked before any outbound provider call
  - Quota status reporting is accurate and consistent with actual usage
  - Reserve-before-execute quota flow enforced on every provider call
  - Tier downgrade applied when a higher tier is exhausted
  - Credits and tokens are accounted correctly per model and tier
  - Every terminal outcome settles quota exactly once, using actual or estimated usage as appropriate
  - Usage is published reliably and exactly once per turn

### 2.4 [Message Streaming](feature-message-streaming/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-message-streaming`

- **Purpose**: Deliver the core product behavior: sending a message and receiving a streamed assistant response. It covers the message history API, the preflight and context assembly, the provider adapters and event translation, the SSE event contract, the turn lifecycle and state machine, idempotency and replay, the parallel-turn guard, turn status, cancellation, crash recovery, error mapping, the turn-finalization transaction, the web search tool, and generic built-in tool assembly and tool event reporting.

- **Depends On**: `cpt-cf-mini-chat-feature-gear-foundation`, `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-quota-and-billing`

- **Scope**:
  - Messages list API with filtering, ordering and cursor pagination, the always-present message response fields (identity, attachments array, reaction field), message count and chronological ordering across turns.
  - Send-message endpoint streaming SSE, correlated by a request id; preflight validation of content, attachments and limits before any provider call, returned as normal JSON errors without opening a stream. The preflight includes the `attachment_ids` invariant (same tenant, user and chat, status `ready`, unique), the send-time image guards (model vision capability check on the effective model after the downgrade cascade, `disable_images` kill switch on the stream path rejected as 400 feature-disabled, and the per-message image limit rejected as `TOO_MANY_IMAGES`), and the `message_attachments` insert inside the reserve transaction.
  - Context plan assembly in the normative order (system prompt, thread summary, recent history, user message, retrieval excerpts), deterministic truncation within the context budget for both the input message and the full assembled request, determinism and snapshot boundary, tool availability and guidance in the assembled request.
  - Provider adapters (`openai_responses`, `openai_chat_completions`, `vllm_responses`, `anthropic_messages`) in-process through OAGW (ADR-0001, ADR-0002), provider request metadata, provider event translation into the SSE event contract.
  - SSE event contract and ordering (stream started, delta, tool, citations, done, error, ping), the done event exposing usage and the quota and downgrade outcome without internal identifiers, terminal and sanitized error event, terminal emission guard, keepalive ping, SSE infrastructure requirements (no buffering, no compression, bounded channels).
  - Persistence of the assistant message and usage once the stream completes, content durability (completed implies replayable).
  - Turn-finalization transaction composition: after the provider terminal, one transaction persists the assistant message and usage, CAS-finalizes the turn, calls the settlement and usage-outbox contract of `cpt-cf-mini-chat-feature-quota-and-billing`, enqueues the turn-finalization audit event through the audit outbox queue, and evaluates the thread-summary trigger and, when it fires, enqueues the summary work through the thread-summary enqueue contract whose worker is owned by `cpt-cf-mini-chat-feature-background-maintenance`.
  - Turn lifecycle and state machine (running to completed, cancelled or failed), partial and null content handling on cancel or failure, user message persistence prerequisite.
  - Idempotency and replay by request id (no provider call, no quota change; conflicting reuse rejected; replay checked before the parallel-turn guard), parallel-turn guard (one running turn per chat), 409 recovery rule, turn status API for reconnect.
  - Cancellation and disconnect handling with bounded best-effort debit through the quota contract, cancellation observability.
  - Orphan watchdog with single-actor guarantee for turns stuck in running.
  - Error mapping and sanitization of provider-originated details, streaming error codes, consistent with the foundation error contract.
  - Web search tool: inclusion per request, `disable_web_search` kill switch and system prompt guard, citations and reporting.
  - Generic built-in tool assembly in the provider request (tools offered by other features, such as file_search and code interpreter, are added through the same assembly) and tool event reporting; tool-start detection and the per-message hard limit enforced mid-turn for web search and code interpreter calls, with the failed-turn outcome (`web_search_calls_exceeded`, `code_interpreter_calls_exceeded`); built-in tool calls per request bounded by the catalog model's `max_tool_calls`. Before the provider call the pipeline invokes the daily tool quota contract of `cpt-cf-mini-chat-feature-quota-and-billing`.
  - Runtime and consistency limitations accepted in P1 (ADR-0010).
  - Use cases: `cpt-cf-mini-chat-usecase-send-message`, `cpt-cf-mini-chat-usecase-reconnect-turn-status`.
  - ADRs traced: `cpt-cf-mini-chat-adr-llm-provider-as-library`, `cpt-cf-mini-chat-adr-internal-transport`, `cpt-cf-mini-chat-adr-multi-provider-adapters` (adapters), `cpt-cf-mini-chat-adr-runtime-consistency-limitations`.

- **Out of scope**:
  - Document upload, indexing and the file_search tool availability (`cpt-cf-mini-chat-feature-attachments-and-file-search`).
  - Retry, edit and delete of the last turn (`cpt-cf-mini-chat-feature-turn-mutations-and-reactions`).
  - Thread summary generation by the summary worker (`cpt-cf-mini-chat-feature-background-maintenance`); this feature consumes an existing summary in context assembly and only evaluates the trigger and enqueues the summary work in the finalization transaction.
  - Credit arithmetic and settlement internals, and the daily web search and code interpreter quota check, call counters and 429 mapping (`cpt-cf-mini-chat-feature-quota-and-billing`); this feature invokes them.
  - Code interpreter upload routing, inclusion rules (ready code-interpreter attachment, model support) and `disable_code_interpreter` gating (`cpt-cf-mini-chat-feature-attachments-and-file-search`).
  - Mid-stream quota abort (not supported by design), DB-server clock for the watchdog, cross-column CHECK constraints and an immutable replay done payload (not implemented in P1; ADR-0010).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-chat-streaming`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-conversation-history`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-streaming-cancellation`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-token-budget`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-web-search`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-ux-recovery`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-streaming-latency`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-resilience-recovery`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-streaming-first`

- **Design Constraints Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-constraint-context-budget`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-no-buffering`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-bounded-channels`
  - [ ] `p1` - `cpt-cf-mini-chat-constraint-model-image-capability`

- **Domain Model Entities**:
  - Message
  - ContextPlan
  - Turn (execution turn)

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-component-llm-provider`
  - [ ] `p1` - `cpt-cf-mini-chat-component-orphan-watchdog`
  - `cpt-cf-mini-chat-design-turn-lifecycle` (design section without task tracking: turn state model, crash recovery, orphan handling, idempotency and parallel turn policy)
  - [ ] `p1` - `cpt-cf-mini-chat-design-web-search`

- **API**:
  - GET /v1/chats/{id}/messages
  - POST /v1/chats/{id}/messages:stream
  - GET /v1/chats/{id}/turns/{request_id}
  - `cpt-cf-mini-chat-interface-turn-status`
  - `cpt-cf-mini-chat-contract-sse-streaming`

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-send-message`
  - [ ] `p1` - `cpt-cf-mini-chat-seq-cancellation`

- **Data**:

  - None. The `messages` and `chat_turns` tables are created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their write and read behavior for the send pipeline.

- **Acceptance Criteria Covered**:
  - Context window budget enforced, for both the input message and the full assembled request
  - Streaming responses are never buffered before relaying
  - List messages with filtering, ordering, and pagination
  - Message response contract: identity, attachments, and reaction fields are always consistently present
  - Message count and chronological ordering are tracked correctly across turns
  - Send-message endpoint streams a response, correlated by a request id
  - Preflight validation (content, attachments, limits) runs before any provider call
  - Assistant message and usage are persisted once a stream completes
  - Full streaming event contract (start, delta, tool activity, citations, completion, error, keepalive) and its ordering
  - Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers
  - Error event is terminal and carries a sanitized message
  - Replaying a known request id returns the stored result without side effects (no provider call, no quota change)
  - Conflicting reuse of a request id across turn states is rejected consistently
  - Replay is checked before the parallel-turn guard
  - Only one turn may run per chat at a time
  - A new turn is accepted once the previous one reaches a terminal state
  - Turn state machine (running → completed / cancelled / failed) is consistent end to end
  - Partial and null-content cases on cancellation or failure are handled correctly
  - System prompt, thread summary, and recent history are assembled and truncated deterministically within budget
  - Tool availability and guidance are reflected correctly in the assembled request
  - Provider-originated error details are sanitized before reaching the client
  - Web search tool use is reported, cited, accounted, and quota-limited correctly (the daily quota limit consumes the quota contract of `cpt-cf-mini-chat-feature-quota-and-billing`)

### 2.5 [Attachments and File Search](feature-attachments-and-file-search/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-attachments-and-file-search`

- **Purpose**: Let users add documents and images to a chat and ask questions about them. It covers upload, status, deletion, validation and limits, asynchronous indexing into the chat's dedicated vector store, availability of attachments to provider tools, retrieval scope, citation resolution, and recovery of abandoned uploads.

- **Depends On**: `cpt-cf-mini-chat-feature-message-streaming`

- **Scope**:
  - Upload (documents and images), get status and metadata, delete attachment, with size, type and per-chat limit validation at upload time and the `ready` status lifecycle. Touchpoint: validation of `attachment_ids` in the send pipeline (same tenant, user and chat, status `ready`, unique) is owned by `cpt-cf-mini-chat-feature-message-streaming`, which consumes the attachment status.
  - Image upload: supported formats and size, thumbnail handling, kill switches for images and code interpreter at upload time (the upload path checks only `disable_images`, with no vision check), synchronous upload (ADR-0007). Touchpoint: the send-time image guards (vision capability on the effective model after the downgrade cascade, `disable_images` on the stream path, per-message image limit) are owned by `cpt-cf-mini-chat-feature-message-streaming`.
  - Asynchronous indexing lifecycle (uploaded to ready or failed), including provider failure and timeout handling, with the document still being indexed at the request deadline.
  - Dedicated per-chat vector store created on first document upload, vector store scope, retrieval invariants, RAG context assembly rules and RAG quality and scale controls (retrieval-k, token limits, per-chat limits).
  - File search tool availability and retrieval scope (entire chat vector store); there is no dedicated per-turn file_search call limit, file_search calls are bounded only by the catalog model's `max_tool_calls` shared by all built-in tools (sent only by the OpenAI Responses adapter; ADR-0007); citation file id and title resolution without exposing provider identifiers.
  - Attachments made available to the relevant provider tools: file_search for documents, multimodal input for images, and the optional `search_knowledge` function tool (disabled by default, mutually exclusive with file_search).
  - Code interpreter: XLSX upload routing (`for_code_interpreter`), inclusion of the `code_interpreter` tool with the ready attachment file ids when the chat has a ready code-interpreter attachment and the effective model supports it, and `disable_code_interpreter` gating (rejection of code-interpreter-only uploads, silent omission of the tool at stream time). The tool is added through the generic tool assembly of `cpt-cf-mini-chat-feature-message-streaming`.
  - Attachment mutability and deletion rules, the upload reaper and recovery of abandoned uploads under failure.
  - The single `attachment_cleanup` outbox handler (queue `mini-chat.attachment_cleanup`) serving user attachment deletion, upload-reaper abandoned uploads and background indexing failures, which acks without action when the parent chat is soft-deleted (chat cleanup then owns the files), and the attachment `cleanup_status` state machine (`pending` to `done` or `failed`, attempts and retry budget).
  - Use cases: `cpt-cf-mini-chat-usecase-upload-document`, `cpt-cf-mini-chat-usecase-upload-image`, `cpt-cf-mini-chat-usecase-doc-search`.
  - ADR traced: `cpt-cf-mini-chat-adr-document-retrieval-scope`.

- **Out of scope**:
  - Document summary on upload: deferred, not implemented in P1 (the summary fields stay null; ADR-0007).
  - Maximum indexed chunks per chat, a dedicated per-turn `file_search` call limit, per-user daily `file_search` limit, immediate exclusion of a deleted document from file_search, and document search for Anthropic chats (not implemented in P1; ADR-0007).
  - The chat-cleanup outbox handler and provider vector store deletion on chat deletion (`cpt-cf-mini-chat-feature-background-maintenance`).
  - Sending the message that uses the attachments (`cpt-cf-mini-chat-feature-message-streaming`).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-file-upload`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-image-upload`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-file-search`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-code-interpreter`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-per-chat-doc-limits`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-attachment-deletion`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-doc-summary` (deferred, not implemented; ADR-0007)
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-rag-scalability`

- **Design Principles Covered**:

  - None. No design principle is specific to attachments; tenant and owner scoping of attachments is owned by `cpt-cf-mini-chat-feature-gear-foundation`.

- **Design Constraints Covered**:

  - None. The model image capability constraint is a send-time guard owned by `cpt-cf-mini-chat-feature-message-streaming`.

- **Domain Model Entities**:
  - Attachment (including its cleanup status)
  - ChatVectorStore
  - Attachment cleanup outbox message

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-design-vector-store-scope`
  - [ ] `p1` - `cpt-cf-mini-chat-design-rag-quality-controls`
  - [ ] `p1` - `cpt-cf-mini-chat-design-code-interpreter`
  - [ ] `p1` - `cpt-cf-mini-chat-design-code-interpreter-config`

- **API**:
  - POST /v1/chats/{id}/attachments
  - GET /v1/chats/{id}/attachments/{attachment_id}
  - DELETE /v1/chats/{id}/attachments/{attachment_id}

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-file-upload`

- **Data**:

  - None. The `attachments`, `message_attachments` and `chat_vector_stores` tables are created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their lifecycle behavior.

- **Acceptance Criteria Covered**:
  - Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation
  - Asynchronous indexing lifecycle, including provider failure and timeout handling
  - Attachments are correctly made available to the relevant provider tools
  - Cleanup and abandoned-upload recovery behave correctly under failure

### 2.6 [Turn Mutations and Reactions](feature-turn-mutations-and-reactions/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`

- **Purpose**: Let users correct a conversation tail (retry, edit or delete the latest terminal turn) and give like or dislike feedback on assistant messages. Mutations reuse the full send pipeline so quota, context budget and attachment rules apply unchanged.

- **Depends On**: `cpt-cf-mini-chat-feature-message-streaming`, `cpt-cf-mini-chat-feature-attachments-and-file-search`

- **Scope**:
  - Retry, edit and delete restricted to the latest turn in a terminal state (tail-only, linear conversation).
  - A mutation passes through the complete send pipeline (quota, context budget, attachment checks) and receives a new request id.
  - Deterministic resolution of concurrent mutations.
  - Carry-forward of attachment and tool-usage history to the mutated turn, exclusion of deleted attachments (attachment rules of `cpt-cf-mini-chat-feature-attachments-and-file-search`); retry and edit reuse the send-time image guards of the send pipeline owned by `cpt-cf-mini-chat-feature-message-streaming`.
  - Summary interaction on mutation: in the retry, edit and delete transaction, deletion of the chat's `thread_summaries` row and clearing of `is_compressed` on the chat's messages when the summary frontier covers the mutated turn (DESIGN "Summary Interaction on Turn Mutation"). The summary worker's commit check against a concurrent mutation is owned by `cpt-cf-mini-chat-feature-background-maintenance`.
  - Audit events for turn retry, edit and delete through the audit outbox queue.
  - Message reactions: set and remove like or dislike on assistant messages only, idempotently, one reaction per user per message.
  - Use cases: `cpt-cf-mini-chat-usecase-retry-turn`, `cpt-cf-mini-chat-usecase-edit-turn`, `cpt-cf-mini-chat-usecase-delete-turn`.

- **Out of scope**:
  - Mutation of arbitrary historical messages, thread branching and multi-version conversations (deferred to P2+).
  - Full audit content and redaction (not implemented in P1; ADR-0009).
  - Reactions on user messages.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-turn-mutations`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-message-reactions`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-linear-conversation`

- **Design Constraints Covered**:

  - None. The constraints that bind mutations (context budget, quota before outbound, model lock) are owned by other features and verified here by mutation tests.

- **Domain Model Entities**:
  - MessageReaction
  - Turn mutation (retry, edit, delete)

- **Design Components**:

  - [ ] `p1` - `cpt-cf-mini-chat-design-turn-mutations`

- **API**:
  - POST /v1/chats/{id}/turns/{request_id}/retry
  - PATCH /v1/chats/{id}/turns/{request_id}
  - DELETE /v1/chats/{id}/turns/{request_id}
  - PUT /v1/chats/{id}/messages/{msg_id}/reaction
  - DELETE /v1/chats/{id}/messages/{msg_id}/reaction
  - `cpt-cf-mini-chat-interface-message-reaction`

- **Sequences**:

  - None. Mutations reuse `cpt-cf-mini-chat-seq-send-message` (owned by `cpt-cf-mini-chat-feature-message-streaming`) and are specified by the turn mutation rules in the DESIGN.

- **Data**:

  - None. The `chat_turns`, `messages` and `message_reactions` tables are created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their mutation and reaction behavior.

- **Acceptance Criteria Covered**:
  - Retry / edit / delete act only on the latest, terminal turn
  - A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id
  - Concurrent mutations resolve deterministically
  - Mutated turns correctly carry forward attachment and tool-usage history
  - Set/remove reaction on assistant messages only, idempotently

### 2.7 [Background Maintenance](feature-background-maintenance/) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-feature-background-maintenance`

- **Purpose**: Keep long conversations affordable and deleted data cleaned up without blocking users. It provides outbox-driven thread summary generation with retry and invalidation, and the reliable background cleanup of provider-side resources after a chat is deleted.

- **Depends On**: `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-message-streaming`, `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`, `cpt-cf-mini-chat-feature-attachments-and-file-search`

- **Scope**:
  - Thread summary: the summary worker (outbox handler) consuming the work that `cpt-cf-mini-chat-feature-message-streaming` enqueues in the turn-finalization transaction, its execution stages and invariants, stable range and commit invariant (including the check that the target frontier message was not soft-deleted by a concurrent mutation), failure and retry behavior, system task isolation (no turn record, no per-user settlement). Trigger evaluation and the in-transaction enqueue are owned by `cpt-cf-mini-chat-feature-message-streaming`; summary deletion on turn mutation is owned by `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`.
  - Chat deletion cleanup: the chat-cleanup outbox handler (queue `mini-chat.chat_cleanup`) consuming the message enqueued by the chat DELETE transaction of `cpt-cf-mini-chat-feature-chat-management`, cleanup responsibility boundaries with the attachment-delete path, payload execution semantics, provider file deletion for the deleted chat's attachments, vector store cleanup ordering, retries and additional invariants.
  - Cross-feature dependency: the shared `attachments.cleanup_status` column and its state machine are owned by `cpt-cf-mini-chat-feature-attachments-and-file-search`; the chat-cleanup handler applies its transitions to the attachments of the deleted chat.
  - Retention behavior: soft-deleted rows stay in the database; provider files and vector stores are removed by the cleanup handlers.
  - Use case touchpoint: the asynchronous cleanup step of `cpt-cf-mini-chat-usecase-delete-chat` (the use case is owned by `cpt-cf-mini-chat-feature-chat-management`).
  - ADR traced: `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (cleanup and retention parts).

- **Out of scope**:
  - Hard-purge of soft-deleted rows, chat-deletion audit event and cancelling a running turn on chat deletion (not implemented in P1; ADR-0009).
  - The `attachment_cleanup` outbox handler and the attachment `cleanup_status` state machine (`cpt-cf-mini-chat-feature-attachments-and-file-search`).
  - Temporary chat 24-hour cleanup (deferred to P2).
  - Document summary generation (not implemented; ADR-0007).
  - Tenant operational bucket, audit and kill-switch checks for system tasks (not implemented in P1; ADR-0008).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-fr-thread-summary`
  - [ ] `p1` - `cpt-cf-mini-chat-fr-chat-deletion-cleanup`
  - [ ] `p1` - `cpt-cf-mini-chat-nfr-data-retention`

- **Design Principles Covered**:

  - [ ] `p1` - `cpt-cf-mini-chat-principle-summary-over-history`

- **Design Constraints Covered**:

  - None. Constraints touching background work (quota before outbound, no credentials) are owned by other features and apply to summary calls without new constraints.

- **Domain Model Entities**:
  - ThreadSummary
  - Chat cleanup outbox message

- **Design Components**:

  - None. The workers and outbox handlers run on components owned by `cpt-cf-mini-chat-feature-gear-foundation` and `cpt-cf-mini-chat-feature-message-streaming`.

- **API**:
  - None. This feature is driven by the outbox and by existing endpoints (chat deletion in `cpt-cf-mini-chat-feature-chat-management`, message sending in `cpt-cf-mini-chat-feature-message-streaming`) and exposes no endpoint of its own.

- **Sequences**:

  - [ ] `p1` - `cpt-cf-mini-chat-seq-thread-summary`
  - [ ] `p1` - `cpt-cf-mini-chat-seq-chat-deletion-cleanup`

- **Data**:

  - None. The `thread_summaries` and `chat_vector_stores` tables are created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their summary and cleanup behavior.

- **Acceptance Criteria Covered**:
  - Chat deletion triggers reliable background cleanup of provider-side resources
  - Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly

---

## 3. Feature Dependencies

```text
cpt-cf-mini-chat-feature-gear-foundation
    ↓
    ├─→ cpt-cf-mini-chat-feature-chat-management ───────────┐
    └─→ cpt-cf-mini-chat-feature-quota-and-billing ─────────┤
                                                            ↓
                                    cpt-cf-mini-chat-feature-message-streaming
                                                            ↓
                                    cpt-cf-mini-chat-feature-attachments-and-file-search
                                                            ↓
                                    cpt-cf-mini-chat-feature-turn-mutations-and-reactions
                                                            ↓
                                    cpt-cf-mini-chat-feature-background-maintenance
```

**Dependency Rationale**:

- `cpt-cf-mini-chat-feature-chat-management` requires `cpt-cf-mini-chat-feature-gear-foundation`: chats need the schema, configuration, authorization scoping, error contract and model catalog.
- `cpt-cf-mini-chat-feature-quota-and-billing` requires `cpt-cf-mini-chat-feature-gear-foundation`: reserve, settlement and usage publication need the `quota_usage` and `chat_turns` schema, the outbox pipeline and the model-policy plugin.
- `cpt-cf-mini-chat-feature-chat-management` and `cpt-cf-mini-chat-feature-quota-and-billing` are independent of each other and can be developed in parallel.
- `cpt-cf-mini-chat-feature-message-streaming` requires `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-quota-and-billing` and `cpt-cf-mini-chat-feature-gear-foundation`: a turn needs an existing chat with a locked model, must reserve quota before any provider call and settle exactly once, and runs on the provider registry and error contract.
- `cpt-cf-mini-chat-feature-attachments-and-file-search` requires `cpt-cf-mini-chat-feature-message-streaming`: attachment references are validated in the send pipeline, and the file_search, code interpreter and image inputs are added through the generic tool assembly and context assembly of the send pipeline.
- `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` requires `cpt-cf-mini-chat-feature-message-streaming` and `cpt-cf-mini-chat-feature-attachments-and-file-search`: a mutation re-enters the send pipeline and acts on a terminal turn, reactions attach to persisted assistant messages, and retry and edit carry attachments forward and exclude deleted attachments, which needs the attachment lifecycle, while the image guards come from the send pipeline. The two features are therefore sequential, not parallel.
- `cpt-cf-mini-chat-feature-background-maintenance` requires `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-message-streaming`, `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` and `cpt-cf-mini-chat-feature-attachments-and-file-search`: cleanup acts on soft-deleted chats and on attachment and vector store state, and thread summaries are triggered by the send pipeline and invalidated by turn mutations.
