---
description: "Specifies the Message Streaming feature of the mini-chat gear: the messages list API, the send-message SSE pipeline with preflight, attachment and image guards, context assembly and truncation, generic tool assembly with web search, the provider adapters and event translation through OAGW, the SSE event contract, the turn-finalization transaction, idempotent replay, the parallel-turn guard, the turn status API, cancellation on disconnect, the orphan watchdog and the sanitized error mapping."
---

# Feature: Message Streaming


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat User Lists Messages](#chat-user-lists-messages)
  - [Chat User Sends a Message and Receives a Streamed Response](#chat-user-sends-a-message-and-receives-a-streamed-response)
  - [Chat User Resends a Known Request Id](#chat-user-resends-a-known-request-id)
  - [Chat User Disconnects During a Stream](#chat-user-disconnects-during-a-stream)
  - [Chat User Checks Turn Status and Recovers](#chat-user-checks-turn-status-and-recovers)
  - [Chat User Sends a Turn With Web Search](#chat-user-sends-a-turn-with-web-search)
  - [Admin Observes Streaming Health](#admin-observes-streaming-health)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Send Request Validation](#send-request-validation)
  - [Turn Admission Checks](#turn-admission-checks)
  - [Replay Stream](#replay-stream)
  - [Send Preflight](#send-preflight)
  - [Image Guards](#image-guards)
  - [Context Budget](#context-budget)
  - [Context Plan Assembly and Truncation](#context-plan-assembly-and-truncation)
  - [Tool Assembly](#tool-assembly)
  - [Reserve Transaction](#reserve-transaction)
  - [Attachment Validation](#attachment-validation)
  - [Provider Call Through OAGW](#provider-call-through-oagw)
  - [OpenAI Responses Request Construction](#openai-responses-request-construction)
  - [Provider Adapter Kinds](#provider-adapter-kinds)
  - [Provider Event Translation](#provider-event-translation)
  - [SSE Relay](#sse-relay)
  - [Tool Event Accounting and Mid-Turn Limits](#tool-event-accounting-and-mid-turn-limits)
  - [Citation Mapping](#citation-mapping)
  - [Turn Finalization](#turn-finalization)
  - [Terminal Event Emission](#terminal-event-emission)
  - [Turn Audit Event](#turn-audit-event)
  - [Thread Summary Trigger and Enqueue](#thread-summary-trigger-and-enqueue)
  - [Provider Error Mapping and Sanitization](#provider-error-mapping-and-sanitization)
  - [Send Pipeline Error Contract](#send-pipeline-error-contract)
  - [Turn Status Projection](#turn-status-projection)
  - [Message List Query](#message-list-query)
  - [Message Projection](#message-projection)
  - [Message Timestamps, Order and Count](#message-timestamps-order-and-count)
  - [Orphan Watchdog](#orphan-watchdog)
  - [Streaming Metrics](#streaming-metrics)
  - [Streaming Configuration](#streaming-configuration)
- [4. States (CDSL)](#4-states-cdsl)
  - [Turn Lifecycle State Machine](#turn-lifecycle-state-machine)
  - [SSE Stream State Machine](#sse-stream-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Messages List API](#messages-list-api)
  - [Message Order and Count Across Turns](#message-order-and-count-across-turns)
  - [Send Endpoint, Validation and Preflight](#send-endpoint-validation-and-preflight)
  - [Reserve Transaction and Attachment Validation](#reserve-transaction-and-attachment-validation)
  - [Context Assembly Within Budget](#context-assembly-within-budget)
  - [Tool Assembly, Tool Events and Web Search](#tool-assembly-tool-events-and-web-search)
  - [Provider Adapters Through OAGW](#provider-adapters-through-oagw)
  - [SSE Event Contract and Relay](#sse-event-contract-and-relay)
  - [Turn Finalization Transaction](#turn-finalization-transaction)
  - [Turn Lifecycle and Content Cases](#turn-lifecycle-and-content-cases)
  - [Idempotency, Replay and Parallel-Turn Guard](#idempotency-replay-and-parallel-turn-guard)
  - [Cancellation on Client Disconnect](#cancellation-on-client-disconnect)
  - [Turn Status API](#turn-status-api)
  - [Orphan Watchdog Recovery](#orphan-watchdog-recovery)
  - [Error Mapping and Sanitization](#error-mapping-and-sanitization)
  - [Streaming Observability and Configuration](#streaming-observability-and-configuration)
  - [Deferred and Not-Implemented Scope Stays Unimplemented](#deferred-and-not-implemented-scope-stays-unimplemented)
  - [Test Layering, Traceability and Feature-Local Deviations](#test-layering-traceability-and-feature-local-deviations)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-message-streaming-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-message-streaming`

<!--
=============================================================================
FEATURE SPECIFICATION: Message Streaming (mini-chat)
Source entry: DECOMPOSITION section 2.4 (gears/mini-chat/docs/DECOMPOSITION.md)
Precedence: gears/mini-chat/docs/ (PRD, DESIGN, ADR, acceptance-criteria) wins over
config/mini-chat.yaml and over platform documents under docs/ wherever they disagree.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

The Message Streaming feature delivers the core product behavior of the `mini-chat` gear: a user sends a message to a chat and receives the assistant response as a Server-Sent Events stream. It covers the messages list API, the send pipeline (request validation, idempotent replay, the parallel-turn guard, the quota preflight call, attachment and image guards, context assembly and truncation, generic tool assembly, provider resolution and the reserve transaction), the four provider adapters reached in-process through OAGW, provider event translation into the stable SSE contract, the turn-finalization transaction, the turn state machine, the turn status API, cancellation on client disconnect, the orphan watchdog, web search reporting and citations, and the sanitized error contract for REST and streaming.

### 1.2 Purpose

The PRD requires real-time streamed answers, persistent conversation history, safe recovery after disconnects and crashes, a bounded context per request, and web search as an explicitly enabled tool. The DESIGN fixes the stable SSE protocol, the order of the pipeline checks, the reserve-before-execute contract with `cpt-cf-mini-chat-feature-quota-and-billing`, the first-terminal-wins finalization and the orphan recovery path. This feature composes those contracts into the user-visible send and read paths and owns every write of `messages`, `message_attachments` and `chat_turns` made by a user turn.

Scope boundary: credit arithmetic, the preflight cascade, the daily web search and code interpreter quota check, the reserve write and the settlement are contracts of `cpt-cf-mini-chat-feature-quota-and-billing` that this feature calls. The inclusion rules of the `file_search`, `code_interpreter` and `search_knowledge` tools, image upload and citation file-id resolution belong to `cpt-cf-mini-chat-feature-attachments-and-file-search` and plug into the generic tool assembly, input assembly and citation mapping of this feature. Retry and edit reuse this pipeline and belong to `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`. The thread-summary worker belongs to `cpt-cf-mini-chat-feature-background-maintenance`; this feature evaluates the trigger and enqueues the work.

**Requirements**: `cpt-cf-mini-chat-fr-chat-streaming`, `cpt-cf-mini-chat-fr-conversation-history`, `cpt-cf-mini-chat-fr-streaming-cancellation`, `cpt-cf-mini-chat-fr-token-budget`, `cpt-cf-mini-chat-fr-web-search`, `cpt-cf-mini-chat-fr-ux-recovery`, `cpt-cf-mini-chat-nfr-streaming-latency`, `cpt-cf-mini-chat-nfr-resilience-recovery`

**Principles**: `cpt-cf-mini-chat-principle-streaming-first`

**Constraints**: `cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-constraint-no-buffering`, `cpt-cf-mini-chat-constraint-bounded-channels`, `cpt-cf-mini-chat-constraint-model-image-capability`. The feature also preserves `cpt-cf-mini-chat-constraint-quota-before-outbound` (owned by `cpt-cf-mini-chat-feature-quota-and-billing`), `cpt-cf-mini-chat-constraint-no-credentials` and `cpt-cf-mini-chat-constraint-model-locked-per-chat` on every send.

**Design elements**: `cpt-cf-mini-chat-component-llm-provider`, `cpt-cf-mini-chat-component-orphan-watchdog`, `cpt-cf-mini-chat-design-turn-lifecycle` (turn state model, crash recovery, orphan handling, idempotency and parallel turn policy), `cpt-cf-mini-chat-design-web-search`, `cpt-cf-mini-chat-interface-turn-status`, `cpt-cf-mini-chat-contract-sse-streaming`, `cpt-cf-mini-chat-seq-send-message`, `cpt-cf-mini-chat-seq-cancellation`.

**Database tables used (created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their write and read behavior for the send pipeline)**: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-message-attachments`. Read only: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-message-reactions`. The `quota_usage` rows are changed only through the contracts of `cpt-cf-mini-chat-feature-quota-and-billing`.

**Domain entities**: `Message`, `ContextPlan`, `Turn` (execution turn), the stream event set and `TurnAuditEvent`.

**ADRs traced**: `cpt-cf-mini-chat-adr-llm-provider-as-library` (in-process `llm_provider`, in-process cancellation), `cpt-cf-mini-chat-adr-internal-transport` (HTTP with SSE passthrough through OAGW), `cpt-cf-mini-chat-adr-multi-provider-adapters` (four adapters, per-adapter tool support, provisioned OAGW upstreams), `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (ping rule, `stream_interrupted`, replay payload, separate orphan finalization, application clock), `cpt-cf-mini-chat-adr-canonical-error-contract` (Problem shape, SSE `{code, message}`), `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (turn audit event types and empty content fields), `cpt-cf-mini-chat-adr-document-retrieval-scope` (`max_tool_calls`, no Anthropic `file_search`), `cpt-cf-mini-chat-adr-quota-policy-scope` (no mid-stream quota abort, knowledge-search iteration billing not implemented).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Lists the messages of a chat, sends a message and receives the SSE stream, enables web search per message, resends a known `request_id` to recover a completed answer, disconnects (which cancels the turn), and reads the authoritative turn state through the turn status API after a disconnect or a 409. |
| `cpt-cf-mini-chat-actor-admin` | Observes streaming, cancellation and orphan watchdog health through the metric series of this feature; never sees chat content and never sees tenant or user identifiers in metric labels. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) section 2.4 (`Message Streaming`)
- **Acceptance criteria**: [acceptance-criteria.md](../acceptance-criteria.md)
- **ADRs (all accepted)**: [ADR-0001](../ADR/0001-cpt-cf-mini-chat-adr-llm-provider-as-library.md), [ADR-0002](../ADR/0002-cpt-cf-mini-chat-adr-internal-transport.md), [ADR-0004](../ADR/0004-cpt-cf-mini-chat-adr-canonical-error-contract.md), [ADR-0005](../ADR/0005-cpt-cf-mini-chat-adr-multi-provider-adapters.md), [ADR-0007](../ADR/0007-cpt-cf-mini-chat-adr-document-retrieval-scope.md), [ADR-0008](../ADR/0008-cpt-cf-mini-chat-adr-quota-policy-scope.md), [ADR-0009](../ADR/0009-cpt-cf-mini-chat-adr-data-lifecycle-audit-scope.md), [ADR-0010](../ADR/0010-cpt-cf-mini-chat-adr-runtime-consistency-limitations.md)
- **Reference OpenAPI** (operations `mini_chat.list_messages`, `mini_chat.stream_message`, `mini_chat.get_turn`; schemas `Page_MiniChatMessageDto`, `MiniChatMessageDto`, `AttachmentSummaryDto`, `ImgThumbnailDto`, `StreamMessageRequest`, `WebSearchConfig`, `MiniChatSseEvent`, `StreamStartedData`, `ThreadSummaryInfo`, `DeltaData`, `DeltaKind`, `ToolData`, `ToolPhase`, `CitationsData`, `Citation`, `TextSpan`, `DoneData`, `Usage`, `QuotaDecisionKind`, `QuotaWarning`, `ErrorData`, `PingData`, `TurnStatusResponse`, `TurnStatusState`): `.cf-studio/.cache/mini-chat/openapi-mini-chat.json`
- **Cross-feature implementation notes**: `.cf-studio/.cache/mini-chat/implementation-notes.md` items 1 (SQLite timestamp ordering), 3 (repository runner and post-commit wakes), 4 (PEP, 404 masking), 5 (OAGW proxy calls under the S2S context), 6 (domain layering), 7 (outbox handler placeholders), 8 and 10 (quota caller contracts and `started_at`)
- **Sibling features**: [0001-gear-foundation.md](./0001-gear-foundation.md) (request admission, PEP and authorization matrix, error mapping, scoped persistence, outbox queues and transactional enqueue, audit delivery, policy snapshot access, catalog rules, provider registry, OAGW provisioning, leader elector, configuration validation), [0002-chat-management.md](./0002-chat-management.md) (owner-scoped chat load, the message-activity contract on `chats.updated_at`, `message_count`, list query validation), [0003-quota-and-billing.md](./0003-quota-and-billing.md) (`QuotaService` preflight, reserve write, finalization settlement, usage event, quota warnings, rejection mapping)
- **Dependencies**: `cpt-cf-mini-chat-feature-gear-foundation`, `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-quota-and-billing`. Features that build on this one: `cpt-cf-mini-chat-feature-attachments-and-file-search` (tool contributions, image input, citation resolution), `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` (retry and edit run this pipeline), `cpt-cf-mini-chat-feature-background-maintenance` (thread-summary worker consuming the enqueued work, non-streaming summary call through the adapters).

**Normative value sources** (the DESIGN holds the exact values; this document states the values it must preserve and names the DESIGN heading that carries the rest):

| Topic | DESIGN location (section and heading) |
|-------|---------------------------------------|
| Streaming-first, linear conversation, context budget, no buffering, bounded channels, model lock, image capability, kill switches | 2.1 `Streaming-First`, `Linear Conversation Model`; 2.2 `Model Capability Constraint (Images)`, `Downgrade Decision Matrix`, `Context Window Budget`, `No Buffering`, `Bounded Channels`, `Model Locked Per Chat`, `Quota Before Outbound` |
| `llm_provider`, adapters, tool support per adapter, OAGW provisioning | 3.2 `Component Model` (`llm_provider`, `OAGW provisioning`); 3.5 `LLM Provider (OpenAI / Azure OpenAI)`, `Provider API Mapping` |
| Messages list contract | 3.3 `List Messages` |
| Send request body, `request_id` semantics, retrieval model, attachment validation, idempotency, parallel guard, replay invariant | 3.3 `Streaming Contract`, `Attachment Preflight Validation Invariant (P1)` |
| Turn status API and state mapping | 3.3 `Turn Status API` |
| SSE events, payloads, ordering, close rules | 3.3 `SSE Event Definitions`, `SSE Event Ordering` |
| Provider event translation, REST error mapping, streaming error codes | 3.3 `Provider Event Translation`, `Error Codes`, `Streaming error codes` |
| Send and cancellation sequences | 3.6 `Send Message with Streaming Response`, `Streaming Cancellation`; trigger and enqueue in `Thread Summary Update` (`Trigger timing`, `Execution stages and invariants`, `Thread Summary - Stable Range and Commit Invariant`) |
| `messages`, `chat_turns`, `message_attachments`, `thread_summaries` columns and indexes | 3.7 `Table: messages`, `Table: chat_turns`, `Table: message_attachments`, `Table: thread_summaries` |
| Context plan order, truncation, determinism, snapshot boundary | 4 `Context Plan Assembly and Truncation`, `Context Plan Truncation Algorithm`, `ContextPlan Determinism and Snapshot Boundary (P1)` |
| Tool availability and web search | 4 `File Search Tool Availability`, `Code Interpreter Tool Availability`, `Web Search Configuration`, `Web Search Quota Enforcement (P1 Determinism Rules)`, `Code Interpreter Quota Enforcement (P1 Determinism Rules)`, `Citation File ID and Title Resolution`, `File Search Per-Turn Call Limit Enforcement (P1)`, `Knowledge Search`, `Model Catalog Configuration` |
| Provider request metadata and multimodal input | 4 `Provider Request Metadata`, `Internal: Multimodal Input Format (Responses API)` |
| Cancellation metrics, metric series, alerts | 4 `Cancellation Observability`, `Metrics (OTLP) and Alerts (P1)` |
| SSE infrastructure | 4 `SSE Infrastructure Requirements` |
| Turn state model, crash handling, orphan watchdog, idempotency, parallel turn policy, 409 recovery, non-goals | 4 `Turn Lifecycle, Crash Recovery and Orphan Handling` |
| Finalization contract, emission guard, forbidden patterns, persistence and durability invariants, accounting per outcome | 5.7 `Turn Finalization Contract (P1)` |
| Error code taxonomy, aborted streams, terminal error reconciliation, first-terminal-wins | 5.8, 5.9 |
| Configuration keys and defaults | Appendix B `B.1`, `B.4`, `B.5.1`, `B.5.5`, `B.6`, `B.6.1`, `B.7`, `B.8`, `B.9.1`, `B.9.4` |

## 2. Actor Flows (CDSL)

User-facing interactions that start with the chat user (or the admin for observability) and describe how the send, read and recovery paths behave end to end. Every flow runs behind the request admission flow `cpt-cf-mini-chat-flow-gear-foundation-request-admission` (authentication, license gate, PEP with the action of the authorization matrix, owner-scoped chat load and 404 masking).

**Use cases**: `cpt-cf-mini-chat-usecase-send-message` (all steps; its quota step and the alternative flow `Quota exceeded` run through the contracts of `cpt-cf-mini-chat-feature-quota-and-billing`), `cpt-cf-mini-chat-usecase-reconnect-turn-status`.

### Chat User Lists Messages

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-message-streaming-list-messages`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with `items` (messages in chronological order by default) and `page_info` (`limit`, `next_cursor`, `prev_cursor`); every item carries `id`, `request_id`, `role`, `content`, `attachments` (possibly empty), `my_reaction` (possibly `null`) and `created_at`, and assistant items carry `model` and non-zero token counts.
- A chat without messages returns an empty `items` array.

**Error Scenarios**:
- 401, 403 (`LICENSE_FEATURE_REQUIRED`, `AUTHZ_DENIED`) and 503 with `Retry-After: 5` from admission; 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.chat.v1~` for a missing, soft-deleted, foreign-user or foreign-tenant chat; 400 `invalid_path_params` for a non-UUID chat id.
- A malformed query: 400 `invalid_argument` with `context.resource_type` `gts.cf.core.odata.query.v1~` and the reason of the message list query algorithm.
- A stored message with a NULL `request_id`: 500 `internal` (implementation guard), never a message serialized without `request_id`.

**Steps**:
1. [x] - `p1` - Chat User sends `GET /v1/chats/{id}/messages` with optional `limit`, `cursor`, `$filter`, `$orderby` and `$select` - `inst-list-request`
2. [x] - `p1` - Run request admission with the action `list_messages` and the chat id as `resource.id`, which loads the chat through the owner-scoped query - `inst-list-admission`
3. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-list-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem (401, 403, 503) or 404 `not_found` for the chat type - `inst-list-return-refusal`
4. [x] - `p1` - Validate the query through the message list query algorithm - `inst-list-validate`
5. [x] - `p1` - **IF** validation failed - `inst-list-invalid`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `context.resource_type` `gts.cf.core.odata.query.v1~` and the reason - `inst-list-return-400`
6. [x] - `p1` - DB: SELECT one page of `messages` of the loaded chat (tenant predicate, `chat_id` from the scoped chat, `deleted_at IS NULL`) with the filter, order and keyset cursor of the validated query - `inst-list-select-page`
7. [x] - `p1` - Project each row through the message projection algorithm, which batch-loads the attachments and the caller's reactions for the page - `inst-list-project`
8. [x] - `p1` - **RETURN** 200 with `items` and `page_info` (`limit` equal to the effective page size, `next_cursor` and `prev_cursor` opaque or `null`) - `inst-list-return-200`

### Chat User Sends a Message and Receives a Streamed Response

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-message-streaming-send-message`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with `Content-Type: text/event-stream`; the stream starts with `stream_started` (`request_id`, pre-allocated `message_id`, `is_new_turn` `true`, optional `thread_summary_applied`), may carry `ping` events before the first content event, carries `delta` and `tool` events as the provider produces them, at most one `citations` event, and ends with exactly one `done` after the finalization committed the turn as `completed`.
- After `done` the user and assistant messages share the turn's `request_id`, the assistant message carries the effective model and the provider-reported token counts, the turn is `completed`, quota is settled once and one usage event and one audit event are enqueued.

**Error Scenarios**:
- Every pre-stream failure (validation, admission, idempotency conflict, parallel turn, invalid model, image count, quota, input too long, image guards, context budget, provider resolution, reserve re-check, attachment validation) is a JSON Problem; no SSE stream is opened, and on the send path no user message, no turn and no reserve remain.
- A failure after the stream opened ends the stream with one terminal `event: error` whose `code` is a streaming error code and whose `message` is sanitized; the turn is `failed` (or stays `running` for `finalization_failed`).
- A client disconnect follows the disconnect flow; nothing more is sent. A disconnect after the reserve committed but before the stream opened ends the turn `cancelled`: when the provider request was not sent yet, none is sent; when it started, it is hard-cancelled.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats/{id}/messages:stream` with a JSON body carrying `content` and optionally `request_id`, `attachment_ids` and `web_search` - `inst-send-request`
2. [x] - `p1` - Validate the body through the send request validation algorithm - `inst-send-validate`
3. [x] - `p1` - **IF** validation failed - `inst-send-invalid`
   1. [x] - `p1` - **RETURN** the Problem of the validation algorithm (400, 415 or 422) without opening a stream - `inst-send-return-invalid`
4. [x] - `p1` - Run request admission with the action `send_message` only (no `read` evaluation) and the chat id as `resource.id`; the decision covers the whole stream session - `inst-send-admission`
5. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-send-refused`
   1. [x] - `p1` - **RETURN** 401, 403, 503 with `Retry-After: 5`, or 404 for the chat type, without opening a stream - `inst-send-return-refusal`
6. [x] - `p1` - Create the turn's cancellation token before the setup starts and bind it to the HTTP response, so dropping the response or the connection at any later point (before the stream opens or while it is open) cancels the token - `inst-send-cancel-token`
7. [x] - `p1` - Run the remaining setup in a spawned task and await its result, so a client disconnect before the stream opens does not interrupt a setup that already committed a turn - `inst-send-spawn-setup`
8. [x] - `p1` - Resolve the turn's `request_id`: the client value when supplied (any UUID version), otherwise a server-generated UUID v4 - `inst-send-resolve-request-id`
9. [x] - `p1` - Run the turn admission checks algorithm (idempotency first, then the parallel-turn guard) - `inst-send-admission-checks`
10. [x] - `p1` - **IF** the checks found a completed, non-deleted turn for `(chat_id, request_id)` - `inst-send-is-replay`
    1. [x] - `p1` - **RETURN** the replay stream of the replay algorithm; no further check, no provider call, no quota change - `inst-send-return-replay`
11. [x] - `p1` - **IF** the checks rejected the request - `inst-send-conflict`
    1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `request_id_conflict` or `turn_already_running` as a JSON Problem - `inst-send-return-409`
12. [x] - `p1` - Run the send preflight algorithm (model resolution, snapshot boundary, image count, quota preflight, input limit, image guards, context assembly, tool assembly, provider resolution) - `inst-send-preflight`
13. [x] - `p1` - **IF** the preflight rejected the request - `inst-send-preflight-rejected`
    1. [x] - `p1` - **RETURN** its JSON Problem; nothing was written and no provider call was made - `inst-send-return-preflight`
14. [x] - `p1` - Run the reserve transaction algorithm (reserve write with re-check, user message, activity bump, attachment validation and links, `running` turn) - `inst-send-reserve-tx`
15. [x] - `p1` - **IF** the reserve transaction failed - `inst-send-reserve-failed`
    1. [x] - `p1` - **RETURN** its JSON Problem (429 `resource_exhausted` scope `tokens`, 400 `invalid_attachment`, 409, or 500); the transaction rolled back, so no user message, turn or reserve remains - `inst-send-return-reserve`
16. [x] - `p1` - **IF** the turn's cancellation token is already cancelled after the reserve commit (the client dropped the connection during setup) - `inst-send-dropped-before-provider`
    1. [x] - `p1` - Send no provider request and run the turn finalization algorithm with the outcome `cancelled`, no usage and empty accumulated text, so no assistant message is written, the settlement is `estimated` by the outcome rules of `cpt-cf-mini-chat-feature-quota-and-billing` and one `aborted` usage event and one `turn_failed` audit event are enqueued - `inst-send-dropped-finalize`
    2. [x] - `p1` - **RETURN** without writing any SSE event; the chat accepts a new `request_id` at once - `inst-send-return-dropped`
17. [x] - `p1` - Open the SSE response through the SSE relay algorithm and emit `stream_started` with `request_id`, the pre-allocated assistant `message_id`, `is_new_turn` `true` and `thread_summary_applied` when a thread summary is in the context - `inst-send-stream-started`
18. [x] - `p1` - Start the provider task: build the request through the adapter of the resolved provider and call OAGW through the provider call algorithm - `inst-send-provider-task`
19. [x] - `p1` - **IF** the response is dropped after the provider request started and before the SSE stream reached the client - `inst-send-dropped-before-stream`
    1. [x] - `p1` - The cancelled token makes the provider task hard-cancel the provider request through the provider call algorithm, and the disconnect flow finalizes the turn as `cancelled`; the turn never stays `running` - `inst-send-dropped-cancel-provider`
20. [x] - `p1` - Relay each translated provider event to the client as it arrives (event translation, tool accounting and SSE relay algorithms); never collect the response before relaying - `inst-send-relay`
21. [x] - `p1` - On the provider terminal, run the turn finalization algorithm (assistant message, CAS, settlement and usage event, audit event, thread-summary trigger) in one transaction - `inst-send-finalize`
22. [x] - `p1` - Emit the terminal event through the terminal emission algorithm: `citations` first when there are mapped citations, then `done` only after the commit of a `completed` turn, or `error` - `inst-send-terminal`
23. [x] - `p1` - **RETURN** the closed stream; the server closes the connection right after the terminal event - `inst-send-return-stream`

### Chat User Resends a Known Request Id

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-message-streaming-resend-request-id`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- Resending the `request_id` of a completed, non-deleted turn returns a replay stream: `stream_started` with `is_new_turn` `false` and the persisted assistant `message_id`, one `delta` with the full persisted text, then `done`; no provider request, no reserve, no settlement, no usage, audit or summary message.
- The replay is served even while another turn of the same chat is `running`, because replay is checked before the parallel-turn guard.

**Error Scenarios**:
- The key belongs to a `running`, `failed` or `cancelled` turn, or to a soft-deleted turn of any state: 409 `aborted` with `context.reason` `request_id_conflict` and a generic `detail`; no stream is opened.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats/{id}/messages:stream` again with the same `request_id` (for example after reading `state` `done` from the turn status API) - `inst-resend-request`
2. [x] - `p1` - Run validation and admission exactly as in the send flow - `inst-resend-admission`
3. [x] - `p1` - Run the turn admission checks algorithm - `inst-resend-checks`
4. [x] - `p1` - **IF** the turn is `completed` and not soft-deleted - `inst-resend-completed`
   1. [x] - `p1` - **RETURN** the replay stream of the replay algorithm, regardless of a different turn running in the chat - `inst-resend-return-replay`
5. [x] - `p1` - **ELSE** - `inst-resend-other-state`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `request_id_conflict`; the UI shows the 409 banner of `cpt-cf-mini-chat-fr-ux-recovery` and requires a new `request_id` to resend - `inst-resend-return-409`

### Chat User Disconnects During a Stream

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-message-streaming-disconnect`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A disconnect before the terminal event cancels the provider request (hard cancel of the outbound HTTP request through OAGW), finalizes the turn as `cancelled` with an estimated settlement and the aborted usage event, persists the accumulated text as a partial assistant message when it is non-empty, and sends nothing to the client.
- A disconnect after the terminal event changes nothing.
- A disconnect after the reserve transaction committed and before the stream opened ends the turn `cancelled` with an estimated settlement and the aborted usage event: no provider request is sent when the drop is observed before the call, and a started provider request is hard-cancelled; the turn never stays `running` until the orphan watchdog.

**Error Scenarios**:
- The provider terminal arrives first and wins the CAS: the turn keeps the provider outcome and the late cancellation is a no-op.
- The pod dies before finalizing: the turn stays `running` until the orphan watchdog finalizes it as `failed` with `orphan_timeout`.
- The partial assistant message cannot be persisted: the turn is still finalized as `cancelled` with `assistant_message_id` NULL and a warning is logged.

**Steps**:
1. [x] - `p1` - Chat User closes the connection or presses stop while the stream is open, or drops the connection after the reserve transaction committed and before the stream opened - `inst-dc-client-closes`
2. [x] - `p1` - **IF** the drop happened before the provider request was sent (the dropped response cancelled the token, which the send flow checks after the reserve commit) - `inst-dc-before-provider`
   1. [x] - `p1` - Send no provider request, run the turn finalization algorithm with the outcome `cancelled`, no usage and empty text (no assistant message, `estimated` settlement, `aborted` usage event), and **RETURN** without any SSE event - `inst-dc-return-before-provider`
3. [x] - `p1` - The dropped response (before the stream opened) or the dropped SSE relay cancels the turn's cancellation token; a provider task blocked on a full channel observes the failed send and treats it as the same disconnect - `inst-dc-cancel-token`
4. [x] - `p1` - The provider task aborts the outbound request (drops the OAGW response stream, so OAGW closes the provider connection) and exits its read loop - `inst-dc-hard-cancel`
5. [x] - `p1` - **IF** a terminal event was already emitted to the client - `inst-dc-after-terminal`
   1. [x] - `p1` - **RETURN** without any change: the committed outcome stands and no second terminal event exists - `inst-dc-return-noop`
6. [x] - `p1` - Run the turn finalization algorithm with the outcome `cancelled`, no usage and the accumulated text - `inst-dc-finalize`
7. [x] - `p1` - Send no SSE event on the broken stream, neither `error` nor anything else - `inst-dc-no-event`
8. [x] - `p1` - **RETURN**; the UI treats delivery as indeterminate, shows `Connection lost. Message delivery is uncertain. You can resend.` and resolves the outcome through the turn status API - `inst-dc-return`

### Chat User Checks Turn Status and Recovers

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-message-streaming-turn-status`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with `request_id`, `state` (`running`, `done`, `error` or `cancelled`), `updated_at`, `error_code` only for `error`, and `assistant_message_id` for `done` and for `cancelled` with a persisted partial message.
- `done` lets the UI replay the completed answer (resend the same `request_id`) or fetch it with `GET /v1/chats/{id}/messages?$filter=id eq '{assistant_message_id}'`; `error` and `cancelled` let the user resend with a new `request_id`; `running` keeps the UI waiting without auto-retry.

**Error Scenarios**:
- A soft-deleted turn (replaced by retry or edit, or deleted), an unknown `request_id`, or a turn in another user's chat: 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.turn.v1~` (or the chat type when the chat itself is not visible).
- A non-UUID `request_id` or chat id: 400 `invalid_path_params`.

**Steps**:
1. [x] - `p1` - Chat User (the UI after a disconnect or a 409) sends `GET /v1/chats/{id}/turns/{request_id}` - `inst-status-request`
2. [x] - `p1` - Run request admission with the action `read_turn` and the chat id as `resource.id` - `inst-status-admission`
3. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-status-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem or 404 for the chat type - `inst-status-return-refusal`
4. [x] - `p1` - DB: SELECT the `chat_turns` row with the scoped `chat_id`, the tenant predicate, the path `request_id` and `deleted_at IS NULL` - `inst-status-select-turn`
5. [x] - `p1` - **IF** no row was found - `inst-status-not-found`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.turn.v1~` - `inst-status-return-404`
6. [x] - `p1` - Project the row through the turn status projection algorithm - `inst-status-project`
7. [x] - `p1` - **RETURN** 200 with the projection; the read changes nothing - `inst-status-return-200`
8. [x] - `p1` - The UI acts on `state`: `done` renders the recovered answer with `Recovered a previously completed response.`; `running` shows that a response is in progress and does not resend; `error` or `cancelled` allows a resend with a new `request_id` - `inst-status-ui-action`

### Chat User Sends a Turn With Web Search

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-message-streaming-web-search-turn`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- With `web_search.enabled` `true` and an effective model whose `tool_support.web_search` is set, the provider request carries the `web_search` tool with the catalog `web_search_context_size` and the system prompt carries `context.web_search_guard`; each provider search is reported as `tool` `start` and `done` events named `web_search`, web citations arrive in `citations` with `source` `web`, `url`, `title`, `snippet` and `span` when the annotation has a range, and the completed call count is settled into the daily `web_search_calls` counter.
- With an effective model without web search support, the turn proceeds without the tool, the guard and the daily check, and the requested flag is still stored on the turn.

**Error Scenarios**:
- `disable_web_search` is on: 400 `failed_precondition` with `violations[{subject: web_search, type: FEATURE_DISABLED}]` before any stream (quota preflight contract).
- The daily web search quota is exhausted: 429 `resource_exhausted` with scope `web_search` before any stream (quota preflight contract).
- The model starts more `web_search` calls than `quota.web_search_max_calls_per_message` (default 2): the provider stream is cancelled, the turn is finalized `failed` with `web_search_calls_exceeded` and an estimated settlement, then SSE `error{code: "web_search_calls_exceeded"}` is sent.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats/{id}/messages:stream` with `web_search` equal to `{"enabled": true}` - `inst-web-request`
2. [x] - `p1` - Run the send flow; the quota preflight applies the kill switch and the daily quota only for a request that enables web search on a supporting effective model - `inst-web-send-flow`
3. [x] - `p1` - Store `chat_turns.web_search_enabled` equal to the requested flag in the reserve transaction, whether or not the tool is sent - `inst-web-store-flag`
4. [x] - `p1` - Include the tool and its guard through the tool assembly algorithm only when the effective model supports web search - `inst-web-assemble`
5. [x] - `p1` - Count `web_search` tool starts and enforce the per-message hard limit through the tool accounting algorithm - `inst-web-limit`
6. [x] - `p1` - Map web annotations to `citations` items through the citation mapping algorithm - `inst-web-citations`
7. [x] - `p1` - Pass the completed `web_search` call count to the finalization settlement, which adds it to the daily `total` bucket row on `actual` and `estimated` settlements - `inst-web-account`
8. [x] - `p1` - **RETURN** the stream ended by `done` or by the terminal `error` - `inst-web-return`

### Admin Observes Streaming Health

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-message-streaming-admin-observability`

**Actor**: `cpt-cf-mini-chat-actor-admin`

**Success Scenarios**:
- The admin reads the streaming, cancellation, orphan watchdog, finalization, image and thread-summary trigger series with the `metrics.prefix` prefix and low-cardinality labels and can distinguish provider failures, disconnects, cancellations and orphan finalizations.

**Error Scenarios**:
- An alert built on a declared-but-unrecorded series (`stream_replay`, `tool_calls`, `tool_call_limited`, `web_search_latency_ms`, `citations_count`, `provider_errors`, `provider_requests`, `tokens_after_cancel`, `time_from_ui_disconnect_to_cancel_ms`, `cancel_orphan`) cannot fire; this feature records none of them.

**Steps**:
1. [x] - `p1` - Admin opens the OTLP metrics of the gear - `inst-obs-open`
2. [x] - `p1` - Read the series recorded by the streaming metrics algorithm - `inst-obs-read`
3. [x] - `p1` - **IF** `mini_chat_ttft_overhead_ms` p99 exceeds 50 ms for 5 minutes, or `mini_chat_time_to_abort_ms` p99 exceeds 200 ms for 5 minutes - `inst-obs-latency-alert`
   1. [x] - `p1` - Raise the warning (overhead) or critical (abort) alert of DESIGN 4 `Alerting and SLO contract (P1)` - `inst-obs-raise-latency`
4. [x] - `p1` - **IF** `mini_chat_orphan_detected` rises over 15 minutes while `mini_chat_orphan_finalized` does not - `inst-obs-orphan-alert`
   1. [x] - `p1` - Raise the critical orphan watchdog alert - `inst-obs-raise-orphan`
5. [x] - `p1` - **RETURN** the aggregated view; no metric carries `tenant_id`, `user_id`, `chat_id`, `request_id`, `provider_response_id` or free-form error text as a label - `inst-obs-return`

## 3. Processes / Business Logic (CDSL)

Internal procedures that the flows call. Domain logic stays free of `crate::infra`, `sea_orm`, `axum` and `http` imports (domain ports implemented in infra; DTOs only under `api/rest/`). Repository functions take the caller's database runner so that a domain write and its outbox enqueue run in one transaction, and every outbox wake is fired only after that transaction commits.

### Send Request Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-request-validation`

**Input**: the raw request of `POST /v1/chats/{id}/messages:stream` (path, content type, body)

**Output**: the validated `content`, optional `request_id`, deduplicated-checked `attachment_ids` and the web search flag, or a Problem. The body schema is `StreamMessageRequest`; the reasons are DESIGN 3.3 `Error Codes`.

**Steps**:
1. [x] - `p1` - **IF** the chat id in the path is not a UUID - `inst-val-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` `invalid_path_params` - `inst-val-return-path`
2. [x] - `p1` - **IF** the body has no JSON `Content-Type` - `inst-val-content-type`
   1. [x] - `p1` - **RETURN** 415 `invalid_argument` with `field_violations[body].reason` `missing_json_content_type` - `inst-val-return-415`
3. [x] - `p1` - **IF** the body is not valid JSON - `inst-val-json-syntax`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[body].reason` `json_syntax_error` - `inst-val-return-syntax`
4. [x] - `p1` - **IF** the body does not match the schema (missing `content`, wrong types, a non-UUID `request_id` or `attachment_ids` entry, a `web_search` object without boolean `enabled`) - `inst-val-schema`
   1. [x] - `p1` - **RETURN** 422 `invalid_argument` with `field_violations[body].reason` `invalid_json_body` - `inst-val-return-422`
5. [x] - `p1` - **IF** `content` is empty or whitespace-only - `inst-val-empty-content`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[content].reason` `EMPTY_CONTENT` - `inst-val-return-empty`
6. [x] - `p1` - **IF** `attachment_ids` contains the same id twice, or more than `rag.max_documents_per_chat` plus `rag.max_images_per_message` ids (defaults 50 and 4) - `inst-val-attachment-shape`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[attachment].reason` `invalid_attachment` - `inst-val-return-attachment`
7. [x] - `p1` - Treat an absent or `null` `web_search` as `{"enabled": false}`, an absent or `null` `request_id` as "not supplied" and an absent `attachment_ids` as an empty list; ignore unknown body fields - `inst-val-defaults`
8. [x] - `p1` - Keep the content as sent: it is never trimmed, rewritten or truncated (only the emptiness check trims) - `inst-val-content-kept`
9. [x] - `p1` - **RETURN** the validated request; these checks run before the PEP call and before the chat is loaded, so they never depend on chat existence - `inst-val-return`

### Turn Admission Checks

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks`

**Input**: the scoped chat, the resolved `request_id` and whether the client supplied it

**Output**: `replay` with the completed turn, `proceed`, or a 409 rejection. The priority order is normative (DESIGN 4 `Parallel Turn Policy (P1)`, `Check Priority Order (Normative)`); `chat_turns` is the only source of running state (`messages.request_id` alone is not sufficient).

**Steps**:
1. [x] - `p1` - **IF** the client supplied `request_id` - `inst-adm-supplied`
   1. [x] - `p1` - DB: SELECT the `chat_turns` row with the scoped `chat_id` and `request_id`, including soft-deleted rows - `inst-adm-select-key`
   2. [x] - `p1` - **MATCH** the found row - `inst-adm-match`
      - [x] - `p1` - **CASE** state `completed` and `deleted_at IS NULL`: **RETURN** `replay` with the turn, and run no further check - `inst-adm-case-replay`
      - [x] - `p1` - **CASE** any soft-deleted row (replaced by retry or edit, or deleted), whatever its state: **RETURN** 409 `aborted` with `context.reason` `request_id_conflict` - `inst-adm-case-deleted`
      - [x] - `p1` - **CASE** state `running`: **RETURN** 409 `aborted` with `context.reason` `request_id_conflict` (an active generation for this key) - `inst-adm-case-running`
      - [x] - `p1` - **CASE** state `failed` or `cancelled`: **RETURN** 409 `aborted` with `context.reason` `request_id_conflict` (the client must use a new `request_id`) - `inst-adm-case-terminal`
      - [x] - `p1` - **DEFAULT** no row: continue with the parallel-turn guard - `inst-adm-case-none`
2. [x] - `p1` - DB: SELECT whether the scoped chat has any `chat_turns` row with `state` `running` and `deleted_at IS NULL`, whatever its `request_id` - `inst-adm-select-running`
3. [x] - `p1` - **IF** such a row exists - `inst-adm-running-found`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `turn_already_running` - `inst-adm-return-running`
4. [x] - `p1` - Use a generic `detail` on every 409 and log the internal cause (turn ids, driver text) only - `inst-adm-generic-detail`
5. [x] - `p1` - Never reverse the order: the parallel-turn guard never runs before the idempotency lookup, so a completed turn stays replayable while another turn of the chat is running - `inst-adm-order`
6. [x] - `p1` - **RETURN** `proceed`; a request that loses the insert race later is resolved by the reserve transaction algorithm - `inst-adm-return-proceed`

### Replay Stream

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-replay`

**Input**: the scoped chat and the completed, non-deleted turn

**Output**: a side-effect-free SSE stream of exactly three events. The replay `done` payload is rebuilt as recorded in ADR-0010 (the immutability invariant of DESIGN 4 `Idempotency Rules` is not implemented as written).

**Steps**:
1. [x] - `p1` - Run on a code path separate from turn execution, with no access to the quota reserve, settlement, outbox enqueue, audit enqueue or provider call - `inst-replay-separate-path`
2. [x] - `p1` - DB: SELECT the assistant `messages` row whose id is `chat_turns.assistant_message_id` (a `completed` turn always has one) - `inst-replay-load-message`
3. [x] - `p1` - **IF** the assistant message is missing or its `request_id` is NULL - `inst-replay-missing`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail and log the inconsistency; no stream is opened - `inst-replay-return-500`
4. [x] - `p1` - Open the SSE response with the headers of the SSE relay algorithm - `inst-replay-open`
5. [x] - `p1` - Emit `stream_started` with `request_id`, `message_id` equal to the persisted assistant message id and `is_new_turn` `false`; omit `thread_summary_applied` - `inst-replay-started`
6. [x] - `p1` - Emit one `delta` with `type` `text` and `content` equal to the full persisted assistant text (also when it is empty) - `inst-replay-delta`
7. [x] - `p1` - Emit `done` with `usage` from the persisted `input_tokens` and `output_tokens` of the assistant message, `effective_model` from `chat_turns.effective_model`, `selected_model` from `chats.model`, `quota_decision` `downgrade` with `downgrade_from` equal to `selected_model` when the two models differ and `allow` otherwise; omit `downgrade_reason` and `quota_warnings` - `inst-replay-done`
8. [x] - `p1` - Send no `ping`, no `tool` and no `citations` event (citations are not persisted) - `inst-replay-no-other-events`
9. [x] - `p1` - Take no reserve, change no `quota_usage` row, enqueue no usage, audit or summary message, write no row and make no provider request - `inst-replay-no-side-effects`
10. [x] - `p1` - **RETURN** the stream and close the connection after `done` - `inst-replay-return`

### Send Preflight

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-send-preflight`

**Input**: the security context, the scoped chat, the validated request and the resolved `request_id`

**Output**: a prepared turn (preflight decision, image list, context plan, tool set, resolved provider, pre-allocated assistant message id) or a JSON Problem. Every step runs before the reserve transaction, so a failure leaves no turn, no user message and no reserve (DESIGN 3.6 `Send Message with Streaming Response`).

**Steps**:
1. [x] - `p1` - Resolve the chat's model (`chats.model`) in the current catalog without the enabled filter through the policy snapshot access of `cpt-cf-mini-chat-feature-gear-foundation` - `inst-pre-resolve-model`
2. [x] - `p1` - **IF** the model is no longer in the catalog - `inst-pre-model-missing`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[model].reason` `INVALID_MODEL` - `inst-pre-return-invalid-model`
3. [x] - `p1` - **IF** the policy plugin cannot be resolved or fails - `inst-pre-policy-failed`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail - `inst-pre-return-policy-500`
4. [x] - `p1` - DB: compute the snapshot boundary `(max_included_created_at, max_included_id)` once from the latest non-deleted message of the chat in the order `(created_at, id)`; it is immutable for the request and later messages never enter this turn's context - `inst-pre-snapshot-boundary`
5. [x] - `p1` - DB: read the attachments referenced by `attachment_ids` within the scoped chat to classify their kind, and read the chat facts the quota estimate needs: whether the chat has a ready, non-deleted document attachment and a ready, non-deleted attachment with `for_code_interpreter` - `inst-pre-read-attachments`
6. [x] - `p1` - **IF** the number of referenced image attachments exceeds `rag.max_images_per_message` (default 4) - `inst-pre-too-many-images`
   1. [x] - `p1` - **RETURN** 400 `out_of_range` with `field_violations[image_count].reason` `TOO_MANY_IMAGES` - `inst-pre-return-too-many-images`
7. [x] - `p1` - Call `QuotaService::preflight` of `cpt-cf-mini-chat-feature-quota-and-billing` with the chat id, the selected model and the request facts (UTF-8 byte length of `content`, image count, ready documents, ready code-interpreter attachments, `web_search.enabled`), passing the turn's requester as the identity - `inst-pre-quota-preflight`
8. [x] - `p1` - **IF** the preflight rejected - `inst-pre-quota-rejected`
   1. [x] - `p1` - **RETURN** the Problem of the quota rejection mapping unchanged: 400 `failed_precondition` `FEATURE_DISABLED` for web search, 429 `resource_exhausted` with scope `tokens`, `web_search` or `code_interpreter`, or 500 `internal` - `inst-pre-return-quota`
9. [x] - `p1` - **IF** the decision reports the input as too long (`max_input_tokens` of the effective model is not 0 and the message's `estimated_text_tokens` exceeds it) - `inst-pre-input-too-long`
   1. [x] - `p1` - **RETURN** 400 `out_of_range` with `field_violations[content].reason` `INPUT_TOO_LONG` - `inst-pre-return-input-too-long`
10. [x] - `p1` - Run the image guards algorithm with the effective model of the decision and **RETURN** its Problem when it rejects - `inst-pre-image-guards`
11. [x] - `p1` - Run the tool assembly algorithm for the effective model - `inst-pre-tools`
12. [x] - `p1` - Run the context plan assembly algorithm with the snapshot boundary, the decision and the tool set, and **RETURN** 400 `out_of_range` with `field_violations[context].reason` `CONTEXT_BUDGET_EXCEEDED` when the mandatory items do not fit - `inst-pre-context`
13. [x] - `p1` - Resolve the provider of the effective model through the provider registry resolver of `cpt-cf-mini-chat-feature-gear-foundation`: the catalog `provider_id` and the caller's tenant (through `tenant_overrides`) give the provider entry, its adapter `kind` and its OAGW alias - `inst-pre-resolve-provider`
14. [x] - `p1` - **IF** the provider cannot be resolved - `inst-pre-provider-failed`
    1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail - `inst-pre-return-provider-500`
15. [x] - `p1` - Pre-allocate the assistant message id as a new UUID v4; no assistant row exists until finalization - `inst-pre-message-id`
16. [x] - `p1` - **RETURN** the prepared turn; no row was written and no provider request was sent - `inst-pre-return`

### Image Guards

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-image-guards`

**Input**: the image attachments referenced by the request, the kill switches of the policy snapshot, and the `VISION_INPUT` capability of the effective model carried by the preflight decision

**Output**: pass or a Problem. The rules are DESIGN 2.2 `Model Capability Constraint (Images)`, `Downgrade Decision Matrix` and `Quota Before Outbound` (`disable_images`); retry and edit reuse this algorithm for the original message's images.

**Steps**:
1. [x] - `p1` - **IF** the request carries no image attachment - `inst-img-no-images`
   1. [x] - `p1` - **RETURN** pass; a model without `VISION_INPUT` is never rejected for a text-only request - `inst-img-return-pass-text`
2. [x] - `p1` - **IF** the `disable_images` kill switch is on - `inst-img-kill-switch`
   1. [x] - `p1` - **RETURN** 400 `failed_precondition` with `violations[{subject: images, type: FEATURE_DISABLED}]` before any stream opens - `inst-img-return-disabled`
3. [x] - `p1` - **IF** the effective model (after the downgrade cascade) lacks `VISION_INPUT` in `multimodal_capabilities`, also when the selected model has it - `inst-img-no-vision`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[content_type].reason` `VISION_NOT_SUPPORTED` - `inst-img-return-vision`
4. [x] - `p1` - Never drop or strip images from the request and never switch to another model to satisfy it - `inst-img-no-silent-drop`
5. [x] - `p1` - **RETURN** pass - `inst-img-return-pass`

### Context Budget

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-context-budget`

**Input**: the catalog entry of the effective model (`context_window`, `max_input_tokens`, `estimation_budgets`), `max_output_tokens_applied` of the decision and the tool set

**Output**: `token_budget`, `effective_budget` for the thread-summary threshold, or a budget rejection. The formula is DESIGN 2.2 `Context Window Budget` and 4 `Context Plan Truncation Algorithm`; it is computed after the downgrade cascade, from the effective model.

**Steps**:
1. [x] - `p1` - **IF** `max_output_tokens_applied` is at least `context_window` - `inst-budget-no-room`
   1. [x] - `p1` - **RETURN** a budget rejection (`CONTEXT_BUDGET_EXCEEDED`) - `inst-budget-return-no-room`
2. [x] - `p1` - Set `input_limit` to `context_window` minus `max_output_tokens_applied`, lowered to `max_input_tokens` when `max_input_tokens` is not 0 and smaller - `inst-budget-input-limit`
3. [x] - `p1` - Set the deductions to `fixed_overhead_tokens` plus each surcharge of a tool in the tool set: `tool_surcharge_tokens` for `file_search`, `web_search_surcharge_tokens` for `web_search`, `code_interpreter_surcharge_tokens` for `code_interpreter`; `search_knowledge` has no surcharge - `inst-budget-deductions`
4. [x] - `p1` - **IF** the deductions reach `input_limit` - `inst-budget-deductions-exceed`
   1. [x] - `p1` - **RETURN** a budget rejection (`CONTEXT_BUDGET_EXCEEDED`) - `inst-budget-return-deductions`
5. [x] - `p1` - Set `token_budget` to `input_limit` minus the deductions, and `effective_budget` to `input_limit` (no surcharge and no fixed overhead subtracted) - `inst-budget-values`
6. [x] - `p1` - **RETURN** `token_budget` and `effective_budget` - `inst-budget-return`

### Context Plan Assembly and Truncation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-context-assembly`

**Input**: the scoped chat, the snapshot boundary, the effective model entry, the tool set, the current `content` and image attachments, `context.recent_messages_limit` (default 10, range 0 to 100)

**Output**: the `ContextPlan` (system instructions, optional thread summary item, recent messages, current user message with images), `messages_truncated`, `assembled_context_tokens`, the applied summary's `token_estimate`, or a budget rejection. The order and rules are DESIGN 4 `Context Plan Assembly and Truncation`, `Context Plan Truncation Algorithm` and `ContextPlan Determinism and Snapshot Boundary (P1)`.

**Steps**:
1. [x] - `p1` - Build the system instructions: the effective model's catalog `system_prompt`, followed by the guards of the tool set in the order `context.file_search_guard`, `context.web_search_guard`, `knowledge_search.guard`, each appended only when its tool is in the tool set; the system prompt is configuration and is never persisted as a `messages` row - `inst-ctx-system`
2. [x] - `p1` - DB: SELECT the chat's `thread_summaries` row (summary text, frontier, `token_estimate`) - `inst-ctx-load-summary`
3. [x] - `p1` - **IF** a summary exists, build the summary item as one `user`-role message whose text is the preamble of DESIGN B.5.5 followed by the summary text; the preamble counts toward the item's estimate - `inst-ctx-summary-item`
4. [x] - `p1` - DB: SELECT at most `context.recent_messages_limit` messages of the chat with `request_id IS NOT NULL`, `deleted_at IS NULL`, `is_compressed = false`, `(created_at, id)` at most the snapshot boundary and, only when a summary exists, `(created_at, id)` greater than the summary frontier, ordered by `(created_at DESC, id DESC)`, then reverse them to chronological order - `inst-ctx-recent`
5. [x] - `p1` - Compare timestamps for the boundary, the frontier and the order with the fixed-width sort key on SQLite (never raw text comparison of variable-length RFC 3339 values) and with the native type on PostgreSQL, always with `id` as the tie-break - `inst-ctx-sort-key`
6. [x] - `p1` - Build the current user item from `content` and, for each referenced image attachment in `attachment_ids` order, one image input referencing the attachment's internal `provider_file_id` (never exposed to clients); images of earlier turns are never reused implicitly - `inst-ctx-user-item`
7. [x] - `p1` - Estimate every item with the effective model's `estimation_budgets` using the text estimate of `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation` (byte length divided by `bytes_per_token_conservative` rounded up, plus `fixed_overhead_tokens`, plus `safety_margin_pct`), plus `image_token_budget` per current image; no provider tokenizer is used - `inst-ctx-estimate`
8. [x] - `p1` - Compute the budgets through the context budget algorithm and **RETURN** its rejection when it rejects - `inst-ctx-budget`
9. [x] - `p1` - **IF** the system instructions plus the current user item and its images exceed `token_budget` - `inst-ctx-mandatory-overflow`
   1. [x] - `p1` - **RETURN** a budget rejection (`CONTEXT_BUDGET_EXCEEDED`); the system prompt, the guards, the user message and the current images are never truncated - `inst-ctx-return-mandatory`
10. [x] - `p1` - Keep the summary item when it fits in the remaining budget after the mandatory items, otherwise drop it entirely - `inst-ctx-summary-fit`
11. [x] - `p1` - **FOR EACH** recent message from newest to oldest - `inst-ctx-walk-recent`
    1. [x] - `p1` - Keep the message while it fits in the remaining budget; at the first message that does not fit, drop it and every older message - `inst-ctx-keep-or-drop`
12. [x] - `p1` - **WHILE** the kept range starts with an `assistant` message - `inst-ctx-whole-turns`
    1. [x] - `p1` - Drop that message too, so an answer is never sent without its question and truncation removes whole turns - `inst-ctx-drop-leading-assistant`
13. [x] - `p1` - Set `messages_truncated` to true when at least one non-system recent message was dropped - `inst-ctx-truncated-flag`
14. [x] - `p1` - Assemble in the normative order: system instructions, thread summary item, recent messages, current user item with its images; retrieval excerpts are not assembled (provider-side `file_search`) and there is no document-summary tier - `inst-ctx-order`
15. [x] - `p1` - Set `assembled_context_tokens` to the sum of the estimates of the kept items (system instructions, summary item, kept history, current user item with images) - `inst-ctx-assembled-tokens`
16. [x] - `p1` - Guarantee determinism: the same boundary, history, summary, catalog entry and tool set always yield the same plan, the same kept range and the same `messages_truncated` - `inst-ctx-deterministic`
17. [x] - `p1` - **RETURN** the plan, `messages_truncated`, `assembled_context_tokens`, `effective_budget` and the kept summary's `token_estimate` (absent when no summary is kept) - `inst-ctx-return`

### Tool Assembly

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-tool-assembly`

**Input**: the effective model entry (`general_config.tool_support`, `web_search_context_size`, `max_num_results`, `max_tool_calls`), the kill switches, the request flag `web_search.enabled`, and the tool contributions registered by other features

**Output**: the adapter-agnostic tool set (tool definitions, guards, `include` entries, the `feature` metadata value). The gate is the catalog `tool_support`, not the adapter (DESIGN 3.2 `llm_provider`, ADR-0005).

**Steps**:
1. [x] - `p1` - Start with an empty tool set - `inst-tools-empty`
2. [x] - `p1` - **IF** `web_search.enabled` is true and the effective model's `tool_support.web_search` is set - `inst-tools-web`
   1. [x] - `p1` - Add the `web_search` tool with the catalog `web_search_context_size` as its search context size and the guard `context.web_search_guard` - `inst-tools-web-add`
3. [x] - `p1` - **ELSE IF** `web_search.enabled` is true - `inst-tools-web-unsupported`
   1. [x] - `p1` - Add no tool and no guard; the turn proceeds without web search and the daily web search check was skipped by the quota preflight - `inst-tools-web-skip`
4. [x] - `p1` - **FOR EACH** tool contribution registered by another feature (`file_search` and `code_interpreter` and `search_knowledge` from `cpt-cf-mini-chat-feature-attachments-and-file-search`) - `inst-tools-contributions`
   1. [x] - `p1` - Ask the contribution whether its tool applies to this turn under its own inclusion rules (DESIGN 4 `File Search Tool Availability`, `Code Interpreter Tool Availability`, `Knowledge Search`) and add the returned definition, guard and `include` entry when it applies; `file_search` and `search_knowledge` are never both added - `inst-tools-contribution-add`
5. [x] - `p1` - Order the tool definitions `file_search`, `web_search`, `code_interpreter`, `search_knowledge` and keep the guards in the order of the context plan assembly - `inst-tools-order`
6. [x] - `p1` - Set the `feature` metadata value to `none` when no built-in tool is present, otherwise to the present built-in tool names joined with `+` in the order `file_search`, `web_search`, `code_interpreter` (for example `file_search+web_search`) - `inst-tools-feature`
7. [x] - `p1` - Decide the set before the adapter runs, without knowing the adapter kind; an adapter that drops a tool still gets the guard in the system instructions, and the surcharge and daily quota decided at preflight stay as they are - `inst-tools-adapter-agnostic`
8. [x] - `p1` - Bound built-in tool calls per provider request only through the catalog `max_tool_calls` (default 2, shared by all built-in tools), sent by the OpenAI Responses adapter; there is no separate per-turn `file_search` limit - `inst-tools-max-calls`
9. [x] - `p1` - **RETURN** the tool set - `inst-tools-return`

### Reserve Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-reserve-transaction`

**Input**: the prepared turn, the security context, the scoped chat, the validated request and the resolved `request_id`

**Output**: the committed `running` turn with its user message and reserve, or a Problem with everything rolled back. This realizes DESIGN 3.6 (single reserve transaction), 5.7 `User message persistence invariant (preflight prerequisite)` and `Attachment Preflight Validation Invariant (P1)`; the reserve part is `cpt-cf-mini-chat-algo-quota-and-billing-reserve-write` with `ReserveTurn::New`.

**Steps**:
1. [x] - `p1` - Open one database transaction - `inst-rtx-begin`
2. [x] - `p1` - Call `QuotaService::write_reserve` with the decision and the turn identity (`tenant_id`, `user_id` of the requester, `chat_id`, `request_id`) inside the transaction - `inst-rtx-reserve`
3. [x] - `p1` - **IF** the reserve re-check reported a bucket over its limit - `inst-rtx-over-limit`
   1. [x] - `p1` - Roll back and **RETURN** 429 `resource_exhausted` with scope `tokens`; no downgrade is attempted - `inst-rtx-return-429`
4. [x] - `p1` - DB: INSERT the user `messages` row: `role` `user`, the turn's `request_id`, `content`, `content_type` `text`, `token_estimate` 0, `request_kind` `chat`, `features_used` `[]`, token columns 0, `model` NULL, `is_compressed` false, `created_at` from the application clock - `inst-rtx-user-message`
5. [x] - `p1` - DB: UPDATE `chats.updated_at` to the current time in the same transaction (the message-activity contract of `cpt-cf-mini-chat-algo-chat-management-activity-ordering`) - `inst-rtx-activity`
6. [x] - `p1` - Run the attachment validation algorithm for `attachment_ids` and, when it rejects, roll back and **RETURN** 400 `invalid_argument` with `field_violations[attachment].reason` `invalid_attachment` - `inst-rtx-attachments`
7. [x] - `p1` - DB: INSERT one `message_attachments` row per referenced attachment with `tenant_id`, `chat_id` taken from the user message row (never from input), `message_id`, `attachment_id` and `created_at` - `inst-rtx-links`
8. [x] - `p1` - DB: INSERT the `chat_turns` row: `state` `running`, `requester_type` `user`, `requester_user_id` the caller, the turn's `request_id`, `started_at` equal to the decision's preflight instant, `last_progress_at` equal to the insert instant, `web_search_enabled` equal to the requested flag, the preflight columns of the decision (`reserve_tokens`, `max_output_tokens_applied`, `reserved_credits_micro`, `policy_version_applied`, `effective_model`, `minimal_generation_floor_applied`), `provider_name` NULL, counters 0, `completed_at` NULL - `inst-rtx-turn`
9. [x] - `p1` - **IF** an insert violated `UNIQUE(chat_id, request_id)` on `chat_turns` or the `(chat_id, request_id, role)` index on `messages` - `inst-rtx-key-race`
   1. [x] - `p1` - Roll back, re-read the key, and **RETURN** 409 `aborted` with `context.reason` `request_id_conflict` when a turn with that `request_id` exists, otherwise `turn_already_running` - `inst-rtx-return-key-race`
10. [x] - `p1` - **IF** the turn insert violated `UNIQUE(chat_id) WHERE state = 'running' AND deleted_at IS NULL` - `inst-rtx-running-race`
    1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `turn_already_running` - `inst-rtx-return-running-race`
11. [x] - `p1` - **IF** any other database error occurred - `inst-rtx-db-error`
    1. [x] - `p1` - Roll back and **RETURN** 500 `internal` with a generic detail - `inst-rtx-return-500`
12. [x] - `p1` - Commit the transaction, then record the reserve metrics of the reserve receipt - `inst-rtx-commit`
13. [x] - `p1` - Allow the provider request only after this commit, so a `running` turn with its user message and reserve exists before the first outbound byte; a crash after the commit and before the request is settled by the orphan watchdog - `inst-rtx-before-provider`
14. [x] - `p1` - **RETURN** the committed turn (turn id, `started_at`, decision, assistant message id) - `inst-rtx-return`

### Attachment Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-attachment-validation`

**Input**: the `attachment_ids` of the request (already unique and within the count cap), the security context, the scoped chat and the open reserve transaction

**Output**: the validated attachments or a rejection. The rules are DESIGN 3.3 `Streaming Contract` (validation list) and `Attachment Preflight Validation Invariant (P1)`; no check relies on a provider-side failure.

**Steps**:
1. [x] - `p1` - DB: SELECT the `attachments` rows with the referenced ids inside the reserve transaction - `inst-att-select`
2. [x] - `p1` - **FOR EACH** referenced id - `inst-att-each`
   1. [x] - `p1` - Reject when no row exists, when `attachments.tenant_id` differs from the security context tenant, when `uploaded_by_user_id` differs from the caller, when `attachments.chat_id` differs from the requested chat (the chat's owner is the caller by the scoped load), when `deleted_at` is set, or when `status` is not `ready` (an `uploaded` document still indexing is not ready) - `inst-att-check`
3. [x] - `p1` - **IF** any id was rejected - `inst-att-rejected`
   1. [x] - `p1` - **RETURN** a rejection that the caller maps to 400 `invalid_argument` `invalid_attachment`, so the reserve and the user message written earlier in the transaction are rolled back - `inst-att-return-rejected`
4. [x] - `p1` - Record the association for UI, audit and history only: in P1 `attachment_ids` neither scopes nor filters retrieval, and only images among them become model input for this turn - `inst-att-association-only`
5. [x] - `p1` - **RETURN** the validated attachments - `inst-att-return`

### Provider Call Through OAGW

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-provider-call`

**Input**: the resolved provider entry (adapter `kind`, `upstream_alias`, `api_path`), the provider request body built by the adapter, the gear's S2S security context and the turn's cancellation token

**Output**: a stream of provider bytes consumed by the adapter's parser, or a transport error. The transport is ADR-0002 (HTTP with SSE passthrough); credentials are injected by OAGW (`cpt-cf-mini-chat-constraint-no-credentials`).

**Steps**:
1. [x] - `p1` - **IF** the cancellation token is already cancelled before the request is sent - `inst-call-precheck-cancelled`
   1. [x] - `p1` - Send nothing to OAGW and **RETURN** `cancelled` to the provider task - `inst-call-return-precheck`
2. [x] - `p1` - Build the request URI as `/{upstream_alias}{api_path}` with `{model}` in `api_path` replaced by the effective model's `provider_model_id`; `upstream_alias` is the alias held by the provider registry (the tenant override's alias when the caller's tenant has one) and may be `host:port` - `inst-call-uri`
3. [x] - `p1` - Send `POST` with the JSON body through the in-process `ServiceGatewayClientV1` `proxy_request`, under the gear's S2S security context obtained at gear start (the upstreams and routes are owned by the S2S tenant), while the user and tenant identity travel only in the request's `user` and `metadata` fields - `inst-call-proxy`
4. [x] - `p1` - Never call a provider directly, never add credentials and never rely on OAGW for quota decisions or retries (OAGW does not retry, including provider 429) - `inst-call-oagw-only`
5. [x] - `p1` - Read the response body incrementally and hand each SSE event to the adapter parser as soon as it is complete; never read the whole body first - `inst-call-incremental`
6. [x] - `p1` - **IF** the cancellation token fires - `inst-call-cancelled`
   1. [x] - `p1` - Drop the response stream at once (hard cancel), which closes the OAGW request and the provider connection, and report `cancelled` to the provider task - `inst-call-hard-cancel`
7. [x] - `p1` - **IF** the response status is not successful, or the transport fails or times out - `inst-call-http-error`
   1. [x] - `p1` - Report the status, the `Retry-After` header and the error body to the provider error mapping algorithm - `inst-call-report-error`
8. [x] - `p1` - Record `mini_chat_ttft_provider_ms` from sending the request to the first response byte - `inst-call-ttft-provider`
9. [x] - `p1` - **RETURN** the event stream - `inst-call-return`

### OpenAI Responses Request Construction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-responses-request`

**Input**: the context plan, the tool set, the decision (`max_output_tokens_applied`), the effective model entry, the security context and the chat id

**Output**: the JSON body of an OpenAI or Azure OpenAI Responses API streaming request (adapter kind `openai_responses`, providers with `storage_kind` `openai` or `azure`). The adapter is implemented fully; field sources are DESIGN 3.2 `llm_provider`, 4 `Provider Request Metadata`, `Internal: Multimodal Input Format (Responses API)`, `Model Catalog Configuration` and the tool sections of 4.

**Steps**:
1. [x] - `p1` - Set `model` to the effective model's `provider_model_id` - `inst-req-model`
2. [x] - `p1` - Set `instructions` to the system instructions of the context plan (system prompt followed by the tool guards) - `inst-req-instructions`
3. [x] - `p1` - Set `input` to an array of message items in the plan order (DESIGN 4 `Internal: Multimodal Input Format (Responses API)`, `Context Plan Assembly and Truncation`): first, when kept, the thread summary item with `role` `user` and `content` a plain string holding the B.5.5 preamble followed by the summary text; then each kept history message in chronological order with `role` `user` for a user message and `assistant` for an assistant message and `content` a plain string holding the stored text (history items are text only, because images of earlier turns are never reused); last, the current user message with `role` `user` whose `content` is the plain message string when the turn has no image, otherwise an array of one item with `type` `input_text` and `text` equal to the message text followed by one item per current image, in `attachment_ids` order, with `type` `input_image` and `file_id` equal to the internal `provider_file_id`; the system instructions are never an `input` item - `inst-req-input`
4. [x] - `p1` - Set `stream` to `true`, `max_output_tokens` to `max_output_tokens_applied` and `max_tool_calls` to the catalog `max_tool_calls` (default 2) - `inst-req-stream-limits`
5. [x] - `p1` - Set `tools` to the tool set in Responses format, in the order of the tool set: for file search an entry with `type` `file_search`, `vector_store_ids` and `max_num_results`; for web search an entry with `type` exactly `web_search`, the tool whose calls the provider reports as `response.web_search_call.*` events, and `search_context_size` equal to the catalog `web_search_context_size`, one of `low`, `medium` or `high`; for the code interpreter an entry with `type` `code_interpreter` and a `container` whose `type` is `auto` and whose `file_ids` list the attachment files; and `search_knowledge` as a function tool; omit `tools` when the set is empty - `inst-req-tools`
6. [x] - `p1` - Set `include` to `["code_interpreter_call.outputs"]` whenever the `code_interpreter` tool is in the request, and omit it otherwise - `inst-req-include`
7. [x] - `p1` - Set `user` to the tenant id and user id of the security context in 32-character lowercase hexadecimal form without hyphens, tenant first (exactly 64 characters); fall back to `{tenant_id}:{user_id}` only when an id is not a UUID - `inst-req-user`
8. [x] - `p1` - Set `metadata` to `tenant_id`, `user_id`, `chat_id`, `request_type` `chat` and `feature` from the tool set; these fields serve observability only - `inst-req-metadata`
9. [x] - `p1` - Set `store` to `false` and send no `previous_response_id`: every request carries the full assembled context and no provider-side conversation state is used - `inst-req-stateless`
10. [x] - `p1` - Add the catalog `general_config.api_params` that are set: `temperature`, `top_p`, `frequency_penalty`, `presence_penalty` (each only when set), `stop`, and `reasoning_effort` as the Responses reasoning effort; merge `extra_body` keys into the top level, except the request-controlled keys `model`, `input`, `messages`, `instructions`, `system`, `stream`, `stream_options`, `max_output_tokens`, `max_completion_tokens`, `max_tokens`, `max_tool_calls`, `tools`, `tool_choice`, `include`, `store`, `previous_response_id`, `user`, `metadata`, which are ignored with a warning - `inst-req-api-params`
11. [x] - `p1` - Never place a provider credential in the body, and never log the body content - `inst-req-no-secrets`
12. [x] - `p1` - **RETURN** the request body - `inst-req-return`

### Provider Adapter Kinds

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-adapter-kinds`

**Input**: the provider entry `kind`, the context plan, the tool set and the decision

**Output**: the request body and the wire parser of the selected adapter. All four adapters are in scope (ADR-0005); each translates its wire protocol into the same internal events (text delta, reasoning delta, tool start, tool done, annotations, tool-use outcome, completed with usage, incomplete with reason and usage, failed with error and optional usage). Each adapter also provides the non-streaming completion call that the thread-summary worker of `cpt-cf-mini-chat-feature-background-maintenance` uses with `request_type` `summary`.

**Steps**:
1. [x] - `p1` - **MATCH** the provider `kind` - `inst-kind-match`
   - [x] - `p1` - **CASE** `openai_responses`: build the body with the OpenAI Responses request construction algorithm and parse the Responses SSE events through the provider event translation algorithm; the only adapter that sends `metadata`, `max_tool_calls`, built-in tools and `include` - `inst-kind-openai-responses`
   - [x] - `p1` - **CASE** `openai_chat_completions`: send `model`, `messages` (a `system` message with the system instructions, then the plan items, the current user message with text and image parts), `stream` `true`, `stream_options` requesting the usage chunk, `max_completion_tokens` equal to `max_output_tokens_applied`, `user`, function tools only (`file_search`, `web_search` and `code_interpreter` dropped) and the merged `extra_body`; translate content deltas to text deltas, tool-call deltas to `tool` events named `function_call` (`start` with `index`, `call_id`, `name` in `details`; `done` with `call_id`, `name`, `arguments`), a `length` finish reason to an incomplete completion with reason `max_tokens`, the final usage chunk (`prompt_tokens`, `completion_tokens`) to `input_tokens` and `output_tokens` - `inst-kind-chat-completions`
   - [x] - `p1` - **CASE** `vllm_responses`: send the Responses body without any tool (function tools included), without `metadata`, without `max_tool_calls` and without `include`, still with `user` and the guards in `instructions`; translate like the Responses adapter and emit text inside `<think>` blocks as `delta` with `type` `reasoning` - `inst-kind-vllm-responses`
   - [x] - `p1` - **CASE** `anthropic_messages`: send `model`, `system` with the system instructions, `messages`, `max_tokens` equal to `max_output_tokens_applied`, `stream` `true`, `metadata.user_id` equal to the composite `user` value, `web_search` and `code_interpreter` mapped to the Anthropic server tools, function tools kept, `file_search` dropped, images referenced by the attachment's secondary Anthropic file id, no `extra_body`; translate text deltas to text deltas, the `code_execution` server tool `content_block_start` and `content_block_stop` to `tool` `start` and `done` named `code_interpreter` with `details` `{}`, the web search server tool blocks to `tool` `start` and `done` named `web_search` with `details` `{}`, a function tool use to one `tool` `start` named `search_knowledge`, `load_files` or `unknown_tool` (by tool name) with no `done`, a `max_tokens` stop to an incomplete completion with reason `max_tokens`, and the usage counts (including cache reads and cache writes) to the internal usage; produce no citations - `inst-kind-anthropic`
   - [x] - `p1` - **DEFAULT**: unreachable, because configuration validation of `cpt-cf-mini-chat-feature-gear-foundation` rejects any other `kind` at startup - `inst-kind-default`
2. [x] - `p1` - Send the composite `user` value on every adapter (as `metadata.user_id` on Anthropic) and apply the same provider error mapping on every adapter - `inst-kind-common`
3. [x] - `p1` - **RETURN** the body and the parser - `inst-kind-return`

### Provider Event Translation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-event-translation`

**Input**: one provider SSE event of a Responses adapter (`openai_responses`, `vllm_responses`)

**Output**: zero or one internal event for the stable SSE contract. The table is DESIGN 3.3 `Provider Event Translation`; provider events are never forwarded to clients.

**Steps**:
1. [x] - `p1` - Take the event name from the SSE `event:` line; when the line is missing or is `message`, take it from the `type` field of the `data` JSON - `inst-tr-event-name`
2. [x] - `p1` - **MATCH** the event name - `inst-tr-match`
   - [x] - `p1` - **CASE** `response.output_text.delta`: text delta mapped one to one - `inst-tr-text-delta`
   - [x] - `p1` - **CASE** `response.file_search_call.searching`: `tool` `start` named `file_search` with `details` `{}` - `inst-tr-fs-start`
   - [x] - `p1` - **CASE** `response.file_search_call.completed`: `tool` `done` named `file_search` with `details` `{files_searched}` equal to the length of the event's `results` (0 when absent, which is always the case with the real API) - `inst-tr-fs-done`
   - [x] - `p1` - **CASE** `response.web_search_call.searching`: `tool` `start` named `web_search` with `details` `{}` - `inst-tr-ws-start`
   - [x] - `p1` - **CASE** `response.web_search_call.completed`: `tool` `done` named `web_search` with `details` `{}` - `inst-tr-ws-done`
   - [x] - `p1` - **CASE** `response.code_interpreter_call.in_progress`: `tool` `start` named `code_interpreter` with `details` `{}` - `inst-tr-ci-start`
   - [x] - `p1` - **CASE** `response.output_item.done` with an item of type `code_interpreter_call`: `tool` `done` named `code_interpreter` with `details` `{output}`, the item's `logs` outputs joined with a newline and capped at 8192 characters with the suffix `...[truncated]`; other `output_item.done` items are ignored, except a `function_call` item, which is a tool-use outcome - `inst-tr-ci-done`
   - [x] - `p1` - **CASE** `response.code_interpreter_call.interpreting` or `response.code_interpreter_call.completed`: ignored, no client event - `inst-tr-ci-ignored`
   - [x] - `p1` - **CASE** `response.completed`: completion with `usage` from `response.usage` (`input_tokens`, `output_tokens`, cached input tokens as `cache_read_input_tokens`, reasoning tokens as `reasoning_tokens`), the provider `response.id` kept internally, and the annotations of the `output_text` parts kept for citation mapping - `inst-tr-completed`
   - [x] - `p1` - **CASE** `response.incomplete`: completion that is truncated but valid, with `usage` from `response.usage`, the incomplete reason kept for logging and the metric label only, and a provider response id when present - `inst-tr-incomplete`
   - [x] - `p1` - **CASE** `response.failed`: failure with the error read from `response.error` (top-level `error` as fallback) and `response.usage` kept when present - `inst-tr-failed`
   - [x] - `p1` - **CASE** `error`: failure parsed like `response.failed`, then as a flat `{code, message}`, and unparseable data used as the message - `inst-tr-error-event`
   - [x] - `p1` - **DEFAULT**: ignored - `inst-tr-default`
3. [x] - `p1` - Treat a stream that ends without `response.completed`, `response.incomplete`, `response.failed` or `error` as a provider stream failure (`provider_error`) unless the cancellation token fired - `inst-tr-premature-end`
4. [x] - `p1` - **RETURN** the internal event - `inst-tr-return`

### SSE Relay

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-sse-relay`

**Input**: the bounded channel from the provider task, `streaming.sse_ping_interval_seconds` (default 15, range 5 to 60) and `streaming.sse_channel_capacity` (default 32, range 16 to 64)

**Output**: the client SSE response. Rules: DESIGN 2.2 `No Buffering`, `Bounded Channels`; 3.3 `SSE Event Definitions`, `SSE Event Ordering` (P1 normative grammar `stream_started ping* (delta | tool)* citations? (done | error)`); 4 `SSE Infrastructure Requirements`; ADR-0010 (ping rule, `stream_interrupted`).

**Steps**:
1. [x] - `p1` - Respond with HTTP 200, `Content-Type: text/event-stream` and `Cache-Control: no-cache`, without a `Connection: keep-alive` header, with response compression disabled and no middleware that reads or buffers the body - `inst-relay-headers`
2. [x] - `p1` - Connect the provider task and the SSE writer through a bounded channel of `streaming.sse_channel_capacity` messages, so a slow client blocks the producer instead of growing memory - `inst-relay-channel`
3. [x] - `p1` - Write each event as `event: <name>` and `data: <JSON payload>` and flush it at once, without batching - `inst-relay-write`
4. [x] - `p1` - Emit `stream_started` first, exactly once - `inst-relay-first`
5. [x] - `p1` - **WHILE** no `delta` or `tool` event was sent yet and no terminal event arrived - `inst-relay-ping-phase`
   1. [x] - `p1` - Emit `event: ping` with `data` `{}` after each `sse_ping_interval_seconds` of idle time, resetting the timer on every event - `inst-relay-ping`
6. [x] - `p1` - After the first `delta` or `tool` event send no more `ping` events; the HTTP server sends an SSE comment keep-alive line every 30 s (hardcoded), which produces no client event - `inst-relay-keepalive`
7. [x] - `p1` - Relay `delta` and `tool` events in arrival order, interleaved as produced; relay at most one `citations` event, after the last `delta` and before the terminal event - `inst-relay-content`
8. [x] - `p1` - **IF** the provider task ends without handing over a terminal event (CAS lost to the orphan watchdog, or a panic) and the client is still connected - `inst-relay-interrupted`
   1. [x] - `p1` - Synthesize `error{code: "stream_interrupted"}` with a generic message; it matches the `failed` state committed by the winner - `inst-relay-emit-interrupted`
9. [x] - `p1` - Emit exactly one terminal event and close the connection immediately after it; never emit any event, `ping` included, after `done` or `error` - `inst-relay-close`
10. [x] - `p1` - **IF** the client drops the connection - `inst-relay-client-dropped`
    1. [x] - `p1` - Cancel the turn's cancellation token and write nothing more - `inst-relay-cancel`
11. [x] - `p1` - Increment `mini_chat_active_streams` when the stream opens and decrement it when it closes - `inst-relay-active-streams`
12. [x] - `p1` - **RETURN** when the connection is closed - `inst-relay-return`

### Tool Event Accounting and Mid-Turn Limits

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-tool-accounting`

**Input**: each internal `tool` event and text delta of the turn, `quota.web_search_max_calls_per_message` (default 2, greater than 0) and `quota.code_interpreter_max_calls_per_message` (default 10, greater than 0)

**Output**: relayed `tool` events, durable progress and counters on the turn, the in-memory completed counts, or a tool-limit breach. Rules: DESIGN 4 `Web Search Quota Enforcement (P1 Determinism Rules)`, `Code Interpreter Quota Enforcement (P1 Determinism Rules)`, `File Search Per-Turn Call Limit Enforcement (P1)`, 3.7 `Table: chat_turns`.

**Steps**:
1. [x] - `p1` - **IF** the event is a `tool` `start` named `web_search` - `inst-ta-web-start`
   1. [x] - `p1` - Increment the turn's web search start count; **IF** it now exceeds `quota.web_search_max_calls_per_message`, report the breach `web_search_calls_exceeded` and do not relay this start event - `inst-ta-web-limit`
2. [x] - `p1` - **IF** the event is a `tool` `start` named `code_interpreter` (including the Anthropic `code_execution` mapping) - `inst-ta-ci-start`
   1. [x] - `p1` - Increment the turn's code interpreter start count; **IF** it now exceeds `quota.code_interpreter_max_calls_per_message`, report the breach `code_interpreter_calls_exceeded` and do not relay this start event - `inst-ta-ci-limit`
3. [x] - `p1` - **IF** a breach was reported - `inst-ta-breach`
   1. [x] - `p1` - Cancel the provider stream before its usage arrives and hand the outcome `failed` with the breach code and no usage to the turn finalization algorithm; the settlement is therefore `estimated`, no surcharge is refunded, and the client receives SSE `error` with the breach code (never HTTP 429 and never `quota_exceeded`) - `inst-ta-breach-finalize`
4. [x] - `p1` - **IF** the event is a `tool` `done` - `inst-ta-done`
   1. [x] - `p1` - DB: increment `web_search_completed_count`, `code_interpreter_completed_count` or `file_search_completed_count` of the turn by one according to the tool name, with the update guarded by `state` `running`, so the orphan watchdog can recover the counts - `inst-ta-durable-count`
   2. [x] - `p1` - Increment the in-memory completed count of that tool; the in-memory file search count also counts `search_knowledge` calls (incremented before each retrieval runs) - `inst-ta-memory-count`
5. [x] - `p1` - Apply no mid-turn limit and no error code to `file_search` calls; they are bounded only by `max_tool_calls` and the reserve - `inst-ta-no-fs-limit`
6. [x] - `p1` - **IF** the adapter reports a function tool use while knowledge search is not active for the turn - `inst-ta-unexpected`
   1. [x] - `p1` - Hand the outcome `failed` with `unexpected_tool_use` and the usage when known to the turn finalization algorithm; the knowledge-search agentic loop and its `agentic_iterations_exceeded` outcome belong to `cpt-cf-mini-chat-feature-attachments-and-file-search` and finalize through the same algorithm - `inst-ta-unexpected-finalize`
7. [x] - `p1` - **IF** the event is a text delta or a `tool` event and at least 30 s passed since the last durable progress write - `inst-ta-progress`
   1. [x] - `p1` - DB: UPDATE `chat_turns.last_progress_at` to the current application time for the turn, guarded by `state` `running` - `inst-ta-progress-write`
8. [x] - `p1` - Relay every other `tool` event unchanged with its `phase` (`start` or `done`), `name` and non-sensitive `details` - `inst-ta-relay`
9. [x] - `p1` - **RETURN** the relayed event or the breach - `inst-ta-return`

### Citation Mapping

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-citations`

**Input**: the annotations of the completed response, the final assistant text parts and, when `file_search` was in the request, the in-memory map `provider_file_id` to `(attachment_id, filename)` built before the provider call by `cpt-cf-mini-chat-feature-attachments-and-file-search`

**Output**: the `citations` event payload `{items}` or nothing. Rules: DESIGN 3.3 `event: citations`, 4 `Citation File ID and Title Resolution`.

**Steps**:
1. [x] - `p1` - **IF** the stream did not complete normally (provider `incomplete`, failure, cancellation, tool-limit breach) - `inst-cit-not-completed`
   1. [x] - `p1` - **RETURN** nothing; no `citations` event is sent - `inst-cit-return-none-incomplete`
2. [x] - `p1` - **FOR EACH** annotation - `inst-cit-each`
   1. [x] - `p1` - **IF** it is a web (URL) annotation, build an item with `source` `web`, `url`, `title` (the page title), `span` `{start, end}` only when the annotation has both `start_index` and `end_index`, and `snippet` equal to the annotation text or, when absent, the answer text in that range applied as character offsets into the `output_text` part carrying the annotation (an out-of-range span gives an empty snippet) - `inst-cit-web`
   2. [x] - `p1` - **IF** it is a file annotation, resolve its `provider_file_id` through the map; build an item with `source` `file`, `attachment_id`, `title` equal to the attachment `filename`, `snippet` `""` and no `span`; omit the annotation when the id is unknown or the attachment is soft-deleted - `inst-cit-file`
3. [x] - `p1` - Never serialize `score` and never expose `provider_file_id`, `vector_store_id`, the provider response id or any other provider identifier - `inst-cit-no-provider-ids`
4. [x] - `p1` - **IF** no item remains - `inst-cit-empty`
   1. [x] - `p1` - **RETURN** nothing - `inst-cit-return-none`
5. [x] - `p1` - Persist no citation; a replay therefore sends none - `inst-cit-not-persisted`
6. [x] - `p1` - **RETURN** one `citations` payload with all items, sent once after the last `delta` and before the terminal event - `inst-cit-return`

### Turn Finalization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-finalization`

**Input**: the committed turn, the outcome (`completed` including provider `incomplete`, `failed` with an error code, `cancelled`), the provider usage when known, the provider response id when known, the accumulated assistant text, the in-memory completed tool counts, the decision and the context plan facts

**Output**: the committed terminal state with its settlement and outbox messages, the already-finalized no-op, or a finalization failure. This composes DESIGN 5.7 (`FinalizeTurn Invariant`, `Forbidden Patterns`, `Content durability invariant (completed ⇒ replayable)`, `Usage accounting rules per outcome`) with the contract `cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement` (called as `QuotaService::finalize_turn` with `Finalizer::Stream`).

**Steps**:
1. [x] - `p1` - Open one finalization transaction - `inst-fin-begin`
2. [x] - `p1` - **MATCH** the outcome - `inst-fin-match`
   - [x] - `p1` - **CASE** `completed`: DB: INSERT the assistant `messages` row with the pre-allocated id, the turn's `request_id`, `role` `assistant`, the accumulated text as is (possibly empty), `model` equal to the turn's `effective_model`, `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens` and `reasoning_tokens` from the provider usage (0 when missing), `provider_response_id`, `content_type` `text`, `request_kind` `chat`, `features_used` `[]`, `token_estimate` 0, `is_compressed` false; the CAS update sets `assistant_message_id`, `provider_response_id`, `completed_at` and keeps `error_code` NULL (the incomplete reason is never written to `error_code`) - `inst-fin-completed`
   - [x] - `p1` - **CASE** `cancelled` with non-empty accumulated text: insert the partial assistant message the same way with token columns 0, and set `assistant_message_id`; when the insert fails, log a warning, keep the transaction usable (the failed insert must not abort the finalization) and finalize as `cancelled` with `assistant_message_id` NULL - `inst-fin-cancelled-partial`
   - [x] - `p1` - **CASE** `cancelled` with empty text, or `failed`: insert no assistant message; the CAS update sets `error_code` (for `failed`), a non-sensitive sanitized `error_detail`, `provider_response_id` when known and `completed_at`, and leaves `assistant_message_id` NULL; partial text of a failed turn is discarded - `inst-fin-failed-or-empty`
3. [x] - `p1` - Call `QuotaService::finalize_turn` inside the transaction with the tenant id, the turn id, the terminal update of the outcome, `Finalizer::Stream` with the chat's selected model and the period starts of the decision, the usage (none for `cancelled` and for tool-limit breaches) and the completed counts (`web_search`, `code_interpreter`, and the in-memory `file_search` count including `search_knowledge` calls); the contract performs the CAS on `state` `running`, derives the billing outcome, settles the buckets and enqueues the one usage event - `inst-fin-settle`
4. [x] - `p1` - **IF** the contract reports `AlreadyFinalized` - `inst-fin-cas-lost`
   1. [x] - `p1` - Roll back the transaction (so no assistant message of the loser remains), change nothing, emit nothing for the provider terminal, and **RETURN** the no-op; the relay then sends `stream_interrupted` when the client is still connected - `inst-fin-return-noop`
5. [x] - `p1` - Build the turn audit event through the turn audit event algorithm and enqueue it to the audit queue (`outbox.audit_queue_name`, default `mini-chat.audit`, partitioned by `tenant_id`) in the same transaction - `inst-fin-audit`
6. [x] - `p1` - **IF** the outcome is `completed` and `thread_summary_worker.enabled` is true - `inst-fin-summary-gate`
   1. [x] - `p1` - Evaluate the thread-summary trigger and, when it fires, enqueue the work in the same transaction through the thread-summary trigger algorithm - `inst-fin-summary`
7. [x] - `p1` - **TRY** - `inst-fin-try`
   1. [x] - `p1` - Commit the transaction; the state transition, the assistant message, the settlement, the usage event, the audit event and the summary work commit together or not at all - `inst-fin-commit`
8. [x] - `p1` - **CATCH** a failure of the message insert of a `completed` outcome - `inst-fin-catch-message`
   1. [x] - `p1` - Roll back and run this algorithm again with the outcome `failed`, `error_code` `message_persistence_failed` and the same usage (settled `actual` when usage is known, otherwise `estimated`), so `completed` is never observable without its message - `inst-fin-retry-as-failed`
9. [x] - `p1` - **CATCH** any other failure of the transaction (settlement computation, policy snapshot, enqueue, database) - `inst-fin-catch-other`
   1. [x] - `p1` - Roll back; the turn stays `running` and keeps its reserve until the orphan watchdog finalizes it; **RETURN** a finalization failure to the terminal emission algorithm - `inst-fin-return-failure`
10. [x] - `p1` - After the commit fire the outbox wakes returned by the settlement and the enqueues, record the quota metrics of the settlement result, `mini_chat_finalization_latency_ms`, and `mini_chat_thread_summary_trigger` when the trigger was evaluated - `inst-fin-after-commit`
11. [x] - `p1` - Never update messages first and the turn state later in a separate transaction, never enqueue an outbox message outside this transaction, and never let a non-winning path persist content, settle or enqueue - `inst-fin-forbidden`
12. [x] - `p1` - **RETURN** the committed terminal state, the settlement result and the assistant message id - `inst-fin-return`

### Terminal Event Emission

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-terminal-emission`

**Input**: the finalization result, the outcome, the error code of a failed outcome, the sanitized message, the decision, the citations payload and the settlement facts

**Output**: the single terminal SSE event, gated on the finalization commit (DESIGN 5.7 `Terminal SSE Event Emission Guard (P1)`).

**Steps**:
1. [x] - `p1` - Hold back the provider's terminal event until the finalization result is known; non-terminal events (`delta`, `tool`, `citations`) are not gated - `inst-emit-hold`
2. [x] - `p1` - **MATCH** the finalization result - `inst-emit-match`
   - [x] - `p1` - **CASE** committed as `completed`: emit `citations` when the citation mapping returned a payload, then `done` with `usage` (`input_tokens`, `output_tokens`; 0 when the provider reported none; cache and reasoning counts never serialized), `effective_model`, `selected_model`, `quota_decision`, `downgrade_from` (equal to `selected_model`) and `downgrade_reason` only for `downgrade`, and `quota_warnings` from `QuotaService::quota_warnings` read after the settlement - `inst-emit-done`
   - [x] - `p1` - **CASE** committed as `failed` with `message_persistence_failed`: emit `error{code: "message_persistence_failed"}` - `inst-emit-persistence-failed`
   - [x] - `p1` - **CASE** committed as `failed` with another code: emit `error` with that code and the sanitized message - `inst-emit-error`
   - [x] - `p1` - **CASE** finalization failure on a `completed` or `incomplete` stream: emit `error{code: "finalization_failed"}` instead of `done` - `inst-emit-finalization-failed`
   - [x] - `p1` - **CASE** finalization failure on an error path (provider error, tool-limit breach, unexpected tool use): emit `error` with the original code - `inst-emit-error-path-failure`
   - [x] - `p1` - **CASE** CAS lost: emit nothing from the provider task; the relay synthesizes `stream_interrupted` - `inst-emit-cas-lost`
   - [x] - `p1` - **CASE** committed as `cancelled`: emit nothing (the stream is already broken) - `inst-emit-cancelled`
3. [x] - `p1` - **IF** the `quota_warnings` read fails - `inst-emit-warnings-failed`
   1. [x] - `p1` - Log a warning and send `done` without `quota_warnings`; the committed turn is not affected - `inst-emit-warnings-omitted`
4. [x] - `p1` - Never put `request_id`, `message_id`, `turn_id`, `provider_response_id`, `reserved_credits_micro`, billing outcome or any provider identifier into `done`, and never put a quota scope into `error` - `inst-emit-no-internal-ids`
5. [x] - `p1` - **RETURN** the terminal event to the relay, which flushes it and closes the connection - `inst-emit-return`

### Turn Audit Event

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-turn-audit-event`

**Input**: the turn row, the outcome, the usage, the tool counts, the decision (stream path) or the orphan facts (watchdog path), and the current trace id

**Output**: one `TurnAuditEvent` payload for the audit queue, delivered by the audit delivery algorithm of `cpt-cf-mini-chat-feature-gear-foundation`. Rules: DESIGN 3.9 `Audit Events for Turn Mutations` (turn finalization paragraph), 2.2 `Model Locked Per Chat`, ADR-0009.

**Steps**:
1. [x] - `p1` - Set `event_type` to `turn_completed` for state `completed` (including a provider `incomplete`) and to `turn_failed` for every other terminal state, `cancelled` and `orphan_timeout` included; there is no `turn_cancelled` value - `inst-audit-type`
2. [x] - `p1` - Set `tenant_id`, `user_id` (the requester), `chat_id`, `turn_id`, `request_id`, `requester_type` `user`, `terminal_state`, `error_code`, `usage` (the provider-reported counts, or null when unknown), `latency_ms` from the turn's `started_at` to the finalization, `web_search_calls`, `file_search_calls` and `code_interpreter_calls`, `trace_id` of the active span (absent when none) and `timestamp` - `inst-audit-fields`
3. [x] - `p1` - On the stream path set `selected_model` from `chats.model`, `effective_model` from the turn, and the quota decision with `downgrade_from` and `downgrade_reason` from the preflight decision - `inst-audit-stream-models`
4. [x] - `p1` - On the orphan watchdog path set `selected_model` equal to the persisted `effective_model` (empty when the turn has no reserve columns) and the quota decision `unknown` - `inst-audit-orphan-models`
5. [x] - `p1` - Leave `prompt`, `response`, `attachments`, `license` and `quota_scope` empty, so no content reaches the audit plugin and no redaction runs; never include a provider identifier - `inst-audit-empty-content`
6. [x] - `p1` - **RETURN** the payload for enqueue in the caller's finalization transaction - `inst-audit-return`

### Thread Summary Trigger and Enqueue

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-summary-trigger`

**Input**: the CAS-winning `completed` turn, `assembled_context_tokens`, `messages_truncated`, `effective_budget`, whether a summary existed at assembly, `thread_summary_worker.compression_threshold_pct` (default 80, range 1 to 99) and the open finalization transaction

**Output**: an enqueued thread-summary work item, or `not_needed`. Rules: DESIGN 3.6 `Thread Summary Update` (`Trigger timing`, `Execution stages and invariants` items 1 and 2, `Thread Summary - Stable Range and Commit Invariant`); the worker that consumes the item belongs to `cpt-cf-mini-chat-feature-background-maintenance`.

**Steps**:
1. [x] - `p1` - Evaluate the trigger from the estimate already computed by context assembly, never from message or turn counts alone - `inst-sum-estimate-source`
2. [x] - `p1` - Fire proactively when no summary existed and `assembled_context_tokens` is at least `effective_budget` multiplied by `compression_threshold_pct` divided by 100; fire urgently when `messages_truncated` is true; otherwise (a summary exists and nothing was truncated) do not fire - `inst-sum-conditions`
3. [x] - `p1` - **IF** the trigger does not fire - `inst-sum-no-fire`
   1. [x] - `p1` - **RETURN** without evaluation result; no metric is recorded for this turn - `inst-sum-return-no-fire`
4. [x] - `p1` - DB: read the base frontier `(summarized_up_to_created_at, summarized_up_to_message_id)` from `thread_summaries` (empty when no row) - `inst-sum-base`
5. [x] - `p1` - DB: compute the frozen target frontier as the latest non-deleted message of the chat, in the order `(created_at, id)`, that does not belong to the causing turn (its `request_id` differs), because the latest turn may still be replaced by retry, edit or delete - `inst-sum-target`
6. [x] - `p1` - **IF** no such message exists, or the base frontier already equals or passes the target - `inst-sum-nothing-to-do`
   1. [x] - `p1` - **RETURN** `not_needed` - `inst-sum-return-not-needed`
7. [x] - `p1` - Enqueue in the finalization transaction one message to `outbox.thread_summary_queue_name` (default `mini-chat.thread_summary`, partitioned by `chat_id`) with `tenant_id`, `chat_id`, a new UUID v4 `system_request_id` that stays stable across every retry of this message, `base_frontier_created_at`, `base_frontier_message_id` (null when the frontier is empty), `frozen_target_created_at`, `frozen_target_message_id` and `system_task_type` `thread_summary_update` - `inst-sum-enqueue`
8. [x] - `p1` - Never block or change the current turn's response or context plan; a summary produced later serves only later turns - `inst-sum-non-blocking`
9. [x] - `p1` - **RETURN** `scheduled`; the caller records `mini_chat_thread_summary_trigger` with `result` `scheduled` or `not_needed` after the commit - `inst-sum-return-scheduled`

### Provider Error Mapping and Sanitization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-error-sanitization`

**Input**: a provider failure (HTTP status and body, `Retry-After`, transport error, timeout, `response.failed` or `error` event) or any text that may reach a client

**Output**: a streaming error code with a sanitized message, the internal provider code and message for logs, and the stored `error_code`. Rules: DESIGN 3.3 `Provider Event Translation`, `Streaming error codes`, `Provider identifier non-exposure invariant`.

**Steps**:
1. [x] - `p1` - **MATCH** the failure - `inst-err-match`
   - [x] - `p1` - **CASE** provider HTTP 429: `rate_limited`; when the provider sent a numeric `Retry-After`, the message includes the delay in seconds (SSE `error` has no separate retry field) - `inst-err-429`
   - [x] - `p1` - **CASE** a gateway timeout or the gateway's own HTTP 504 `deadline_exceeded` Problem: `provider_timeout` - `inst-err-timeout`
   - [x] - `p1` - **CASE** any other non-successful status (a provider's own 504 with its JSON error body included), an invalid response, an unavailable provider, a broken provider stream, `response.failed` or a provider `error` event: `provider_error` - `inst-err-provider-error`
2. [x] - `p1` - Keep the provider's own code and message internally for logs; the client `message` is the sanitized provider message, or a generic text for the code when no provider message exists or nothing remains after sanitizing - `inst-err-internal-keep`
3. [x] - `p1` - Sanitize every client-visible message, matching each prefix only at the start of a token (not preceded by a letter, digit, `_` or `-`): replace with `[provider_id]` each response, completion, message, request, organization, project or tool-use id (`resp_`, `chatcmpl-`, `cmpl-`, `msg_`, `req_`, `org-`, `proj_`, `toolu_` followed by letters and digits), each call, run, thread, file, assistant or vector store id (`call_`, `run_`, `thread_`, `file-`, `file_`, `assistant-`, `vs_` followed by at least 12 letters and digits), and each request-id style token (a UUID in hyphenated form, or a run of at least 16 hexadecimal characters); replace each URL with `[url]`; replace with `[credential]` each `sk-` key (`sk-` followed by at least 10 characters from letters, digits, `_`, `-` and `*`, which covers project keys such as `sk-proj-…` and masked forms such as `sk-proj-****abcd`) and each `Bearer` token (the word `Bearer` with the token that follows it); leave everything else unchanged, so ordinary words such as `file-based`, `file_search` and `thread_summary` stay intact - `inst-err-sanitize`
4. [x] - `p1` - **IF** the provider failure is an authentication or authorization failure (provider HTTP 401 or 403), or the sanitized text still contains an unrecognized id-like token (a run of at least 20 letters, digits and `_` that mixes letters and digits) - `inst-err-generic-fallback`
   1. [x] - `p1` - Use the generic message of the streaming error code instead of any provider text, so no provider-issued identifier (provider correlation ids and any other provider-scoped id included) and no credential reaches a client (DESIGN 3.3 `Provider identifier non-exposure invariant`) - `inst-err-generic-message`
5. [x] - `p1` - Apply the same sanitization and the same generic fallback to `chat_turns.error_detail` and to every REST `detail` derived from a provider failure - `inst-err-sanitize-stored`
6. [x] - `p1` - Store the code in `chat_turns.error_code` for `provider_error`, `provider_timeout`, `rate_limited`, `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `agentic_iterations_exceeded`, `unexpected_tool_use` and `message_persistence_failed`; never store the SSE-only codes `finalization_failed` and `stream_interrupted` - `inst-err-store`
7. [x] - `p1` - Never map a provider 429 to a quota 429 and never place a quota scope in an SSE `error` - `inst-err-no-quota-mapping`
8. [x] - `p1` - **RETURN** the code, the sanitized message and the internal details - `inst-err-return`

### Send Pipeline Error Contract

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-send-error-contract`

**Input**: any rejection raised by the send, replay, list or turn status path before a stream opens

**Output**: the canonical Problem, built by the error mapping algorithm of `cpt-cf-mini-chat-feature-gear-foundation` (`cpt-cf-mini-chat-algo-gear-foundation-error-mapping`). The rows are DESIGN 3.3 `Error Codes` (Option A error model: no SSE stream for any pre-stream failure).

**Steps**:
1. [x] - `p1` - **MATCH** the rejection - `inst-sec-match`
   - [x] - `p1` - **CASE** chat, turn or message not visible: 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.chat.v1~` or `gts.cf.core.mini_chat.turn.v1~` - `inst-sec-404`
   - [x] - `p1` - **CASE** body or path errors: 415 `missing_json_content_type`, 400 `json_syntax_error`, 422 `invalid_json_body`, 400 `invalid_path_params` - `inst-sec-body`
   - [x] - `p1` - **CASE** content and attachment errors: 400 `EMPTY_CONTENT` (`content`), 400 `invalid_attachment` (`attachment`), 400 `out_of_range` `TOO_MANY_IMAGES` (`image_count`) - `inst-sec-content`
   - [x] - `p1` - **CASE** model and capability errors: 400 `INVALID_MODEL` (`model`), 400 `VISION_NOT_SUPPORTED` (`content_type`), 400 `failed_precondition` with `violations[{subject: web_search|images, type: FEATURE_DISABLED}]` - `inst-sec-model`
   - [x] - `p1` - **CASE** budget errors: 400 `out_of_range` `INPUT_TOO_LONG` (`content`) and `CONTEXT_BUDGET_EXCEEDED` (`context`) - `inst-sec-budget`
   - [x] - `p1` - **CASE** conflicts: 409 `aborted` with `context.reason` `request_id_conflict` or `turn_already_running`; the defensive replay outcome 409 `aborted` with reason `REPLAY` is intercepted by the handler, which serves the replay instead - `inst-sec-409`
   - [x] - `p1` - **CASE** quota: 429 `resource_exhausted` with `violations[{subject: tokens|web_search|code_interpreter, description: "quota_exceeded"}]` - `inst-sec-429`
   - [x] - `p1` - **CASE** authorization: 401, 403 `AUTHZ_DENIED` or `LICENSE_FEATURE_REQUIRED`, 503 with `Retry-After: 5` - `inst-sec-authz`
   - [x] - `p1` - **CASE** provider or policy resolution failure before streaming, internal or database error: 500 `internal` with a generic detail - `inst-sec-500`
2. [x] - `p1` - Keep `detail` human-readable and outside the contract; clients branch on the status, the category and the machine-readable fields only - `inst-sec-detail`
3. [x] - `p1` - Declare 200 (`text/event-stream`), 400, 401, 403, 404, 409, 422, 429, 500 and 503 for `messages:stream`, 200, 400, 401, 403, 404, 500 and 503 for the messages list and the turn status in the OpenAPI document; 415 is not declared - `inst-sec-openapi`
4. [x] - `p1` - **RETURN** the Problem - `inst-sec-return`

### Turn Status Projection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-turn-status`

**Input**: a non-deleted `chat_turns` row

**Output**: the `TurnStatusResponse` body (DESIGN 3.3 `Turn Status API`, `Internal-to-API state mapping`).

**Steps**:
1. [x] - `p1` - **MATCH** `chat_turns.state` - `inst-ts-match`
   - [x] - `p1` - **CASE** `running`: `state` `running` - `inst-ts-running`
   - [x] - `p1` - **CASE** `completed`: `state` `done` - `inst-ts-done`
   - [x] - `p1` - **CASE** `failed`: `state` `error` - `inst-ts-error`
   - [x] - `p1` - **CASE** `cancelled`: `state` `cancelled` - `inst-ts-cancelled`
2. [x] - `p1` - Set `request_id` and `updated_at`; never include `chat_id` (it is in the path) - `inst-ts-base-fields`
3. [x] - `p1` - Include `error_code` from `chat_turns.error_code` only when `state` is `error` (for example `provider_error`, `orphan_timeout`, `context_length_exceeded`, `turn_setup_failed`, `quota_exceeded`, `rate_limited`, `web_search_calls_exceeded`), and omit it otherwise - `inst-ts-error-code`
4. [x] - `p1` - Include `assistant_message_id` when `state` is `done`, and when `state` is `cancelled` with a persisted partial message; omit it while `running` and on `error` - `inst-ts-message-id`
5. [x] - `p1` - Omit absent optional fields instead of sending `null`, and never expose the billing outcome, the settlement, the reserve or a provider identifier - `inst-ts-no-internal`
6. [x] - `p1` - **RETURN** the body - `inst-ts-return`

### Message List Query

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-message-list-query`

**Input**: the query parameters of `GET /v1/chats/{id}/messages`

**Output**: a validated filter, order, page size and cursor, or a 400 with `context.resource_type` `gts.cf.core.odata.query.v1~`. The validation rules and reason codes are those of `cpt-cf-mini-chat-algo-chat-management-list-query` (platform OData extractor and library), applied to the message fields of DESIGN 3.3 `List Messages` and the reference OpenAPI (`x-odata-filter`, `x-odata-orderby`).

**Steps**:
1. [x] - `p1` - Apply the shared rules: `INVALID_QUERY_PARAMS` for an unparsable query string, a non-numeric, negative, fractional, empty or overflowing `limit`, or `limit` together with `$top`; `UNSUPPORTED_QUERY_PARAM` for `$` options other than `$filter`, `$orderby` and `$select`; `INVALID_SELECT` for a bad `$select` (a valid one is ignored and undeclared); `FILTER_TOO_LONG` and `FILTER_TOO_COMPLEX` - `inst-mq-shared`
2. [x] - `p1` - Resolve the page size: absent gives 20, 1 to 100 is kept, above 100 is clamped to 100, `0` gives `INVALID_LIMIT` with field `$top` - `inst-mq-limit`
3. [x] - `p1` - Validate `$filter`: `created_at` with `eq`, `ne`, `gt`, `ge`, `lt`, `le`, `in`; `id` with `eq`, `ne`, `in`; `role` with `eq`, `ne`, `contains`, `startswith`, `endswith`, `in`; anything else gives `INVALID_FILTER` - `inst-mq-filter`
4. [x] - `p1` - Validate `$orderby`: `created_at asc`, `created_at desc`, `id asc`, `id desc`, `role asc`, `role desc`; anything else gives `INVALID_ORDERBY_FIELD` - `inst-mq-orderby`
5. [x] - `p1` - **IF** neither `$orderby` nor `cursor` is given - `inst-mq-default`
   1. [x] - `p1` - Use the default order `created_at asc` with `id asc` as tie-break (chronological) - `inst-mq-default-order`
6. [x] - `p1` - Always append `id` in the same direction as a final tie-break to any requested order, so pages are total and stable - `inst-mq-tiebreak`
7. [x] - `p1` - Treat `cursor` as opaque: undecodable gives `INVALID_CURSOR`, another order gives `ORDER_MISMATCH`, another filter gives `FILTER_MISMATCH`, `cursor` together with `$orderby` gives `ORDER_WITH_CURSOR`; the cursor serves next and previous navigation - `inst-mq-cursor`
8. [x] - `p1` - On SQLite, order, filter and build the keyset predicate and the cursor value on `created_at` through the fixed-width sort key of the chat page module (`TimeStorage::Rfc3339Text`), never by raw text comparison; on PostgreSQL use the native timestamp - `inst-mq-sqlite-key`
9. [x] - `p1` - Raise errors found by the repository while paginating with the same `context.resource_type` and reasons - `inst-mq-repo-errors`
10. [x] - `p1` - **RETURN** the validated query or the first violation - `inst-mq-return`

### Message Projection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-message-projection`

**Input**: one page of `messages` rows and the caller's user id

**Output**: the `MiniChatMessageDto` items. Rules: DESIGN 3.3 `List Messages`, 3.1 `Message`, 3.7 `Table: message_attachments` (Semantics).

**Steps**:
1. [x] - `p1` - **IF** any row of the page has a NULL `request_id` - `inst-mp-null-request-id`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail and log the row id; a message is never serialized without `request_id` - `inst-mp-return-500`
2. [x] - `p1` - DB: batch-load the attachments of the page with one query: `message_attachments` of the page's message ids inner-joined to `attachments` with `attachments.deleted_at IS NULL`, projecting `attachment_id`, `kind`, `filename`, `status` and the thumbnail columns - `inst-mp-attachments`
3. [x] - `p1` - DB: batch-load the reactions of the page with one query on `message_reactions` for the caller's `user_id` and the page's message ids - `inst-mp-reactions`
4. [x] - `p1` - **FOR EACH** row - `inst-mp-each`
   1. [x] - `p1` - Emit the required fields `id`, `request_id`, `role` (`user`, `assistant` or `system`), `content`, `created_at` (RFC 3339), `attachments` and `my_reaction` - `inst-mp-required`
   2. [x] - `p1` - Set `attachments` to the row's `AttachmentSummary` list (`attachment_id`, `kind`, `filename`, `status`, and `img_thumbnail` with `content_type`, `width`, `height` and `data_base64` only for an image with `status` `ready` and a stored thumbnail), or to an empty array; assistant and system messages always have an empty array - `inst-mp-attachment-list`
   3. [x] - `p1` - Set `my_reaction` to `like` or `dislike` for an assistant message the caller reacted to, and to `null` otherwise; user and system messages always carry `null`; the field is always present - `inst-mp-reaction`
   4. [x] - `p1` - Emit `model` only when `messages.model` is set (assistant messages) and `input_tokens` and `output_tokens` only when the stored value is not 0; omit them otherwise - `inst-mp-optional`
5. [x] - `p1` - Never emit `tenant_id`, `chat_id`, `deleted_at`, `is_compressed`, `provider_response_id`, `token_estimate`, cache or reasoning counts, or a provider file id; compressed messages stay listed - `inst-mp-no-internal`
6. [x] - `p1` - **RETURN** the items in page order - `inst-mp-return`

### Message Timestamps, Order and Count

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-message-order`

**Input**: every message write of this feature (user message in the reserve transaction, assistant or partial message in the finalization)

**Output**: a chronological order that is stable across turns and a `message_count` consistent with the list. Rules: DESIGN 3.7 `Table: messages` (`Ordering note (normative)`), 3.1 `Chat` (`message_count`); implementation note 1.

**Steps**:
1. [x] - `p1` - Take `created_at` of every message from the application UTC clock at the write - `inst-mo-clock`
2. [x] - `p1` - Set the assistant (or partial) message's `created_at` strictly after its user message's `created_at`: when the clock reading is not later, use the user message's `created_at` plus one microsecond - `inst-mo-assistant-after-user`
3. [x] - `p1` - Rely on the parallel-turn guard for the order across turns: a new user message is written only after the previous turn of the chat left `running`, so its `created_at` follows the previous turn's messages - `inst-mo-across-turns`
4. [x] - `p1` - Order every server-side message selection by the strict total order `(created_at, id)`, with `id` only as a deterministic tie-break, and compare SQLite timestamps through the fixed-width sort key - `inst-mo-total-order`
5. [x] - `p1` - Keep `message_count` consistent with the list: a completed turn adds exactly two non-deleted messages (user and assistant), a failed turn and a cancelled turn without text add one (the user message), a cancelled turn with a persisted partial message adds two, a rejected or rolled-back send adds none, and a replay adds none; the count itself is computed by `cpt-cf-mini-chat-algo-chat-management-message-count` - `inst-mo-count`
6. [x] - `p1` - **RETURN** the ordering contract - `inst-mo-return`

### Orphan Watchdog

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog`

**Input**: the leader elector prepared at gear start, `orphan_watchdog.enabled` (default true), `orphan_watchdog.timeout_secs` (default 300, range 90 to 3600), `orphan_watchdog.scan_interval_secs` (default 60, range 1 to 3600) and the gear cancellation token

**Output**: stale `running` turns finalized as `failed` with `orphan_timeout`, each with its estimated settlement, its usage event and its audit event. Rules: DESIGN 4 `Orphan Turn Watchdog`, `Watchdog Single-Actor Guarantee (P1 Mandatory)`, B.9.1 `Orphan watchdog semantics`, ADR-0010; the worker is spawned by the gear start of `cpt-cf-mini-chat-feature-gear-foundation` when enabled.

**Steps**:
1. [x] - `p1` - **WHILE** the gear cancellation token is not cancelled - `inst-ow-loop`
   1. [x] - `p1` - Wait `orphan_watchdog.scan_interval_secs`, ending early on cancellation - `inst-ow-wait`
   2. [x] - `p1` - **IF** this process is not the current leader of the role `mini-chat-orphan-watchdog` (Kubernetes Lease with the `k8s` feature, 15 s lease and 2 s renew; the no-op elector always reports leadership) - `inst-ow-not-leader`
      1. [x] - `p1` - Skip the scan and claim nothing - `inst-ow-skip`
   3. [x] - `p1` - Compute `cutoff` as the application UTC clock minus `orphan_watchdog.timeout_secs` - `inst-ow-cutoff`
   4. [x] - `p1` - DB: SELECT at most 100 candidate turns with `state` `running`, `deleted_at IS NULL` and `last_progress_at` at most `cutoff`, or `last_progress_at` NULL and `started_at` at most `cutoff`, oldest progress first; on SQLite compare through the fixed-width sort key - `inst-ow-scan`
   5. [x] - `p1` - **FOR EACH** candidate, while leadership holds - `inst-ow-each`
      1. [x] - `p1` - Record `mini_chat_orphan_detected` with `reason` `stale_progress` - `inst-ow-detected`
      2. [x] - `p1` - Open one transaction and call `QuotaService::finalize_turn` with `Finalizer::Orphan` carrying `cutoff`, the update `state` `failed`, `error_code` `orphan_timeout`, `completed_at` and `updated_at` at the application time, no usage and the durable completed counts of the row; the contract's CAS re-checks `state` `running`, `deleted_at IS NULL` and the stale-progress predicate, derives the period starts from `started_at`, settles on the estimated path (`aborted`) and enqueues the usage event (with the placeholder event and no bucket change when the reserve columns or `requester_user_id` are NULL) - `inst-ow-finalize`
      3. [x] - `p1` - **IF** the contract reports `AlreadyFinalized` - `inst-ow-cas-lost`
         1. [x] - `p1` - Roll back and skip the turn: no settlement, no usage or audit event, no orphan-finalized metric - `inst-ow-skip-turn`
      4. [x] - `p1` - Enqueue the turn audit event of the orphan path (`turn_failed`, `selected_model` equal to the effective model, quota decision `unknown`) in the same transaction and commit - `inst-ow-audit`
      5. [x] - `p1` - **IF** the transaction fails - `inst-ow-tx-failed`
         1. [x] - `p1` - Roll back, log the cause, leave the turn `running` and continue with the next candidate; a later scan retries it - `inst-ow-retry-later`
      6. [x] - `p1` - After the commit fire the outbox wakes and record `mini_chat_orphan_finalized` with `reason` `stale_progress` and `mini_chat_streams_aborted` with `trigger` `orphan_timeout` - `inst-ow-finalized-metrics`
   6. [x] - `p1` - Record `mini_chat_orphan_scan_duration_seconds` for the scan - `inst-ow-scan-duration`
2. [x] - `p1` - Never create or modify `messages` rows, never touch a turn that left `running` or made progress after discovery, and never let a late provider result reopen or overwrite a turn the watchdog finalized - `inst-ow-invariants`
3. [x] - `p1` - Never block a user-visible request path; the watchdog bounds the time a chat stays locked by a stale `running` turn - `inst-ow-non-blocking`
4. [x] - `p1` - **RETURN** when the token is cancelled - `inst-ow-return`

### Streaming Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-streaming-metrics`

**Input**: the stream, cancellation, finalization and watchdog events of this feature

**Output**: the recorded series with the `metrics.prefix` prefix (default `mini_chat`) and the label keys of DESIGN 4 `Naming and cardinality rules`.

**Steps**:
1. [x] - `p1` - Record `mini_chat_stream_started` (labels `provider`, `model`) when a new generation opens its stream, and `mini_chat_stream_completed` when a turn commits as `completed` - `inst-met-started-completed`
2. [x] - `p1` - Record `mini_chat_stream_incomplete` with `provider`, `model` and `reason` (the adapter's incomplete reason) for a provider `incomplete`, which also records `mini_chat_stream_completed` - `inst-met-incomplete`
3. [x] - `p1` - Record `mini_chat_stream_failed` with `provider`, `model` and `error_code` (a streaming error code) for every terminal `error` sent by a new generation - `inst-met-failed`
4. [x] - `p1` - Record `mini_chat_ttft_overhead_ms` from the provider's first streamed token to its send on the internal channel, `mini_chat_ttft_provider_ms` in the provider call algorithm, and `mini_chat_stream_total_latency_ms` from the request receipt to the terminal event, all with `provider` and `model` - `inst-met-latency`
5. [x] - `p1` - Record `mini_chat_stream_disconnected` with `stage` `before_first_token` when no `delta` was sent yet and `mid_stream` otherwise - `inst-met-disconnected`
6. [x] - `p1` - Record `mini_chat_cancel_requested` with `trigger` `disconnect` for a disconnect observed through the cancellation token (not for one detected by a failed channel send), `mini_chat_cancel_effective` with `trigger` `disconnect` when a cancelled turn's provider stream was cancelled and the turn finalized, `mini_chat_time_to_abort_ms` from the observed disconnect until the provider stream is cancelled and the read loop exits (finalization excluded), and `mini_chat_streams_aborted` with `trigger` `client_disconnect` for a committed `cancelled` turn - `inst-met-cancel`
7. [x] - `p1` - Record `mini_chat_image_inputs_per_turn` once per provider request that carries at least one image, with the image count - `inst-met-images`
8. [x] - `p1` - Record nothing for a replay (`mini_chat_stream_replay` stays declared and unrecorded) and never record `tool_calls`, `tool_call_limited`, `web_search_latency_ms`, `citations_count`, `citations_by_source`, `context_truncation`, `provider_requests`, `provider_errors`, `provider_latency_ms`, `tokens_after_cancel`, `time_from_ui_disconnect_to_cancel_ms` or `cancel_orphan`; the `trigger` values `user_stop`, `timeout` and `internal_abort` are never emitted - `inst-met-not-recorded`
9. [x] - `p1` - Never use `tenant_id`, `user_id`, `chat_id`, `request_id`, `provider_response_id`, file names or free-form error text as label values - `inst-met-cardinality`
10. [x] - `p1` - **RETURN** the recorded series - `inst-met-return`

### Streaming Configuration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-message-streaming-configuration`

**Input**: the gear configuration validated by `cpt-cf-mini-chat-algo-gear-foundation-config-validation` and the policy snapshot

**Output**: the values this feature reads (DESIGN Appendix B); validation, unknown-key rejection and defaults are applied by the foundation, and this feature adds no key.

**Steps**:
1. [x] - `p1` - Read `streaming.sse_ping_interval_seconds` (default 15, 5 to 60), `streaming.sse_channel_capacity` (default 32, 16 to 64) and `streaming.max_output_tokens` (default 32768, the cap of `max_output_tokens_applied`) - `inst-cfg-streaming`
2. [x] - `p1` - Read `context.recent_messages_limit` (default 10, 0 to 100), `context.web_search_guard` and `context.file_search_guard` (built-in texts) - `inst-cfg-context`
3. [x] - `p1` - Read `quota.web_search_max_calls_per_message` (default 2) and `quota.code_interpreter_max_calls_per_message` (default 10), both greater than 0 - `inst-cfg-tool-limits`
4. [x] - `p1` - Read `rag.max_images_per_message` (default 4) and `rag.max_documents_per_chat` (default 50) - `inst-cfg-rag`
5. [x] - `p1` - Read `orphan_watchdog.enabled`, `orphan_watchdog.timeout_secs` and `orphan_watchdog.scan_interval_secs` once at gear initialization; a change takes effect after a restart and then applies to every running turn - `inst-cfg-watchdog`
6. [x] - `p1` - Read `thread_summary_worker.enabled` (default true) and `thread_summary_worker.compression_threshold_pct` (default 80, 1 to 99) - `inst-cfg-summary`
7. [x] - `p1` - Read the catalog fields of the effective model from the policy snapshot on every request (`provider_model_id`, `provider_id`, `system_prompt`, `context_window`, `max_input_tokens`, `max_output_tokens`, `estimation_budgets`, `multimodal_capabilities`, `general_config.tool_support`, `general_config.api_params`, `web_search_context_size`, `max_num_results`, `max_tool_calls`) and the kill switches; no snapshot is cached - `inst-cfg-catalog`
8. [x] - `p1` - Treat the removed key `streaming.web_search_context_size` as a startup error (unknown key) and take the search context size from the catalog - `inst-cfg-removed`
9. [x] - `p1` - **RETURN** the values - `inst-cfg-return`

## 4. States (CDSL)

### Turn Lifecycle State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-message-streaming-turn-lifecycle`

**States**: running, completed, failed, cancelled

**Initial State**: running (the `chat_turns` row is inserted in the reserve transaction, after the user message; pre-reserve failures on the send path create no row)

**Transitions**:
1. [x] - `p1` - **FROM** running **TO** completed **WHEN** the provider sent `response.completed` or `response.incomplete` (or the adapter equivalent), the assistant message was inserted, and the stream finalization won the CAS `state = 'running'` - `inst-turn-to-completed`
2. [x] - `p1` - **FROM** running **TO** failed **WHEN** the provider failed (`provider_error`, `provider_timeout`, `rate_limited`), a per-message tool limit was breached, a function tool was unexpected, or the assistant message could not be persisted (`message_persistence_failed`), and the stream finalization won the CAS - `inst-turn-to-failed`
3. [x] - `p1` - **FROM** running **TO** cancelled **WHEN** the client disconnected before the terminal event (cancellation token or failed channel send) and the cancellation finalization won the CAS - `inst-turn-to-cancelled`
4. [x] - `p1` - **FROM** running **TO** failed **WHEN** the orphan watchdog won its CAS with `state` `running`, `deleted_at IS NULL` and stale progress older than `orphan_watchdog.timeout_secs`, setting `error_code` `orphan_timeout` - `inst-turn-to-orphan-failed`
5. [x] - `p1` - **FROM** completed **TO** completed **WHEN** any later terminal signal arrives: the CAS affects 0 rows and the signal is discarded (the same holds for failed and cancelled; terminal states are immutable and the first committed terminal transition is authoritative) - `inst-turn-terminal-immutable`

### SSE Stream State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-message-streaming-sse-stream`

**States**: opened, awaiting_content, streaming_content, terminal_sent, closed

**Initial State**: opened (HTTP 200 with `text/event-stream`, only after the reserve transaction committed or for a replay)

**Transitions**:
1. [x] - `p1` - **FROM** opened **TO** awaiting_content **WHEN** `stream_started` was emitted - `inst-sse-to-awaiting`
2. [x] - `p1` - **FROM** awaiting_content **TO** awaiting_content **WHEN** `sse_ping_interval_seconds` of idle time passed and `ping` was emitted - `inst-sse-ping`
3. [x] - `p1` - **FROM** awaiting_content **TO** streaming_content **WHEN** the first `delta` or `tool` event was emitted; no `ping` follows - `inst-sse-to-streaming`
4. [x] - `p1` - **FROM** streaming_content **TO** streaming_content **WHEN** a `delta`, `tool` or the single `citations` event was emitted - `inst-sse-content`
5. [x] - `p1` - **FROM** awaiting_content **TO** terminal_sent **WHEN** `done` or `error` was emitted before any content (including the replay, which goes through one `delta` first, and `stream_interrupted`) - `inst-sse-terminal-early`
6. [x] - `p1` - **FROM** streaming_content **TO** terminal_sent **WHEN** exactly one `done` (after a committed `completed` turn) or `error` was emitted - `inst-sse-terminal`
7. [x] - `p1` - **FROM** terminal_sent **TO** closed **WHEN** the server closed the connection right after the terminal event - `inst-sse-close`
8. [x] - `p1` - **FROM** awaiting_content **TO** closed **WHEN** the client disconnected; nothing is written and the turn is cancelled (the same holds from streaming_content) - `inst-sse-client-gone`

## 5. Definitions of Done

### Messages List API

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-messages-api`

The system **MUST** serve `GET /v1/chats/{id}/messages` with OData filtering over `created_at`, `id` and `role`, ordering over the same fields, a default chronological order with `id` tie-break, cursor pagination with `limit` default 20 and clamp 100, and items whose `id`, `request_id`, `role`, `content`, `attachments` (always an array) and `my_reaction` (always present, possibly `null`) are always present, with `model`, `input_tokens` and `output_tokens` omitted when absent or 0.

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-list-messages`
- `cpt-cf-mini-chat-algo-message-streaming-message-list-query`
- `cpt-cf-mini-chat-algo-message-streaming-message-projection`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `GET /v1/chats/{id}/messages`
- DB: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-message-reactions`
- DB Table: `cpt-cf-mini-chat-dbtable-messages`
- Entities: `Message`

### Message Order and Count Across Turns

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-message-order`

The system **MUST** keep messages in a strict chronological order `(created_at, id)` across turns (assistant after its user message, each turn after the previous one) on both PostgreSQL and SQLite, and **MUST** change `message_count` only by the messages a turn durably wrote.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-message-order`

**Constraints**: `cpt-cf-mini-chat-principle-linear-conversation`

**Touches**:
- API: `GET /v1/chats/{id}/messages`, `GET /v1/chats/{id}`
- DB: `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-messages`
- Entities: `Message`, `Chat`

### Send Endpoint, Validation and Preflight

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-send-preflight`

The system **MUST** expose `POST /v1/chats/{id}/messages:stream` with the body `content`, optional `request_id`, `attachment_ids` and `web_search`, **MUST** run validation, admission (`send_message` only), the turn admission checks, model resolution, the image count limit, the quota preflight, the input limit, the image guards, context and tool assembly and provider resolution before the reserve, and **MUST** answer every failure before the stream opens as a JSON Problem with no SSE stream, no user message, no turn, no reserve and no provider call.

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-send-message`
- `cpt-cf-mini-chat-algo-message-streaming-request-validation`
- `cpt-cf-mini-chat-algo-message-streaming-send-preflight`
- `cpt-cf-mini-chat-algo-message-streaming-image-guards`
- `cpt-cf-mini-chat-algo-message-streaming-send-error-contract`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-model-image-capability`, `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`, `Message`

### Reserve Transaction and Attachment Validation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-reserve-transaction`

The system **MUST** write, in one transaction and in this order, the quota reserve with its re-check, the user message with the `chats.updated_at` bump, the validated `message_attachments` links (same tenant, uploader, chat, `ready`, not deleted, unique) and the `running` turn with `started_at` equal to the preflight instant, **MUST** map every key or running-index race to the specified 409, and **MUST** leave nothing behind when any part fails.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-reserve-transaction`
- `cpt-cf-mini-chat-algo-message-streaming-attachment-validation`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-quota-usage` (through the reserve write contract)
- DB Table: `cpt-cf-mini-chat-dbtable-message-attachments`
- Entities: `Turn`, `Message`

### Context Assembly Within Budget

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-context-assembly`

The system **MUST** assemble the system prompt with tool guards, the thread summary, the recent messages within the snapshot boundary and the current message with its images in the normative order, **MUST** enforce the budget for both the input message (`INPUT_TOO_LONG`) and the assembled request (`CONTEXT_BUDGET_EXCEEDED`, then summary drop, then oldest whole turns first), and **MUST** produce the same plan for the same inputs.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-context-budget`
- `cpt-cf-mini-chat-algo-message-streaming-context-assembly`

**Constraints**: `cpt-cf-mini-chat-constraint-context-budget`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-thread-summaries`
- DB Table: `cpt-cf-mini-chat-dbtable-thread-summaries`
- Entities: `ContextPlan`

### Tool Assembly, Tool Events and Web Search

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-tools-web-search`

The system **MUST** assemble the tool set adapter-agnostically from `tool_support`, the kill switches, the request and the registered contributions, **MUST** include `web_search` with its guard only when requested and supported, **MUST** report tool activity as `tool` events, count completed calls durably, enforce the per-message web search and code interpreter limits mid-turn with the failed outcome and estimated settlement, and **MUST** report web citations without provider identifiers.

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-web-search-turn`
- `cpt-cf-mini-chat-algo-message-streaming-tool-assembly`
- `cpt-cf-mini-chat-algo-message-streaming-tool-accounting`
- `cpt-cf-mini-chat-algo-message-streaming-citations`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns` (`web_search_enabled`, completed counters, `last_progress_at`)
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`, tool set

### Provider Adapters Through OAGW

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-provider-adapters`

The system **MUST** call providers only through `ServiceGatewayClientV1` `proxy_request` to `/{upstream_alias}{api_path}` under the gear's S2S security context, **MUST** implement the `openai_responses` adapter fully (request shape, Responses event translation, usage, annotations, errors) for `storage_kind` `openai` and `azure`, and **MUST** implement the `openai_chat_completions`, `vllm_responses` and `anthropic_messages` adapters with exactly the per-kind request fields, dropped tools and translations of the adapter kinds algorithm.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-provider-call`
- `cpt-cf-mini-chat-algo-message-streaming-responses-request`
- `cpt-cf-mini-chat-algo-message-streaming-adapter-kinds`
- `cpt-cf-mini-chat-algo-message-streaming-event-translation`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`, `cpt-cf-mini-chat-constraint-openai-compatible`

**Touches**:
- API: OAGW proxy `POST /{upstream_alias}{api_path}`
- DB: none
- Entities: provider request, internal stream events

### SSE Event Contract and Relay

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-sse-contract`

The system **MUST** emit the seven events `stream_started`, `ping`, `delta`, `tool`, `citations`, `done` and `error` with the specified payloads in the order `stream_started ping* (delta | tool)* citations? (done | error)`, **MUST** relay each event without buffering through a bounded channel, **MUST** send `done` only after the committed `completed` finalization with usage and the quota and downgrade outcome but no internal identifier, and **MUST** end every stream with exactly one terminal event followed by the close of the connection.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-sse-relay`
- `cpt-cf-mini-chat-algo-message-streaming-terminal-emission`
- `cpt-cf-mini-chat-state-message-streaming-sse-stream`

**Constraints**: `cpt-cf-mini-chat-constraint-no-buffering`, `cpt-cf-mini-chat-constraint-bounded-channels`, `cpt-cf-mini-chat-principle-streaming-first`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream` (SSE)
- DB: none
- Entities: stream events

### Turn Finalization Transaction

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-finalization`

The system **MUST** finalize every stream terminal in one transaction that persists the assistant (or partial) message, wins the CAS on `running`, settles quota and enqueues the usage event through `QuotaService::finalize_turn`, enqueues the turn audit event and, for a completed turn whose trigger fires, the thread-summary work; a lost CAS **MUST** be a complete no-op and a failed message persistence **MUST** downgrade the turn to `failed` with `message_persistence_failed`.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-finalization`
- `cpt-cf-mini-chat-algo-message-streaming-turn-audit-event`
- `cpt-cf-mini-chat-algo-message-streaming-summary-trigger`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-quota-usage`, `cpt-cf-mini-chat-dbtable-thread-summaries` (read)
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`, `Message`, `UsageEvent`, `TurnAuditEvent`, thread-summary work item

### Turn Lifecycle and Content Cases

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-turn-lifecycle`

The system **MUST** keep every turn in the state machine `running` to `completed`, `failed` or `cancelled` with immutable terminal states, **MUST** guarantee that `completed` always has a committed assistant message (possibly empty), that a cancelled turn persists its non-empty partial text as a message with `assistant_message_id` and otherwise has none, and that a failed turn has no assistant message and `assistant_message_id` NULL.

**Implements**:
- `cpt-cf-mini-chat-state-message-streaming-turn-lifecycle`

**Constraints**: `cpt-cf-mini-chat-principle-linear-conversation`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`, `GET /v1/chats/{id}/turns/{request_id}`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Idempotency, Replay and Parallel-Turn Guard

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-idempotency`

The system **MUST** replay a completed, non-deleted `(chat_id, request_id)` as `stream_started` (`is_new_turn` `false`), one `delta` and `done` with no provider call, no reserve, no quota change and no outbox message, **MUST** reject every other reuse with 409 `request_id_conflict`, **MUST** check replay before the parallel-turn guard, **MUST** reject a new turn while another is `running` with 409 `turn_already_running`, and **MUST** accept a new turn as soon as the previous one is terminal.

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-resend-request-id`
- `cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks`
- `cpt-cf-mini-chat-algo-message-streaming-replay`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Cancellation on Client Disconnect

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-cancellation`

The system **MUST** create the turn's cancellation token before the send setup and cancel it when the response or connection is dropped, **MUST** send no provider request when the token is already cancelled after the reserve commit and finalize that turn as `cancelled` without an assistant message, **MUST** turn every other client disconnect (dropped response before the stream opened, dropped relay or failed channel send) into a hard cancel of the provider request, a `cancelled` finalization with estimated settlement and the `aborted` usage event, no SSE event on the broken stream, and **MUST** leave a turn whose terminal event was already emitted unchanged.

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-disconnect`
- `cpt-cf-mini-chat-algo-message-streaming-provider-call`

**Constraints**: `cpt-cf-mini-chat-constraint-bounded-channels`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Turn Status API

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-turn-status`

The system **MUST** serve `GET /v1/chats/{id}/turns/{request_id}` read-only, mapping `running`, `completed`, `failed` and `cancelled` to `running`, `done`, `error` and `cancelled`, with `error_code` only for `error`, `assistant_message_id` only for `done` and `cancelled` with partial content, and 404 for soft-deleted, unknown or foreign turns.

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-turn-status`
- `cpt-cf-mini-chat-algo-message-streaming-turn-status`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: `GET /v1/chats/{id}/turns/{request_id}`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Orphan Watchdog Recovery

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-orphan-watchdog`

The system **MUST** run the orphan watchdog only on the elected leader, scan at most 100 stale `running` turns per scan by `last_progress_at` (falling back to `started_at`), finalize each one only through the orphan CAS that re-checks `running`, `deleted_at IS NULL` and stale progress, settle it on the estimated path with the `aborted` usage event and the `turn_failed` audit event in the same transaction, and **MUST** skip every turn whose CAS affects 0 rows.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-quota-usage` (through the settlement contract)
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`, `UsageEvent`, `TurnAuditEvent`

### Error Mapping and Sanitization

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-error-sanitization`

The system **MUST** map provider failures to `provider_error`, `provider_timeout` and `rate_limited`, **MUST** sanitize every client-visible message, stored `error_detail` and REST `detail` by the replacement rules (response, completion, message, request, organization, project, tool-use, call, run, thread, file, assistant and vector store ids, request-id style tokens, URLs, `sk-` keys in plain, project and masked forms, `Bearer` tokens), **MUST** use the generic message of the code instead of provider text for a provider 401 or 403 and whenever an unrecognized id-like token remains, so no provider-issued identifier or credential reaches a client, and **MUST** map pre-stream failures to the canonical Problems of the send pipeline error contract, consistently with the foundation error mapping.

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-error-sanitization`
- `cpt-cf-mini-chat-algo-message-streaming-send-error-contract`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`, `GET /v1/chats/{id}/messages`, `GET /v1/chats/{id}/turns/{request_id}`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns` (`error_code`, `error_detail`)
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: stream error, Problem

### Streaming Observability and Configuration

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-observability`

The system **MUST** record the streaming, cancellation, orphan, finalization, image and thread-summary trigger series with low-cardinality labels and **MUST** read only the configuration keys and catalog fields listed in the streaming configuration algorithm.

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-admin-observability`
- `cpt-cf-mini-chat-algo-message-streaming-streaming-metrics`
- `cpt-cf-mini-chat-algo-message-streaming-configuration`

**Constraints**: `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: none (OTLP export)
- DB: none
- Entities: metric series

### Deferred and Not-Implemented Scope Stays Unimplemented

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-deferred-scope`

The system **MUST NOT** implement mid-stream quota abort, a DB-server clock for the watchdog, the cross-column CHECK constraints on `chat_turns`, an immutable replay `done` payload or replayed citations, persistence of citations or `downgrade_reason`, a `completion_signal` payload field, incremental partial-delta persistence, resume after a crash, attach to a running stream, cross-pod stream recovery, attachment-scoped retrieval, Anthropic web citations, a per-turn `file_search` limit, or billing of knowledge-search agentic iterations, and **MUST** keep them recorded as not implemented (ADR-0007, ADR-0008, ADR-0010).

**Implements**:
- `cpt-cf-mini-chat-algo-message-streaming-replay`
- `cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: none
- Entities: none

### Test Layering, Traceability and Feature-Local Deviations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-message-streaming-tests-and-deviations`

The system **MUST** cover every acceptance-criteria item that this feature owns with tests in the layers below, and **MUST** record each feature-local deviation with its rationale and review owner.

**Test layering**:
- Unit tests live in the gear crate (`gears/mini-chat/mini-chat/`) and cover pure logic: request validation (every Problem and default), the turn admission check order (each existing state, soft-deleted rows, replay while another turn runs), the replay event builder with and without a downgrade, the image guards against the downgrade decision matrix, the context budget formula (`max_input_tokens` 0, deductions reaching the limit, `max_output_tokens_applied` at least `context_window`), the truncation algorithm (mandatory overflow, summary drop, oldest-first drop, leading assistant drop, `messages_truncated`, determinism), the tool assembly (web search supported and unsupported, kill switch effects decided upstream, a stub contribution, ordering, `feature` values), the OpenAI Responses request shape (every field, the `input` item shapes with and without a summary and with and without current images, the `web_search` tool `type` literal, `include`, `user` of exactly 64 characters, `extra_body` reserved keys), each adapter's request body and wire translation against recorded fixtures (OpenAI Responses including `response.incomplete`, `response.failed` with usage, the bare `error` event and the missing `event:` line; Chat Completions; vLLM with `<think>` blocks; Anthropic with `code_execution`, web search and function tool blocks), the provider error mapping (429 with and without `Retry-After`, gateway 504, provider 504 with body), the sanitizer (each identifier family, the 12-character floor, `file-based`, `file_search` and `thread_summary` untouched, URLs, `sk-` keys including `sk-proj-…` and masked `sk-proj-****abcd` forms, `Bearer` tokens, UUID and long hexadecimal request ids, the generic fallback for 401, 403 and residual id-like tokens, and the fixtures OpenAI 500 `request ID req_abc123`, OpenAI 429 `organization org-XYZ`, OpenAI 401 `sk-proj-****abcd` and Anthropic `req_011CSHoEeqs5C35K2UUqR7Fy`, none of which survives in the client text), citation mapping (web span and snippet rules, out-of-range span, unknown and deleted file ids), the tool limit counters, the terminal emission matrix, the audit event fields on both paths, the summary trigger conditions and frozen target, and the turn status projection.
- Repository tests with SQLite cover the message page (default order, every filter and order, keyset cursors forward and backward, messages with identical `created_at` and with fractional seconds of different lengths such as `.12` and `.123`, the NULL `request_id` guard, the attachment and reaction batches including deleted attachments and the thumbnail rule), the snapshot boundary and recent messages query (summary frontier, `is_compressed`, limit 0 and 100), the reserve transaction (commit with all rows, rollback on the re-check, on an invalid, foreign, not-ready, deleted or other-uploader attachment, on the `(chat_id, request_id)` race and on the running-index race, with no rows left), the finalization transaction (completed with the message and `assistant_message_id`, the forced message insert failure downgraded to `message_persistence_failed`, cancelled with and without partial text and with a forced partial insert failure, failed without message, CAS loss leaving no assistant message, a forced enqueue failure leaving the turn `running`, exactly one usage, one audit and at most one summary message), the progress and counter updates guarded by `running`, and the orphan scan and CAS (stale and fresh `last_progress_at`, NULL fallback to `started_at`, soft-deleted turns skipped, progress refreshed after discovery, the 100-candidate cap, turns without reserve columns).
- In-process REST and SSE tests drive the router with stubbed platform clients (PDP, types-registry, authn resolver, model-policy and audit plugins) over SQLite and a mock OAGW `ServiceGatewayClientV1` that records each proxied request (URI, method, security context, body) and streams OpenAI-Responses-format SSE scripted per test (text deltas, tool events, annotations, `response.completed`, `response.incomplete`, `response.failed`, an `error` event, HTTP 429 with `Retry-After`, HTTP 500 with provider identifiers in the body (including `req_abc123`), HTTP 429 naming `org-XYZ`, HTTP 401 carrying `sk-proj-****abcd`, a gateway timeout, a premature end, a hang for cancellation and slow delivery for ping and backpressure); no network is used. They cover the full send stream and its persisted rows, every pre-stream Problem with no rows written and no proxied request, the event ordering and payloads, ping before content only, replay, the 409 cases and the new turn after a terminal one, disconnect during setup after the reserve commit and before the provider call (no proxied request, turn `cancelled` with an estimated settlement, an `aborted` usage event and no assistant message, a new `request_id` accepted at once), disconnect after the provider request started and before the stream opened (the proxied request is cancelled, turn `cancelled`), disconnect during the stream, the tool limits, citations, the turn status API, the messages list after several turns and `message_count`, and the orphan watchdog run with a controllable clock.
- No tests are added under `testing/e2e/suites/mini_chat/`.
- Every implemented CDSL step carries `@cpt-begin` and `@cpt-end` markers with its ID, phase token and `inst-*` ID (traceability mode FULL).

**Reliability and data integrity**: the reserve transaction and the finalization transaction are each atomic; the first committed terminal transition is authoritative; `completed` implies a committed assistant message; every reserved turn ends with exactly one settlement and one usage event through the stream finalization or the orphan watchdog. **Security**: admission, tenant and owner scoping and 404 masking come from the foundation; provider calls run under the gear's S2S context with user identity only in request metadata; no provider identifier or credential reaches a client, a metric label or the audit content; message content is never logged. **Observability**: the series of the streaming metrics algorithm, the turn audit event and the usage event; causes are logged, never content. **Rollback**: a failed transaction leaves no partial state; schema rollback is owned by the foundation migrations. **Compile-time gates**: those of the foundation apply (workspace lints, the domain layering lint that keeps `domain/` free of `crate::infra`, `sea_orm`, `axum` and `http`, the domain-model lint on the new domain types, the example server build with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`).

**Feature-local deviations** (review owner for every item: the mini-chat gear owners who maintain `gears/mini-chat/docs/`):

| Deviation | Rationale | Source |
|-----------|-----------|--------|
| `ping` is sent only before the first content event; afterwards the HTTP server sends a 30 s SSE comment keep-alive | Accepted P1 behavior | ADR-0010 |
| A CAS loser whose client is still connected receives `error{code: "stream_interrupted"}` instead of no terminal event | Matches the `failed` state committed by the watchdog | ADR-0010 |
| Replay rebuilds `quota_decision` and `downgrade_from`, omits `downgrade_reason`, `quota_warnings` and citations | Neither is persisted | ADR-0010 |
| The orphan watchdog has its own finalization path with its own CAS and reports `selected_model` equal to the effective model and the audit quota decision `unknown`; the audit event payload type must be able to carry `unknown` | The selected model is not stored on the turn | DESIGN 2.2, 5.7, ADR-0010 |
| The watchdog uses the application clock and a gear-local Lease (no-op without `k8s`); the cross-column CHECKs on `chat_turns` are kept by the repositories | Accepted limitations | ADR-0010 |
| The orphan watchdog is specified and owned here although the foundation's gear start names `cpt-cf-mini-chat-feature-background-maintenance` for its behavior | DECOMPOSITION 2.4 assigns `cpt-cf-mini-chat-component-orphan-watchdog` to this feature; the foundation only spawns it | DECOMPOSITION 2.4 |
| A stream request whose chat model is missing from the catalog gets 400 `INVALID_MODEL` before the quota preflight, so the cascade's "missing selected model" branch is never reached from this route | DESIGN 3.3 states the 400 for `messages:stream` | DESIGN 3.3 |
| `INPUT_TOO_LONG` is reported on field `content` and `CONTEXT_BUDGET_EXCEEDED` on field `context` | The DESIGN fixes the reasons, not the field names | DESIGN 3.3 |
| Body validation (`EMPTY_CONTENT`, duplicate or too many `attachment_ids`) runs before the PEP call and the chat load | Same order as the title of create; validation stays independent of chat existence | DESIGN 3.3 |
| The OpenAI Responses request sets `store` to `false` and never sends `previous_response_id` or `tool_choice` | The gear sends the full context on every request; the DESIGN lists these keys as request-controlled only | DESIGN 4 `Model Catalog Configuration` |
| The Chat Completions adapter requests the usage chunk through `stream_options` and caps output with `max_completion_tokens`; the Anthropic adapter maps its web search server tool blocks to `tool` events named `web_search` | The DESIGN lists these keys as request-controlled and maps only `code_execution` explicitly; the mapping lets the shared limits and counters apply | DESIGN 3.2, 3.3 |
| The breaching tool `start` event is not relayed; the counts settled are the completed (`done`) counts | The DESIGN counts starts for the limit and completions for accounting | DESIGN 4 |
| The audit event fills `code_interpreter_calls`, which the SDK audit type carries although the DESIGN says the audit type has no code interpreter count; `latency_ms` runs from `started_at` to finalization | The SDK type is the wire contract; the count carries no content | DESIGN 4, ADR-0009 |
| The assistant message `created_at` is forced strictly after the user message by one microsecond when the clock has not advanced | Guarantees the chronological order with a random `id` tie-break | DESIGN 3.7, implementation note 1 |
| The thread-summary work item is enqueued here while its handler stays a placeholder until `cpt-cf-mini-chat-feature-background-maintenance` wires it | The outbox keeps the message until the handler exists | DECOMPOSITION 2.7, implementation note 7 |
| No path reaches the `released` settlement of a turn that took a reserve but never reached the provider | Every fallible pre-provider step runs before the reserve; a client disconnect between the reserve commit and the provider call ends the turn `cancelled`, which the outcome rules settle `estimated` | DESIGN 3.3 `SSE stream close rules`, 5.7, 5.8 |
| Where `config/mini-chat.yaml` and the DESIGN differ, the DESIGN governs | `gears/mini-chat/docs/` is the source of truth | DECOMPOSITION 1 |

**Implements**:
- `cpt-cf-mini-chat-flow-message-streaming-send-message`
- `cpt-cf-mini-chat-flow-message-streaming-list-messages`
- `cpt-cf-mini-chat-flow-message-streaming-resend-request-id`
- `cpt-cf-mini-chat-flow-message-streaming-disconnect`
- `cpt-cf-mini-chat-flow-message-streaming-turn-status`
- `cpt-cf-mini-chat-flow-message-streaming-web-search-turn`
- `cpt-cf-mini-chat-flow-message-streaming-admin-observability`

**Constraints**: `cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-constraint-no-buffering`, `cpt-cf-mini-chat-constraint-bounded-channels`, `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- API: every route of this feature
- DB: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-message-attachments`
- Entities: `Message`, `Turn`, `ContextPlan`

## 6. Acceptance Criteria

The first twenty-two items are the DECOMPOSITION 2.4 "Acceptance Criteria Covered" and the matching items of `acceptance-criteria.md`; each is both implemented and tested. The remaining items pin the details behind them.

- [ ] Context window budget is enforced for both the input message and the full assembled request: a message whose `estimated_text_tokens` exceeds a non-zero `max_input_tokens` of the effective model gets 400 `out_of_range` `INPUT_TOO_LONG`; mandatory items above `token_budget` (or `max_output_tokens_applied` at least `context_window`, or deductions reaching the input limit) get 400 `out_of_range` `CONTEXT_BUDGET_EXCEEDED`; both are JSON Problems with no stream, no rows and no proxied request, and the budget uses the effective model after a downgrade.
- [ ] Streaming responses are never buffered before relaying: with a mock provider that delivers a delta and then pauses, the client receives that `delta` before the provider sends the next event; events pass through a channel of `streaming.sse_channel_capacity`; the response has `Content-Type: text/event-stream`, `Cache-Control: no-cache` and no compression.
- [ ] Messages are listed with filtering, ordering and pagination: `$filter` over `created_at`, `id` and `role` with the allowed operators, `$orderby` over the six values, the default `created_at asc` with `id` tie-break, `limit` default 20 and clamp 100, `0` rejected with `INVALID_LIMIT` (field `$top`), cursor pages without gaps or repeats, and every malformed input rejected with 400 and `context.resource_type` `gts.cf.core.odata.query.v1~`.
- [ ] The message response contract is consistent: every item has `id`, `request_id`, `role`, `content`, `created_at`, an `attachments` array (empty when none; only non-deleted attachments; `img_thumbnail` only for ready images) and `my_reaction` (`like`, `dislike` or `null`, always `null` for user messages); `model`, `input_tokens` and `output_tokens` are omitted when absent or 0; a stored NULL `request_id` gives 500 instead of a message without it; no internal or provider field appears.
- [ ] Message count and chronological ordering are correct across turns: after three completed turns the list returns user and assistant messages alternating in send order on SQLite (also with equal or differently precise timestamps), the user and assistant messages of a turn share its `request_id`, and `message_count` is 6; a failed turn adds 1, a cancelled turn adds 1 or 2, a rejected send and a replay add 0.
- [ ] The send-message endpoint streams a response correlated by a request id: `stream_started` carries the client `request_id` (or a server UUID v4 when omitted) and the pre-allocated `message_id`, which becomes the persisted assistant message id and the turn status `assistant_message_id`.
- [ ] Preflight validation runs before any provider call: empty content, duplicate, foreign, not-ready, deleted or too many `attachment_ids`, too many images (`TOO_MANY_IMAGES` above `rag.max_images_per_message`), `disable_images` with images (400 `FEATURE_DISABLED` subject `images`), an effective model without `VISION_INPUT` with images (400 `VISION_NOT_SUPPORTED`, also after a downgrade from a vision model), a missing chat model (400 `INVALID_MODEL`), quota rejections and provider resolution failures each return a JSON Problem with no SSE stream, no proxied request and no user message, turn or reserve left.
- [ ] The assistant message and usage are persisted once a stream completes: after `done` the assistant row has the streamed text, `model` equal to the effective model, the provider token counts and `provider_response_id`; the turn is `completed` with `assistant_message_id` and NULL `error_code`; the quota buckets are settled and one usage event is in `mini-chat.usage_snapshot`; a provider `incomplete` is persisted and settled the same way.
- [ ] The full streaming event contract and its ordering hold: a scripted stream yields `stream_started`, `ping` only before the first `delta` or `tool` (at `sse_ping_interval_seconds`), `delta` with `type` `text`, `tool` `start` and `done` for `file_search`, `web_search` and `code_interpreter` (with `files_searched` and the capped `output`), at most one `citations` after the last `delta`, and exactly one terminal `done` or `error`, after which the connection closes with no further event.
- [ ] The completion event exposes usage and the quota and downgrade outcome without internal identifiers: `done` has `usage.input_tokens`, `usage.output_tokens`, `effective_model`, `selected_model`, `quota_decision`, and only for a downgrade `downgrade_from` (equal to `selected_model`) and `downgrade_reason`, plus `quota_warnings` on the CAS-winning completed turn; it contains no `request_id`, `message_id`, `turn_id`, provider response id, reserve or billing field.
- [ ] The error event is terminal and sanitized: a provider failure whose body contains `resp_…`, `file-…`, `vs_…`, a URL, an `sk-` key and a `Bearer` token yields one `error{code: "provider_error", message}` with `[provider_id]`, `[url]` and `[credential]` substituted, nothing follows it, the turn is `failed` with that code, and `error_detail` holds only the sanitized text.
- [ ] Replaying a known request id returns the stored result without side effects: a resend of a completed turn's `request_id` returns `stream_started` (`is_new_turn` `false`, persisted `message_id`), one `delta` with the full text and `done`, with no proxied request, no change in any `quota_usage` row, no new outbox message and no new row.
- [ ] Conflicting reuse of a request id is rejected consistently: reuse for a `running`, `failed` or `cancelled` turn, or for a soft-deleted turn of any state, returns 409 `aborted` with `context.reason` `request_id_conflict` and a generic `detail`; two concurrent sends with the same new `request_id` yield one stream and one `request_id_conflict`.
- [ ] Replay is checked before the parallel-turn guard: while another turn of the chat is `running`, a resend of a completed turn's `request_id` is replayed, not rejected with `turn_already_running`.
- [ ] Only one turn runs per chat: a second send with a different or no `request_id` while a turn is `running` returns 409 `aborted` `turn_already_running`, and two concurrent sends to an idle chat yield one stream and one `turn_already_running` through the running-turn unique index.
- [ ] A new turn is accepted once the previous one is terminal: after `completed`, `failed`, `cancelled` and orphan `failed`, a send with a new `request_id` opens a new stream.
- [ ] The turn state machine is consistent end to end: every turn moves from `running` to exactly one of `completed`, `failed` or `cancelled`, a second finalizer (provider terminal against disconnect, stream against watchdog) changes nothing, and the turn status API, the SSE terminal event and the database always agree.
- [ ] Partial and null-content cases are handled: a cancel after deltas persists the partial text as an assistant message and sets `assistant_message_id` (turn status `cancelled` with the id); a cancel before any delta persists nothing; a failed partial insert still ends `cancelled` with no id; a provider failure after deltas persists no assistant message; a completion without deltas persists an empty assistant message and stays replayable.
- [ ] System prompt, thread summary and recent history are assembled and truncated deterministically within budget: the provider request `instructions` field holds the system prompt followed by the tool guards, and `input` holds the summary as a `user` message whose plain-string content starts with the B.5.5 preamble, then at most `context.recent_messages_limit` non-compressed messages after the frontier and before the snapshot boundary with their `user` or `assistant` roles, and the current user message last (plain-string content without images, an `input_text` item followed by `input_image` items with images); no system item is in `input`; under budget pressure the summary is dropped when it does not fit, older whole turns are dropped first and no answer is sent without its question; the same inputs produce the same request.
- [ ] Tool availability and guidance are reflected in the assembled request: with web search requested on a supporting model the request `tools` array has an entry with `type` `web_search` and `search_context_size` equal to the catalog `web_search_context_size`, and `instructions` end with `context.web_search_guard`; on a non-supporting model neither is present and `web_search_enabled` is still stored; a registered contribution appears with its guard in the specified order; `metadata.feature` names the built-in tools; the Chat Completions and vLLM adapters drop the tools they cannot send while the guard stays.
- [ ] Provider-originated error details are sanitized before reaching the client: SSE `error` messages, REST `detail` values, `chat_turns.error_detail` and the turn status never contain provider response, completion, message, request, organization, project, tool-use, call, run, thread, file, assistant or vector store identifiers, request-id style tokens, URLs or credentials, while words such as `file-based`, `file_search` and `thread_summary` are kept; with the fixtures OpenAI 500 `request ID req_abc123`, OpenAI 429 `organization org-XYZ` and Anthropic `req_011CSHoEeqs5C35K2UUqR7Fy` the client text holds `[provider_id]` in place of the id, and a provider 401 carrying `sk-proj-****abcd` yields the generic message of its code with no provider text.
- [ ] Web search tool use is reported, cited, accounted and quota-limited: `tool` events named `web_search` are relayed, web citations carry `url`, `title`, `snippet` and `span` per the rules, the completed count lands in `chat_turns.web_search_completed_count` and in the daily `web_search_calls` counter at settlement, the third `web_search` start with the default limit 2 ends the turn `failed` with `web_search_calls_exceeded` (SSE `error`, estimated settlement, no refund), and the daily quota and kill switch are rejected before the stream through the quota contract (429 scope `web_search`, 400 `FEATURE_DISABLED`).
- [ ] The provider request is sent through OAGW correctly: the mock `ServiceGatewayClientV1` receives `POST /{upstream_alias}{api_path}` (with `{model}` replaced and a `host:port` alias kept) under the gear's S2S security context, and the body has `model`, `instructions`, `input`, `stream` `true`, `max_output_tokens` equal to `max_output_tokens_applied`, `max_tool_calls`, `tools`, `include` only with `code_interpreter`, `user` of 64 hexadecimal characters, `metadata` with `tenant_id`, `user_id`, `chat_id`, `request_type` `chat` and `feature`, `store` `false`, and no reserved key overridden by `extra_body`.
- [ ] The provider errors map as specified: HTTP 429 gives `rate_limited` (with the seconds of a numeric `Retry-After` in the message), a gateway timeout or OAGW 504 `deadline_exceeded` gives `provider_timeout`, a provider 504 with a JSON body, any other failure, `response.failed` and an `error` event give `provider_error`; a `response.failed` with non-zero usage settles `actual`, without usage `estimated`.
- [ ] The finalization transaction is composed atomically: a completed turn commits the assistant message, the CAS, the settlement, exactly one usage event, exactly one `turn_completed` audit event and, when the trigger fires, exactly one thread-summary message with the frozen range and a stable `system_request_id`; a forced enqueue failure leaves the turn `running` with no message and the client receives `error{code: "finalization_failed"}`; a forced assistant insert failure yields `failed` with `message_persistence_failed` and the matching SSE error.
- [ ] The thread-summary trigger follows its rules: with no summary and `assembled_context_tokens` at least 80 percent of `effective_budget`, or with truncated history, one work item is enqueued; with a summary and no truncation nothing is enqueued; the causing turn's messages are never in the frozen range; `mini_chat_thread_summary_trigger` records `scheduled` or `not_needed` after the commit; failed and cancelled turns evaluate no trigger.
- [ ] Client disconnect cancels the turn: the mock provider stream is dropped within the cancellation path, the turn ends `cancelled` with an estimated settlement and an `aborted` usage event, the `turn_failed` audit event is enqueued, no SSE event is written after the disconnect, and a disconnect after `done` changes nothing; a disconnect during setup still yields a `cancelled` turn instead of a turn left `running`: when the connection drops after the reserve commit and before the provider call, the mock `ServiceGatewayClientV1` records no proxied request, the turn is `cancelled` with an estimated settlement, an `aborted` usage event and no assistant message, and a send with a new `request_id` is accepted immediately; when it drops after the provider request started and before the stream opened, the proxied request is cancelled and the turn ends `cancelled`.
- [ ] The turn status API is authoritative: it returns `running`, `done`, `error` (with `error_code`) or `cancelled` with `request_id` and `updated_at`, `assistant_message_id` for `done` and partial `cancelled`, no `chat_id`, no billing data, and 404 with the turn type for soft-deleted, unknown and other users' turns.
- [ ] The orphan watchdog recovers stuck turns: on the leader, a `running` turn whose `last_progress_at` (or NULL with an old `started_at`) is older than `orphan_watchdog.timeout_secs` becomes `failed` with `orphan_timeout`, settled `estimated` with an `aborted` usage event and a `turn_failed` audit event with `selected_model` equal to the effective model; a turn with recent progress, a soft-deleted turn or a turn finalized in between is skipped with no settlement or metric; a scan handles at most 100 turns; a non-leader scans nothing; the chat accepts a new turn afterwards.
- [ ] The streaming metrics are recorded as specified: stream started, completed, incomplete, failed, disconnected, active streams, the three latency histograms, cancel requested and effective, time to abort, streams aborted, orphan detected and finalized, scan duration, finalization latency and image inputs appear with low-cardinality labels and no identifiers, and the declared-but-unrecorded streaming series stay unrecorded.
- [ ] The deferred scope stays unimplemented: no mid-stream quota abort, no replayed citations or `downgrade_reason`, no `completion_signal`, no partial-delta persistence or resume, no attach to a running stream, no attachment-scoped retrieval and no Anthropic web citations.
- [ ] Unit tests live in the gear crate, repository and REST/SSE tests run in-process over SQLite with a mock OAGW `ServiceGatewayClientV1` and no network, no test is added under `testing/e2e/suites/mini_chat/`, and every implemented CDSL step has `@cpt-begin` and `@cpt-end` markers.
