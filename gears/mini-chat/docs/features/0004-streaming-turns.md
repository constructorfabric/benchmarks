---
description: "Streaming turns feature of the mini-chat gear: the send-message SSE endpoint with its event contract, request validation, idempotent replay and the one-running-turn guard, context assembly and truncation, the tool-enablement decision, the four provider adapters through OAGW with event translation, cancellation on disconnect, turn finalization with message persistence and audit, error sanitization, request metrics and the turn status API."
---

# Feature: Streaming Turns

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-streaming-turns-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-streaming-turns`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Send Message](#send-message)
  - [Get Turn Status](#get-turn-status)
  - [Client Disconnect](#client-disconnect)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Send Request Validation](#send-request-validation)
  - [Idempotency and Parallel-Turn Guard](#idempotency-and-parallel-turn-guard)
  - [Completed Turn Replay](#completed-turn-replay)
  - [Turn Input Loading](#turn-input-loading)
  - [Tool Enablement Decision](#tool-enablement-decision)
  - [Context Assembly and Truncation](#context-assembly-and-truncation)
  - [Reserve Transaction and Turn Creation](#reserve-transaction-and-turn-creation)
  - [Provider Request Construction](#provider-request-construction)
  - [Provider Stream Translation](#provider-stream-translation)
  - [Provider Error Classification](#provider-error-classification)
  - [Error Message Sanitization](#error-message-sanitization)
  - [Citation Mapping](#citation-mapping)
  - [Provider Task](#provider-task)
  - [Knowledge Search Iteration](#knowledge-search-iteration)
  - [Stream Finalization](#stream-finalization)
  - [Turn Audit Event](#turn-audit-event)
  - [SSE Relay](#sse-relay)
  - [Stream Metrics Recording](#stream-metrics-recording)
- [4. States (CDSL)](#4-states-cdsl)
  - [Turn Lifecycle State Machine](#turn-lifecycle-state-machine)
  - [SSE Stream State Machine](#sse-stream-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Send Endpoint and Request Validation](#send-endpoint-and-request-validation)
  - [Idempotency, Replay and Parallel-Turn Guard](#idempotency-replay-and-parallel-turn-guard)
  - [Turn Creation in the Reserve Transaction](#turn-creation-in-the-reserve-transaction)
  - [Context Assembly and Token Budget](#context-assembly-and-token-budget)
  - [Tool Enablement and Guidance](#tool-enablement-and-guidance)
  - [OpenAI Responses Adapter](#openai-responses-adapter)
  - [Other Provider Adapters](#other-provider-adapters)
  - [SSE Event Contract and Relay](#sse-event-contract-and-relay)
  - [Cancellation on Disconnect](#cancellation-on-disconnect)
  - [Turn Finalization and Persistence](#turn-finalization-and-persistence)
  - [Turn Status API](#turn-status-api)
  - [Error Mapping and Sanitization](#error-mapping-and-sanitization)
  - [Turn Audit Events and Request Metrics](#turn-audit-events-and-request-metrics)
  - [MCP Never Injected](#mcp-never-injected)
  - [REST Registration and OpenAPI](#rest-registration-and-openapi)
  - [Cross-Cutting Quality and Verification](#cross-cutting-quality-and-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

This feature delivers the core chat turn of the `mini-chat` gear (DESIGN §3.3 "Streaming Contract", §3.6 `cpt-cf-mini-chat-seq-send-message` and `cpt-cf-mini-chat-seq-cancellation`, §4 "Context Plan Assembly and Truncation" and "Turn Lifecycle, Crash Recovery and Orphan Handling"). It owns `POST /v1/chats/{id}/messages:stream`: request validation, idempotency and side-effect-free replay by `request_id`, the one-running-turn-per-chat guard, the chat attachment state and the `attachment_ids` checks, the image guards, the `INPUT_TOO_LONG` check, the tool-enablement decision, context assembly with deterministic truncation, provider resolution, and the reserve transaction that persists the user message and the `running` turn. It then owns the stream itself: the provider request of the four adapter kinds through the OAGW proxy, the translation of provider events into the stable SSE contract (`stream_started`, `delta`, `tool`, `citations`, `done`, `error`, `ping`) over bounded channels without buffering, per-turn tool limits, citations, the knowledge-search loop, cancellation on client disconnect, and the turn state machine with its terminal decision. Finalization persists the assistant message and enqueues the turn audit event in the transaction of the CAS-guarded quota finalization, and sends `done` only after that commit. The feature also owns SSE error sanitization, the request group of the cost metrics, and `GET /v1/chats/{id}/turns/{request_id}`.

Quota arithmetic is not redefined here. The preflight, the reserve write with its re-check, the CAS-guarded finalization with settlement and the usage event, the per-turn tool limit decision and the quota warnings come from `cpt-cf-mini-chat-feature-quota-billing` and are called by reference.

**Implementation slices** (implement in this order; each slice is independently testable):

| # | Slice | CDSL elements |
|---|-------|---------------|
| 1 | Request validation, idempotency and replay | `cpt-cf-mini-chat-algo-streaming-turns-validate-request`, `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`, `cpt-cf-mini-chat-algo-streaming-turns-replay-completed` |
| 2 | Turn inputs, tools and context | `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`, `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`, `cpt-cf-mini-chat-algo-streaming-turns-assemble-context` |
| 3 | Reserve transaction and turn creation | `cpt-cf-mini-chat-algo-streaming-turns-create-turn`, `cpt-cf-mini-chat-flow-streaming-turns-send-message` (setup steps up to the reserve commit) |
| 4 | OpenAI Responses adapter and translation | `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request` (`openai_responses`), `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream` (`openai_responses`), `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`, `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`, `cpt-cf-mini-chat-algo-streaming-turns-map-citations` |
| 5 | Provider task, relay and finalization | `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`, `cpt-cf-mini-chat-algo-streaming-turns-relay-sse`, `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`, `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`, `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle`, `cpt-cf-mini-chat-state-streaming-turns-sse-stream` |
| 6 | Cancellation, turn status and metrics | `cpt-cf-mini-chat-flow-streaming-turns-client-disconnect`, `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`, `cpt-cf-mini-chat-algo-streaming-turns-record-metrics` |
| 7 | Other adapters and knowledge search | `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request` and `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream` (`openai_chat_completions`, `vllm_responses`, `anthropic_messages`), `cpt-cf-mini-chat-algo-streaming-turns-knowledge-search` |

**Out of scope** (owned by other features): quota arithmetic, the preflight cascade, the reserve write and re-check, settlement, the usage event, the tool limit values and the quota view (`cpt-cf-mini-chat-feature-quota-billing`, reused by reference); attachment upload, indexing, the vector store lifecycle, thumbnails and the data the citation map is built from (`cpt-cf-mini-chat-feature-attachments`; this feature reads `ready` attachment rows seeded by that feature or directly in tests); retry, edit and delete of turns, which reuse this pipeline (`cpt-cf-mini-chat-feature-turn-mutations`); the orphan watchdog, the thread-summary trigger step in the finalization transaction and the summary handler (`cpt-cf-mini-chat-feature-background-workers`; this feature exposes `assembled_context_tokens`, `messages_truncated` and the frontier inputs to that step and enqueues nothing for it). MCP tool discovery and execution are Future (`cpt-cf-mini-chat-adr-mcp-deferred`). The request gate, PEP, chat-scoped access, error mapping, provider resolution, OAGW provisioning, outbox queues, event encoding and metric registration come from `cpt-cf-mini-chat-feature-gear-foundation`.

### 1.2 Purpose

The send-message turn is the product: a user writes a message and sees the answer appear token by token, with tool activity and sources, within a turn that is billed exactly once and can be recovered after a lost connection. The pipeline must decide every refusal before it opens a stream, so that clients get a canonical JSON error instead of a half-open stream. It must never buffer the provider stream, stop the provider when the user leaves, and keep the database the single authoritative record of how each turn ended.

**Requirements**: `cpt-cf-mini-chat-fr-chat-streaming`, `cpt-cf-mini-chat-fr-conversation-history`, `cpt-cf-mini-chat-fr-streaming-cancellation`, `cpt-cf-mini-chat-fr-token-budget`, `cpt-cf-mini-chat-fr-web-search`, `cpt-cf-mini-chat-fr-code-interpreter`, `cpt-cf-mini-chat-fr-file-search`, `cpt-cf-mini-chat-fr-ux-recovery`, `cpt-cf-mini-chat-fr-audit`, `cpt-cf-mini-chat-fr-cost-metrics`, `cpt-cf-mini-chat-fr-mcp-tool-discovery`, `cpt-cf-mini-chat-fr-mcp-tool-execution`, `cpt-cf-mini-chat-nfr-streaming-latency`, `cpt-cf-mini-chat-nfr-tenant-isolation`, `cpt-cf-mini-chat-nfr-observability-supportability`, `cpt-cf-mini-chat-nfr-resilience-recovery`

`cpt-cf-mini-chat-fr-conversation-history` is covered for the write side (user and assistant messages persisted per turn, history in the context) and for message count and chronological order across turns; the list API is `cpt-cf-mini-chat-flow-chat-crud-list-messages`. `cpt-cf-mini-chat-fr-web-search`, `cpt-cf-mini-chat-fr-code-interpreter` and `cpt-cf-mini-chat-fr-file-search` are covered for tool enablement, guards, tool events, call counting and citations; their quota facet belongs to `cpt-cf-mini-chat-feature-quota-billing` and their attachment facet to `cpt-cf-mini-chat-feature-attachments`. `cpt-cf-mini-chat-fr-audit` is covered for the turn finalization event; mutation events belong to `cpt-cf-mini-chat-feature-turn-mutations`. `cpt-cf-mini-chat-fr-cost-metrics` is covered for its request group (model, tool call counts, time to first token, total latency). The two MCP requirements are traced only to confirm their Future status: no MCP tool is ever placed in a provider request.

**Principles**: `cpt-cf-mini-chat-principle-streaming-first`, `cpt-cf-mini-chat-principle-summary-over-history`, `cpt-cf-mini-chat-principle-linear-conversation`, `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Sends a message to an own chat and receives the SSE stream; reuses a `request_id` to replay a completed turn; disconnects (closing the tab, stop button) and thereby cancels the turn; queries the turn status after a lost connection |
| LLM provider (behind OAGW) | Receives the adapter request through the OAGW in-process proxy and streams provider events; in tests an OpenAI-compatible mock server behind OAGW, or a fake `ServiceGatewayClientV1` returning scripted SSE bodies |
| ToolKit runtime (gear host) | Registers the two routes of this feature; runs authentication, license middleware and extractors; hosts the shared outbox that stores the usage and audit events |
| Model policy plugin (via the model-policy gateway) | Supplies the catalog for the chat-model check and, through the quota preflight, the snapshot, kill switches and limits of the turn |
| Orphan watchdog (`cpt-cf-mini-chat-feature-background-workers`) | Competes for the CAS of a stalled `running` turn; a turn it wins ends the stream with `stream_interrupted` |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-mini-chat-fr-chat-streaming`, `cpt-cf-mini-chat-fr-streaming-cancellation`, `cpt-cf-mini-chat-fr-token-budget`, `cpt-cf-mini-chat-fr-ux-recovery`, `cpt-cf-mini-chat-fr-cost-metrics`, `cpt-cf-mini-chat-nfr-streaming-latency`, `cpt-cf-mini-chat-nfr-resilience-recovery`, UC-001 `cpt-cf-mini-chat-usecase-send-message`, UC-006 `cpt-cf-mini-chat-usecase-reconnect-turn-status`
- **Design**: [DESIGN.md](../DESIGN.md) — §2.2 "Model Capability Constraint (Images)", "Context Window Budget", "No Buffering", "Bounded Channels", "Quota Before Outbound"; §3.2 `llm_provider` (`cpt-cf-mini-chat-component-llm-provider`), chat service (`cpt-cf-mini-chat-component-chat-service`); §3.3 "Streaming Contract" (`cpt-cf-mini-chat-contract-sse-streaming`), "Attachment Preflight Validation Invariant (P1)", "Turn Status API" (`cpt-cf-mini-chat-interface-turn-status`), "SSE Event Definitions", "SSE Event Ordering", "Provider Event Translation", REST error mapping, "Streaming error codes"; §3.5 "LLM Provider"; §3.6 `cpt-cf-mini-chat-seq-send-message`, `cpt-cf-mini-chat-seq-cancellation`; §3.7 `chats`, `messages`, `chat_turns`, `message_attachments`, `thread_summaries`; §3.8 per-operation matrix (`send_message`, `read_turn`); §4 "Context Plan Assembly and Truncation", "ContextPlan Determinism and Snapshot Boundary (P1)", "File Search Tool Availability", "Code Interpreter Tool Availability" (`cpt-cf-mini-chat-design-code-interpreter`), "Web Search Configuration" (`cpt-cf-mini-chat-design-web-search`), "Code Interpreter Configuration" (`cpt-cf-mini-chat-design-code-interpreter-config`), "Citation File ID and Title Resolution", "Knowledge Search", "Model Catalog Configuration", "Provider Request Metadata", "Internal: Multimodal Input Format", "Cancellation Observability", "Metrics (OTLP) and Alerts (P1)", "SSE Infrastructure Requirements", "Turn Lifecycle, Crash Recovery and Orphan Handling" (`cpt-cf-mini-chat-design-turn-lifecycle`), "Idempotency Rules", "Parallel Turn Policy (P1)"; §5.7 terminal outcome taxonomy, "Terminal SSE Event Emission Guard (P1)", "Forbidden Patterns", user message persistence and content durability invariants; §5.8 "Internal Error Code Taxonomy"; §5.9 "Terminal Signal Race Resolution"; Appendix B.4, B.5.1, B.5.5, B.6, B.6.1, B.7
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.4
- **Acceptance checklist**: [acceptance-criteria.md](../acceptance-criteria.md) — owned items: Principles & Constraints "Context window budget enforced, for both the input message and the full assembled request" and "Streaming responses are never buffered before relaying"; Messages API "Message count and chronological ordering are tracked correctly across turns"; Streaming: Send Message (all three items); SSE Event Contract (all three items); Idempotency & Replay (all three items); Parallel Turn Enforcement (both items); Turn Lifecycle (both items); Context Assembly (both items); Error Mapping & Sanitization (both items, streaming part of the first); Web Search "Web search tool use is reported, cited, accounted, and quota-limited correctly" (reporting and citations; accounting and quota limits are owned by `cpt-cf-mini-chat-feature-quota-billing`)
- **ADRs**: `cpt-cf-mini-chat-adr-llm-provider-as-library`, `cpt-cf-mini-chat-adr-internal-transport`, `cpt-cf-mini-chat-adr-canonical-error-contract`, `cpt-cf-mini-chat-adr-multi-provider-adapters`, `cpt-cf-mini-chat-adr-mcp-deferred`, `cpt-cf-mini-chat-adr-document-retrieval-scope`, `cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`
- **Foundation elements reused by reference** (from `cpt-cf-mini-chat-feature-gear-foundation`, not redefined here): request gate `cpt-cf-mini-chat-flow-gear-foundation-request-gate`; PEP `cpt-cf-mini-chat-algo-gear-foundation-authorize` (actions `send_message`, `read_turn`); chat-scoped access `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`; DomainError to `Problem` mapping `cpt-cf-mini-chat-algo-gear-foundation-map-error`; provider resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`; OAGW provisioning and its alias rule `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw`; plugin resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`; outbox queues `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`; audit delivery `cpt-cf-mini-chat-algo-gear-foundation-handle-audit`; event encoding `cpt-cf-mini-chat-algo-gear-foundation-encode-events`; configuration `cpt-cf-mini-chat-algo-gear-foundation-load-config` (`streaming`, `context`, `rag`, `knowledge_search`, `outbox.audit_queue_name`); metric instruments `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`; schema `cpt-cf-mini-chat-dod-gear-foundation-schema`
- **Quota elements reused by reference** (from `cpt-cf-mini-chat-feature-quota-billing`): the quota steps of a turn `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`; preflight `cpt-cf-mini-chat-algo-quota-billing-preflight`; reserve estimation (item token formula) `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`; reserve write `cpt-cf-mini-chat-algo-quota-billing-reserve-write`; CAS-guarded finalization `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`; billing outcome `cpt-cf-mini-chat-algo-quota-billing-billing-outcome`; turn usage event `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`; per-turn tool limit `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit`; quota warnings `cpt-cf-mini-chat-algo-quota-billing-quota-view`; quota metrics `cpt-cf-mini-chat-algo-quota-billing-record-metrics`
- **Chat CRUD elements reused by reference** (from `cpt-cf-mini-chat-feature-chat-crud`): message list `cpt-cf-mini-chat-flow-chat-crud-list-messages` and its enrichment `cpt-cf-mini-chat-algo-chat-crud-enrich-messages` (where persisted turns are read back); `message_count` in `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`; the chat ordering by `updated_at` of `cpt-cf-mini-chat-algo-chat-crud-paginate`
- **Dependencies**: `cpt-cf-mini-chat-feature-chat-crud`, `cpt-cf-mini-chat-feature-quota-billing`

**Source precedence**: DESIGN is authoritative for details. Where DESIGN delegates schemas to the generated OpenAPI (`docs/api/api.json`), the schemas `StreamMessageRequest`, `WebSearchConfig`, `MiniChatSseEvent`, `StreamStartedData`, `ThreadSummaryInfo`, `PingData`, `DeltaData`, `ToolData`, `CitationsData`, `Citation`, `TextSpan`, `DoneData`, `Usage`, `QuotaWarning`, `ErrorData` and `TurnStatusResponse` apply. The gear docs win over `config/mini-chat.yaml` and platform docs (DECOMPOSITION task-level overrides).

**REST contract** (paths under `{url_prefix}`, default `/mini-chat`; both routes require authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`; PEP resource type `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, `require_constraints` `true`, `resource.id` the chat id):

| Method and path | PEP action | Request | Success | Domain errors (besides gate, extractor and PEP errors), all as `Problem` before any stream |
|---|---|---|---|---|
| `POST /v1/chats/{id}/messages:stream` | `send_message` (no `read`) | JSON `StreamMessageRequest` (below) | 200, `text/event-stream` | 400 `EMPTY_CONTENT`, `invalid_attachment`, `TOO_MANY_IMAGES`, `INVALID_MODEL`, `INPUT_TOO_LONG`, `CONTEXT_BUDGET_EXCEEDED`, `VISION_NOT_SUPPORTED`, `failed_precondition` `FEATURE_DISABLED` (`web_search`, `images`); 404 chat; 409 `request_id_conflict`, `turn_already_running`; 429 `quota_exceeded` (`tokens`, `web_search`, `code_interpreter`); 500 `internal` (policy or provider resolution, database) |
| `GET /v1/chats/{id}/turns/{request_id}` | `read_turn` | none; `request_id` must be a UUID | 200, `TurnStatusResponse` | 404 chat; 404 turn (`gts.cf.core.mini_chat.turn.v1~`) |

`StreamMessageRequest` has `content` (string, required), `request_id` (UUID of any version, optional or `null`; the server generates a UUID v4 when absent), `attachment_ids` (array of UUIDs, optional; an empty array equals absence) and `web_search` (object `{ "enabled": boolean }`, optional or `null`, meaning `{ "enabled": false }`). There is no `model` field: the turn runs on `chats.model` through the quota cascade (`cpt-cf-mini-chat-constraint-model-locked-per-chat`); unknown body fields are ignored. A body that does not match the schema (missing `content`, non-UUID `request_id` or `attachment_ids` entry) is 422 `invalid_json_body`, malformed JSON 400, a non-JSON `Content-Type` 415.

`TurnStatusResponse` has `request_id` (UUID), `state` (`running` \| `done` \| `error` \| `cancelled`), `error_code` (string, present only for `error` with a stored code), `assistant_message_id` (UUID, present for `done`, and for `cancelled` when a partial message was persisted) and `updated_at` (RFC 3339 UTC). It never carries `chat_id`, provider identifiers, billing outcomes or settlement details.

**SSE event contract** (each event is one `event: <name>` line and one `data: <compact JSON>` line followed by a blank line; keys marked optional are omitted, never sent as `null`):

| Event | `data` JSON | Emitted |
|---|---|---|
| `stream_started` | `request_id` (UUID), `message_id` (UUID; pre-allocated for a new turn, the persisted assistant message id on replay), `is_new_turn` (boolean), optional `thread_summary_applied` `{ "token_estimate": integer }` | Exactly once, first |
| `ping` | `{}` | Only between `stream_started` and the first `delta` or `tool`, after every `streaming.sse_ping_interval_seconds` (default 15, `5..=60`) of idle time; never on replay |
| `delta` | `type` (`text`, or `reasoning` for `<think>` text of the vLLM adapter), `content` (string fragment) | Per provider text fragment, in arrival order; on replay exactly one `text` delta with the full persisted text |
| `tool` | `phase` (`start` \| `done`), `name` (`file_search`, `web_search`, `code_interpreter`, `function_call`, `search_knowledge`, `load_files`, `unknown_tool`), `details` (object: `{}`; `{ "files_searched": n }` for `file_search` `done`; `{ "output": string }` for OpenAI `code_interpreter` `done`; `index`, `call_id`, `name` / `call_id`, `name`, `arguments` for `function_call`) | Per provider tool event, interleaved with `delta` |
| `citations` | `items` array; each item `source` (`file` \| `web`), `title`, `snippet`, optional `url` (web), optional `attachment_id` (file), optional `span` `{ "start", "end" }`; never `score` | At most once, after the last `delta` and before `done`, only on a normally completed (not `incomplete`) stream with at least one mapped item; never on replay |
| `done` | `usage` `{ "input_tokens", "output_tokens" }`, `effective_model`, `selected_model`, `quota_decision` (`allow` \| `downgrade`), optional `downgrade_from` (equals `selected_model`), optional `downgrade_reason` (`premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier`, `model_disabled`), optional `quota_warnings` (array of `QuotaWarning`) | Terminal; only after the finalization committed the turn as `completed` |
| `error` | `code` (streaming error code), `message` (sanitized text, not contract) | Terminal; never for a client disconnect |

Ordering: `stream_started ping* (delta | tool)* citations? (done | error)`. The connection is closed right after the terminal event. Response headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`; no `Connection: keep-alive`, no compression; the HTTP server adds an SSE comment keep-alive (`:` line) every 30 s that clients receive as no event (`cpt-cf-mini-chat-adr-runtime-consistency-limitations`).

Streaming error codes sent in `error`: `provider_error`, `provider_timeout`, `rate_limited`, `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `agentic_iterations_exceeded`, `unexpected_tool_use`, `message_persistence_failed`, `finalization_failed`, `stream_interrupted`. Quota exhaustion never appears in `error`; it is always a pre-stream 429.

**OpenAI Responses wire contract** (`openai_responses`; the hidden-mock contract every other adapter mirrors in its own wire format). The request is `POST /{alias}{api_path}` through `ServiceGatewayClientV1::proxy_request` with the request's `SecurityContext`, where `alias` is the resolver alias (the alias OAGW stores, see the alias row of the foundation deviation table) and `{model}` in `api_path` (default `/v1/responses`, query part such as `?api-version=` kept) is replaced by `provider_model_id`. Headers: `Content-Type: application/json`, `Accept: text/event-stream`; no credential header (OAGW injects it).

| Body field | Value |
|---|---|
| `model` | The effective model's `provider_model_id` |
| `instructions` | The catalog `system_prompt` followed by the tool guards of the turn, joined by a blank line; omitted when the result is empty |
| `input` | Array of message items in context order: the thread summary as `{ "role": "user", "content": [ { "type": "input_text", "text": preamble + blank line + summary } ] }` when kept; each kept history message as `{ "role": "user", "content": [ { "type": "input_text", "text": ... } ] }` or `{ "role": "assistant", "content": [ { "type": "output_text", "text": ... } ] }`; the current message as `{ "role": "user", "content": [ { "type": "input_text", "text": content }, { "type": "input_image", "file_id": provider_file_id }, ... ] }` with one `input_image` per image of `attachment_ids` |
| `tools` | Present only when non-empty, in this order: `{ "type": "file_search", "vector_store_ids": [chat vector store id], "max_num_results": catalog max_num_results }`, `{ "type": "web_search", "search_context_size": catalog web_search_context_size }`, `{ "type": "code_interpreter", "container": { "type": "auto", "file_ids": [...] } }`, the `search_knowledge` function tool |
| `include` | `["code_interpreter_call.outputs"]` only when `code_interpreter` is in `tools` |
| `max_tool_calls` | Catalog `max_tool_calls` (default 2), only when `tools` holds a built-in tool |
| `max_output_tokens` | `max_output_tokens_applied` of the turn |
| `stream` | `true` |
| `store` | `false` |
| `user` | `{tenant_id_hex}{user_id_hex}` (two simple 32-hex UUIDs, tenant first, 64 characters) |
| `metadata` | `{ "tenant_id", "user_id", "chat_id", "request_type": "chat", "feature" }`; `feature` is `none` or the built-in tools of the request joined by `+` in the order `file_search`, `web_search`, `code_interpreter` |
| `temperature`, `top_p`, `frequency_penalty`, `presence_penalty` | Each only when set in `general_config.api_params` |
| `reasoning` | `{ "effort": api_params.reasoning_effort }` only when set |
| `extra_body` keys | Merged at the top level, except `model`, `input`, `messages`, `instructions`, `system`, `stream`, `stream_options`, `max_output_tokens`, `max_completion_tokens`, `max_tokens`, `max_tool_calls`, `tools`, `tool_choice`, `include`, `store`, `previous_response_id`, `user`, `metadata`, which are ignored with a warning |

Never sent: `previous_response_id`, `tool_choice`, `stop`, any MCP tool. The response must be `text/event-stream`; the event name is the SSE `event:` line, or the `type` field of the `data` JSON when the line is missing or `message`. Translation (DESIGN "Provider Event Translation"): `response.output_text.delta` to `delta` text (`data.delta`); `response.file_search_call.searching` / `.completed` to `tool` `file_search` start / done (`files_searched` = length of `results`, 0 when absent); `response.web_search_call.searching` / `.completed` to `tool` `web_search` start / done; `response.code_interpreter_call.in_progress` to `tool` `code_interpreter` start; `response.output_item.done` with an item of type `code_interpreter_call` to `tool` `code_interpreter` done with `output` = the `logs` outputs joined by `"\n"`, capped at 8192 characters plus `...[truncated]`; `response.output_text.annotation.added` and the annotations of `response.completed` to `citations`; `response.completed` to `done` (usage from `response.usage`, `response.id` kept internally); `response.incomplete` to `done` (reason from `response.incomplete_details.reason`); `response.failed` and the SSE `error` event to `error` `provider_error`. Every other event (`response.created`, `response.in_progress`, `response.output_item.added`, `response.content_part.*`, `response.output_text.done`, `response.code_interpreter_call.interpreting`, `response.code_interpreter_call.completed`, ...) is ignored.

**Feature-local deviations and interpretations** (review owner: mini-chat gear maintainers):

| Topic | Deviation or interpretation | Rationale |
|-------|-----------------------------|-----------|
| Check order before the stream | `EMPTY_CONTENT`, duplicate `attachment_ids` and the `attachment_ids` count run before authorization; then PEP, chat load, idempotency and parallel-turn guard, the chat-model catalog check (`INVALID_MODEL`), turn input loading (`TOO_MANY_IMAGES`), the quota preflight, `INPUT_TOO_LONG`, `disable_images`, vision, context assembly (`CONTEXT_BUDGET_EXCEEDED`), provider resolution and request building; ownership and readiness of `attachment_ids` are checked inside the reserve transaction | DESIGN fixes the idempotency-before-guard order, the "before any quota step" position of `INVALID_MODEL` (`cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`), the in-transaction attachment check and the input-limit-then-image order; pure body checks need no PDP or database call, as for titles in `cpt-cf-mini-chat-feature-chat-crud`. Replay therefore works for a chat whose model was later removed from the catalog |
| Chat-model catalog check | Before the preflight the pipeline reads the current policy version and snapshot (no cache) and rejects a `chats.model` absent from `model_catalog` (enabled or not) with 400 `INVALID_MODEL`; a disabled model proceeds and is downgraded by the cascade (`model_disabled`) | DESIGN requires `INVALID_MODEL` for a removed chat model before any quota step; the preflight helper treats a missing model as a defensive case only |
| Replay content | Replay sends `stream_started` (`is_new_turn` `false`, `message_id` the persisted assistant message, no `thread_summary_applied`), exactly one `delta` `text` with the full persisted content (also when it is empty), and `done` with `usage` from the assistant message, `effective_model` from `chat_turns.effective_model`, `selected_model` from `chats.model`, `quota_decision` `allow` when the two are equal and `downgrade` with `downgrade_from` otherwise, no `downgrade_reason` and no `quota_warnings`; no `ping`, no `citations`; the `content`, `attachment_ids` and `web_search` of the replaying request are not compared with the original | DESIGN "Idempotency" table and `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (replay rebuilds the decision, omits the reason and citations); "one `delta` with the full text" is literal |
| Settlement snapshot of a stream turn | The stream finalization passes the preflight snapshot (the snapshot of the turn's `policy_version_applied`) as the settlement snapshot; no plugin call runs on the terminal path | The quota feature defines the stream variant this way (`FinalizeRequest.snapshot`), which satisfies "fetched before the transaction opens" without a second plugin call |
| OpenAI Responses body details | `stream` `true`, `store` `false`, no `previous_response_id` and no `tool_choice`; `instructions` carries the system prompt and guards and is omitted when empty; history uses `input_text` (user) and `output_text` (assistant) parts; `max_tool_calls` only with a built-in tool; `stop` is not sent by the two Responses adapters; `reasoning_effort` is sent as `reasoning.effort` | DESIGN lists `store`, `stream`, `instructions`, `tool_choice` and `previous_response_id` among the keys the request controls but gives no values; the gear assembles the full context on every turn, so no provider-side storage or chaining is used; the Responses API has no `stop` field |
| Chat Completions body details | `messages` (`system`, `user`, `assistant` roles with string content; images as `image_url` parts are not sent because the adapter has no file id form), `stream` `true`, `stream_options` `{ "include_usage": true }`, `max_completion_tokens` = `max_output_tokens_applied`, `stop` when non-empty, `reasoning_effort` when set, `user` | `max_tokens` is rejected by reasoning models; `include_usage` is the only way to receive usage on a streamed completion |
| Anthropic usage normalization | `input_tokens` reported to settlement is Anthropic `input_tokens` plus `cache_read_input_tokens` plus `cache_creation_input_tokens`; `cache_read_input_tokens` and `cache_write_input_tokens` keep their own values | DESIGN defines the cache counts as subsets of `input_tokens`; Anthropic reports them separately |
| Annotation source | Annotations are taken from `response.output_text.annotation.added` events; when none was streamed, from the `output_text` parts of the `response.completed` payload; one citation item per mapped annotation in provider order, no de-duplication | DESIGN names "annotations in response" without fixing the event; both are valid Responses API carriers |
| Assistant message timestamp | The assistant message `created_at` is the finalization time, but at least the user message `created_at` plus 1 microsecond | The message order `(created_at, id)` must put the answer after its question even when the clock resolution or a fast mock gives equal instants |
| Reasoning deltas | `reasoning` deltas of the vLLM adapter are relayed but not part of the persisted assistant content | DESIGN defines the persisted content as the assistant text; `<think>` text is model reasoning |
| Tool limit breach event | The `tool` `start` event that breaches the per-turn limit is not relayed; the stream ends with `error` of the limit code | The breaching call is cancelled; relaying its start would announce a tool run that never completes |
| Citations and the finalization | `citations` is sent after the provider's completion event and before the finalization transaction | Non-terminal events are not gated by the CAS (DESIGN §5.7 note); `done` stays gated |
| `quota_warnings` presence | On a CAS-winning `completed` turn `quota_warnings` is always present (possibly `[]`); when the post-commit read of the buckets fails, the key is omitted, the error is logged and `done` is still sent | DESIGN: present on CAS-winning completed turns; the turn is already committed and must not be reported as failed |
| Message persistence failure | When the assistant message INSERT fails in the finalization transaction of a `completed` stream, the transaction rolls back and the finalization is run again as `failed` / `message_persistence_failed` without a message (usage kept); `error` `message_persistence_failed` is sent; if that second run also fails, `error` `finalization_failed` is sent and the turn stays `running`. For a `cancelled` turn a failed partial-message INSERT is logged at `warn` and the finalization is run again as `cancelled` without a message | The quota helper runs the caller's INSERT in its own transaction, so "finalize as failed" means a second, message-free finalization; DESIGN §5.7 "Clarification" |
| Progress writes | `last_progress_at` (at most every 30 s on text deltas and tool events) and the `*_completed_count` columns are updated by single-statement writes `WHERE state = 'running'`, dispatched off the relay path with at most one in flight per turn; a failed write is logged and retried with the next event | DESIGN requires durable progress for the watchdog and no buffering in the relay path; a write on the hot path would delay relayed events |
| `error_detail` | For a failed turn: the provider error code and the sanitized provider message, sanitized first and then cut to at most 1024 characters; NULL for codes without provider detail | DESIGN: non-sensitive diagnostic text, not exposed; sanitizing before the cut means a cut can never leave an unrecognized fragment of an identifier |
| Sanitizer patterns (widening) | Besides the DESIGN patterns (URLs, `sk-` keys, `Bearer` tokens, `resp_` / `chatcmpl-` / `cmpl-` / `msg_` ids, `file-` / `file_` / `assistant-` / `vs_` ids of at least 12 characters), `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error` also replaces: `Bearer` in any letter case followed by any non-space token; `sk-` keys whose tail contains `_` or `-` (`sk-proj-...`); prefixed ids `(resp\|chatcmpl\|cmpl\|msg\|req\|org\|proj\|call\|fc\|rs\|ws\|ci\|toolu\|file\|vs\|assistant\|thread\|run\|step)[-_]` followed by at least 6 characters of `[A-Za-z0-9_-]` that contain a digit; UUIDs; and the value after an `x-request-id` header name. URLs with user info or query strings are already covered by the DESIGN URL rule | DESIGN §3.3 "Provider identifier non-exposure invariant" forbids any provider identifier, correlation ids included, in error text, but its pattern list misses request, organization, project, call and run ids, `sk-proj-` keys and UUID request ids. Pattern replacement is kept rather than an allowlist of generic texts, because DESIGN defines the SSE `message` of a provider error as the sanitized provider message. The digit requirement keeps ordinary words such as `file_search`, `file-based` or `step-by-step` intact. Review owner: mini-chat gear maintainers |
| Provider hang (residual risk) | A provider that keeps its connection open without sending events holds the stream, the `running` turn and the chat's one-running-turn slot. This lasts until the OAGW upstream idle or total timeout ends the read (`error` `provider_timeout`, turn `failed`), until the client disconnects (turn `cancelled`), or until the orphan watchdog finalizes the turn (`orphan_timeout`). In the watchdog case, the provider task's liveness check (`inst-check-turn-running`) notices it within one `streaming.sse_ping_interval_seconds` and the relay sends `stream_interrupted`. The gear adds no deadline of its own. Owner: mini-chat gear maintainers | DESIGN forbids any internal abort of normal generation (§5.7 "There is no internal-abort trigger", `cpt-cf-mini-chat-seq-cancellation`). The hang is therefore bounded by OAGW configuration and the watchdog, not by the gear. Operators tune the bound through the OAGW upstream timeouts of the provider upstream |
| Stream end without terminal event | A provider stream that ends (EOF or `data: [DONE]` without a finish signal) without a completion, incomplete or error event is `failed` / `provider_error` | DESIGN maps a provider disconnect to `provider_error` |
| Recent message roles | The recent-messages query selects `user` and `assistant` rows only | P1 writes no `system` messages; a `system` row has no defined place in the provider input |
| Guard order | Guards are appended in the order `file_search`, `web_search`, `search_knowledge` | DESIGN says "concatenated after the main system prompt" without an order; a fixed order keeps the context deterministic |
| `INPUT_TOO_LONG` estimate | The current message is estimated with the text formula of the effective model's `estimation_budgets` over its UTF-8 bytes; images are not part of this check | DESIGN: "the current message estimate above `max_input_tokens`"; images are bounded by their count |
| Latency definitions | `mini_chat_stream_total_latency_ms` and the audit `latency_ms` run from the stream service accepting the request to the terminal event being handed to the relay (audit: to the finalization commit); `mini_chat_ttft_provider_ms` from sending the provider request to the first content event; `mini_chat_ttft_overhead_ms` from receiving that event to its send on the internal channel | DESIGN names the instruments and the TTFT-overhead definition only |
| Per-request cost record | Every won finalization writes one structured log record `turn finalized` with chat, turn and request ids, provider id, effective model, terminal state, error code, input and output tokens, web search, file search and code interpreter call counts, TTFT and total latency | `cpt-cf-mini-chat-fr-cost-metrics` requires these values "for every LLM request" without tenant or user labels; the quota feature writes the credit values in `quota settled` |

**External-call timeouts** (foundation and quota constants, reused unchanged; every call to a dependency that can stall is bounded):

| Constant | Value | Call in this feature | Outcome on timeout |
|----------|-------|----------------------|--------------------|
| `PDP_EVALUATION_TIMEOUT` | 5 s | PEP evaluation of `send_message` and `read_turn` | 503 `service_unavailable`, `Retry-After: 5`; no stream is opened |
| `REGISTRY_QUERY_TIMEOUT` | 10 s | Model-policy plugin resolution of the chat-model check and of the preflight | 500 `internal`; nothing is reserved |
| `POLICY_PLUGIN_CALL_TIMEOUT` | 10 s | `get_current_policy_version`, `get_policy_snapshot` (chat-model check) and the preflight calls of `cpt-cf-mini-chat-algo-quota-billing-resolve-policy` | 500 `internal`; nothing is reserved |
| OAGW upstream timeouts (OAGW configuration) | per upstream | The provider `proxy_request` and its streamed body; the knowledge-search retrieval request | Gateway timeout or OAGW HTTP 504 `deadline_exceeded`: `error` `provider_timeout`, the turn is `failed`; for a retrieval: a failed retrieval (`mini_chat_knowledge_search{result="error"}`), the loop continues |

The gear adds no deadline of its own to the provider stream and has no internal abort. The only cancellation source is the dropped SSE relay (DESIGN `cpt-cf-mini-chat-seq-cancellation`), and every await of the provider task that waits on the provider or the client is raced against it (`inst-race-cancel`). Provider idle timeouts (no bytes within the upstream idle timeout) and total timeouts come only from the OAGW upstream timeouts of the provider upstream. They end the `proxy_request` call or the body read with a gateway timeout and surface as `error` `provider_timeout` (`inst-if-gateway-timeout`). A turn whose stream stalls without a terminal event is finalized by the orphan watchdog. The provider task checks at most once per ping interval whether its turn is still `running` (`inst-check-turn-running`). After the watchdog has won, the task drops the body and ends without finalizing, and the relay sends `stream_interrupted`. The remaining hang window is recorded as the "Provider hang (residual risk)" deviation. A slow client blocks the provider task on the bounded channel (backpressure), never memory.

**Transaction discipline** (normative, shared with `cpt-cf-mini-chat-algo-quota-billing-reserve-write` and `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`): every read the pipeline needs (chat, idempotency key, running turn, catalog, attachment state, snapshot boundary, summary, recent messages, preflight) runs before the reserve transaction opens. The reserve transaction is opened through `SeaQuotaRepository::reserve_with`; its first statement is the daily `total` `quota_usage` UPSERT (on SQLite the transaction may instead be begun `IMMEDIATE`), so it never upgrades from a read lock to a write lock; this feature's statements (user message, attachment check, `message_attachments`, `chat_turns`, `chats.updated_at`) run after the quota statements in the same transaction. The finalization transaction is opened through `SeaQuotaRepository::finalize_turn_with`; its first statement is the `chat_turns` CAS UPDATE (or `IMMEDIATE` on SQLite), and this feature's statements (assistant message INSERT, audit enqueue) run after the settlement and the usage enqueue. No plugin call and no provider call runs inside either transaction. Both closures are deterministic: ids, timestamps, content and payloads are computed before the transaction and captured by value, because a lock or serialization conflict (`SQLITE_BUSY`, PostgreSQL `40001` / `40P01`) re-runs the whole transaction, at most 3 attempts in total with the 20 ms / 100 ms backoff. A reserve that exhausts its attempts is 500 `internal` with nothing left behind; a finalization that exhausts them leaves the turn `running` for the orphan watchdog. Progress writes are single autocommit statements outside both transactions.

**Audit and metrics**: the feature enqueues one `TurnAuditEvent` per won stream finalization, inside the finalization transaction, to `outbox.audit_queue_name` partitioned by `tenant_id` (`cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`). It records the streaming, cancellation, finalization, knowledge-search and image instruments listed in `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`; the quota instruments are recorded by `cpt-cf-mini-chat-algo-quota-billing-record-metrics`. Replay records nothing and enqueues nothing.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-mini-chat-usecase-send-message`, `cpt-cf-mini-chat-usecase-reconnect-turn-status`

### Send Message

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-streaming-turns-send-message`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A new message on an own chat opens a 200 SSE stream: `stream_started` (`is_new_turn` `true`), optional `ping`s, `delta` and `tool` events as the provider produces them, an optional `citations`, and `done` after the turn was committed `completed` with the assistant message, the quota settlement, the usage event and the audit event.
- A message whose `request_id` names a completed, non-deleted turn of the chat is replayed from the database without a provider call, reserve, settlement or outbox write.
- A provider failure after the stream opened ends the stream with one `error` whose `code` is a streaming error code and whose `message` is sanitized; the turn is `failed` and settled.

**Error Scenarios**:
- Gate, extractor and PEP errors of `cpt-cf-mini-chat-flow-gear-foundation-request-gate`: 401, 403 `LICENSE_FEATURE_REQUIRED`, 403 `AUTHZ_DENIED`, 503 `Retry-After: 5`, 400 / 415 / 422 extractor problems.
- 400 `EMPTY_CONTENT`, `invalid_attachment`, `TOO_MANY_IMAGES`, `INVALID_MODEL`, `INPUT_TOO_LONG`, `FEATURE_DISABLED` (`web_search`, `images`), `VISION_NOT_SUPPORTED`, `CONTEXT_BUDGET_EXCEEDED`; 404 chat; 409 `request_id_conflict` or `turn_already_running`; 429 `quota_exceeded`; 500 `internal`. Each is a `Problem` and no stream is opened.

**Steps**:
1. [x] - `p1` - User calls `POST {url_prefix}/v1/chats/{id}/messages:stream` through the request gate (see `cpt-cf-mini-chat-flow-gear-foundation-request-gate`); the platform extractors reject a body that does not match `StreamMessageRequest` before the handler runs - `inst-call-send`
2. [x] - `p1` - The handler runs the whole setup (steps 3 to 20) in a separately spawned setup task and awaits only its result: a `Problem`, the replay stream, or the relay handle (channel receiver with its drop guard) together with the `stream_started` payload. A client disconnect during setup therefore never interrupts a started reserve transaction, and it never leaves a committed turn without its provider task. Everything after the reserve commit that a dropped handler could skip runs in the setup task - `inst-spawn-setup`
3. [x] - `p1` - Validate the request body (see `cpt-cf-mini-chat-algo-streaming-turns-validate-request`); **IF** it returns an error, **RETURN** its `Problem` - `inst-validate-request`
4. [x] - `p1` - Authorize action `send_message` on the chat resource with `resource.id` the path chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); `read` is not evaluated; **IF** it returns an error, **RETURN** 403 `AUTHZ_DENIED` or 503 with `Retry-After: 5` - `inst-authorize-send`
5. [x] - `p1` - Load the chat through the compiled scope (see `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`); **IF** it is missing, deleted or foreign, **RETURN** 404 with `gts.cf.core.mini_chat.chat.v1~` - `inst-load-chat`
6. [x] - `p1` - Resolve idempotency and the parallel-turn guard (see `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`) - `inst-resolve-idempotency`
7. [x] - `p1` - **IF** the result is `replay` - `inst-if-replay`
   1. [x] - `p1` - **RETURN** the replay stream (see `cpt-cf-mini-chat-algo-streaming-turns-replay-completed`); no further step runs - `inst-return-replay`
8. [x] - `p1` - **IF** the result is a conflict - `inst-if-conflict`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `request_id_conflict` or `turn_already_running` - `inst-return-conflict`
9. [x] - `p1` - Read the current policy version and the snapshot of that version through the model-policy gateway (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`); **IF** a call fails or times out, **RETURN** 500 `internal`; **IF** no `model_catalog` entry has `id` equal to `chats.model`, **RETURN** 400 `invalid_argument` with `field_violations[model].reason` `INVALID_MODEL` - `inst-check-chat-model`
10. [x] - `p1` - Load the turn inputs (see `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`); **IF** it returns `TOO_MANY_IMAGES`, **RETURN** 400 `out_of_range` with `field_violations[image_count].reason` `TOO_MANY_IMAGES` - `inst-load-inputs`
11. [x] - `p1` - Run the quota preflight with the tenant, the user, the chat id, `chats.model`, the UTF-8 byte length of `content`, the image count, `web_search.enabled` and the two ready-attachment flags of the turn inputs (see `cpt-cf-mini-chat-algo-quota-billing-preflight`); **IF** it returns an error, **RETURN** its `Problem` (400 `FEATURE_DISABLED` `web_search`, 429 `quota_exceeded`, 500) - `inst-preflight`
12. [x] - `p1` - **IF** the effective model's `max_input_tokens` is greater than 0 and the text estimate of `content` with the effective model's `estimation_budgets` (the formula of `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`, steps `inst-clamp-ratio` to `inst-text-tokens`) is greater than it - `inst-if-input-too-long`
    1. [x] - `p1` - **RETURN** 400 `out_of_range` with reason `INPUT_TOO_LONG`; nothing was reserved - `inst-return-input-too-long`
13. [x] - `p1` - **IF** the image count is greater than 0 and the preflight snapshot kill switch `disable_images` is `true`, **RETURN** 400 `failed_precondition` with `violations[{subject: images, type: FEATURE_DISABLED}]`; **ELSE IF** the image count is greater than 0 and the decision's vision capability is `false` (the effective model lacks `VISION_INPUT`, also after a downgrade), **RETURN** 400 `invalid_argument` `VISION_NOT_SUPPORTED`; images are never dropped silently - `inst-image-guards`
14. [x] - `p1` - Decide the tools of the turn (see `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`) - `inst-decide-tools`
15. [x] - `p1` - Assemble the context (see `cpt-cf-mini-chat-algo-streaming-turns-assemble-context`); **IF** it returns `CONTEXT_BUDGET_EXCEEDED`, **RETURN** 400 `out_of_range` with reason `CONTEXT_BUDGET_EXCEEDED` - `inst-assemble-context`
16. [x] - `p1` - Resolve the provider of the effective model's `provider_id` for the request tenant (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`) and build the provider request (see `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`); **IF** either fails, **RETURN** 500 `internal`; nothing was reserved - `inst-resolve-and-build`
17. [x] - `p1` - Run the reserve transaction that creates the turn (see `cpt-cf-mini-chat-algo-streaming-turns-create-turn`); **IF** it returns an error, **RETURN** its `Problem` (429 `quota_exceeded` `tokens`, 400 `invalid_attachment`, 409 `request_id_conflict` or `turn_already_running`, 500). **IF** it returns `replay` (a concurrent send with the same `request_id` won and its turn is already completed), **RETURN** the replay stream (see `cpt-cf-mini-chat-algo-streaming-turns-replay-completed`). A failed reserve transaction leaves no reserve, message, attachment link or turn - `inst-create-turn`
18. [x] - `p1` - In the setup task, record the reserve metrics (see `cpt-cf-mini-chat-algo-quota-billing-record-metrics`, `inst-count-reserve`) and the image count of the turn (see `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`) - `inst-record-reserve`
19. [x] - `p1` - In the setup task, create the bounded channel of `streaming.sse_channel_capacity` (default 32) events and the turn's cancellation token. Wrap the receiver in the relay handle, whose drop guard cancels the token and records the disconnect metrics of `inst-drop-relay` whenever the handle is dropped. This happens whether the SSE body was dropped, or the setup result was never delivered because the handler was dropped - `inst-create-channel`
20. [x] - `p1` - In the setup task, spawn the provider task with the turn, the decision, the context, the provider request, the channel sender and the cancellation token (see `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`). Then hand the relay handle and the `stream_started` payload (`request_id`, the pre-allocated assistant `message_id`, `is_new_turn` `true`, `thread_summary_applied` when the summary was kept) to the handler. The handler receives nothing else - `inst-spawn-provider-task`
21. [x] - `p1` - **IF** the handler is gone when the setup task hands over its result (the client disconnected after the reserve commit), the undelivered relay handle is dropped and its guard cancels the token. The provider task then finds the token already cancelled before its provider call (`inst-cancelled-before-call`). It finalizes the turn as `cancelled` without calling the provider, with no assistant message and the `aborted` / `estimated` settlement, so the turn is never left `running` - `inst-handler-gone`
22. [x] - `p1` - In the handler, open the 200 SSE response and start the relay with the received handle and `stream_started` payload (see `cpt-cf-mini-chat-algo-streaming-turns-relay-sse`) - `inst-open-stream`
23. [x] - `p1` - **RETURN** the SSE stream. It ends with the terminal event sent by the provider task after the finalization commit, with `stream_interrupted` when the task ends without one, or silently on a client disconnect - `inst-return-stream`

### Get Turn Status

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with `request_id`, `state` mapped from `chat_turns.state` (`running` to `running`, `completed` to `done`, `failed` to `error`, `cancelled` to `cancelled`) and `updated_at`; `error_code` for `error`; `assistant_message_id` for `done` and for `cancelled` with a persisted partial message.

**Error Scenarios**:
- Gate and PEP errors; a non-UUID `request_id` is 400 `invalid_path_params`.
- 404 with the chat type for a missing, deleted or foreign chat; 404 with `gts.cf.core.mini_chat.turn.v1~` for an unknown or soft-deleted turn.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/chats/{id}/turns/{request_id}` through the request gate; the path extractor requires both ids to be UUIDs - `inst-call-status`
2. [x] - `p1` - Authorize action `read_turn` with `resource.id` the chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`) - `inst-authorize-read-turn`
3. [x] - `p1` - Load the chat and the child turn (see `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`): DB: SELECT `chat_turns` through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is the loaded chat, `request_id` is the path value and `deleted_at IS NULL` - `inst-load-turn`
4. [x] - `p1` - **IF** no row is found (unknown, soft-deleted by retry, edit or delete, or belonging to another chat) - `inst-if-turn-missing`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.turn.v1~` - `inst-return-turn-404`
5. [x] - `p1` - **MATCH** `chat_turns.state` - `inst-match-state`
   - [x] - `p1` - **CASE** `running`: `state` `running`, no `error_code`, no `assistant_message_id` - `inst-case-running`
   - [x] - `p1` - **CASE** `completed`: `state` `done`, `assistant_message_id` the stored id - `inst-case-done`
   - [x] - `p1` - **CASE** `failed`: `state` `error`, `error_code` the stored code when not NULL (for example `provider_error`, `orphan_timeout`, `web_search_calls_exceeded`), no `assistant_message_id` - `inst-case-error`
   - [x] - `p1` - **CASE** `cancelled`: `state` `cancelled`, `assistant_message_id` only when a partial message was persisted - `inst-case-cancelled`
6. [x] - `p1` - Never project `chat_id`, `provider_response_id`, `error_detail`, reserve fields, billing outcome or settlement data; omit absent optional keys - `inst-status-non-exposure`
7. [x] - `p1` - **RETURN** 200 with `{ request_id, state, error_code?, assistant_message_id?, updated_at }` (`updated_at` RFC 3339 UTC) - `inst-return-status`

### Client Disconnect

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-streaming-turns-client-disconnect`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A disconnect before the provider's terminal event closes the provider connection, finalizes the turn as `cancelled` with an `aborted` / `estimated` settlement, persists the non-empty partial text as the assistant message, and sends nothing.
- A disconnect after the provider's terminal event changes nothing: the provider outcome is finalized as it would be with the client connected.

**Error Scenarios**:
- The finalization transaction fails: the turn stays `running` and the orphan watchdog finalizes it later as `failed` / `orphan_timeout`.
- Another finalizer already won the CAS: nothing is settled or enqueued by this path.

**Steps**:
1. [x] - `p1` - User closes the connection or presses stop while the SSE stream is open - `inst-user-disconnects`
2. [x] - `p1` - The HTTP server drops the SSE response; the relay's drop guard cancels the turn's cancellation token, records `mini_chat_cancel_requested{trigger="disconnect"}` and `mini_chat_stream_disconnected{stage}` (`before_first_token` before any `delta`, else `mid_stream`) and stops reading the channel - `inst-drop-relay`
3. [x] - `p1` - **IF** the provider task is blocked on a full channel when the receiver is dropped, its failed send is treated as the same disconnect (without the `cancel_requested` count) - `inst-failed-send`
4. [x] - `p1` - The provider task observes the cancellation through the cancellation arm, which comes first, of whichever raced await is pending (see `inst-race-cancel`): the `proxy_request` call, the read of the next provider event, a knowledge-search retrieval call or a channel send. It does not wait for the next provider event, so a provider that holds its stream open without sending is aborted at once. The task drops the pending operation and the proxied response body, which closes the OAGW upstream connection so that the provider stops generating. It records `mini_chat_time_to_abort_ms{trigger="disconnect"}` from the observed disconnect to the exit of the read loop. A disconnect before the provider call (the handler or the relay dropped right after the reserve commit) makes the task skip the provider call (`inst-cancelled-before-call`) - `inst-abort-provider`
5. [x] - `p1` - **IF** the provider's terminal event (completion, incomplete or error) was already received - `inst-if-terminal-received`
   1. [x] - `p1` - Finalize with that outcome (see `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`); the terminal SSE event is not written because the stream is gone - `inst-finalize-provider-outcome`
6. [x] - `p1` - **ELSE** - `inst-else-cancel`
   1. [x] - `p1` - Finalize as `cancelled` with the accumulated text and no usage (see `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`); the settlement is `aborted` / `estimated` by `cpt-cf-mini-chat-algo-quota-billing-billing-outcome` - `inst-finalize-cancelled`
   2. [x] - `p1` - **IF** the CAS was won, record `mini_chat_cancel_effective{trigger="disconnect"}` and `mini_chat_streams_aborted{trigger="client_disconnect"}` - `inst-record-cancel-effective`
7. [x] - `p1` - **RETURN** without writing any SSE event; the Turn Status API is the authoritative source of the outcome - `inst-return-silent`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Send Request Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-validate-request`

**Input**: The decoded `StreamMessageRequest`, `rag.max_documents_per_chat`, `rag.max_images_per_message`

**Output**: The normalized request (`content`, `request_id`, distinct `attachment_ids`, `web_search_enabled`), or a validation error

**Steps**:
1. [x] - `p1` - **IF** `content` is empty or consists only of whitespace - `inst-if-empty-content`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[content].reason` `EMPTY_CONTENT` - `inst-return-empty-content`
2. [x] - `p1` - Keep `content` unchanged (it is sent and stored as given, not trimmed) - `inst-keep-content`
3. [x] - `p1` - Set `request_id` to the given UUID (any version) or, when absent or `null`, to a new UUID v4 - `inst-resolve-request-id`
4. [x] - `p1` - Set `web_search_enabled` to `web_search.enabled`, or `false` when `web_search` is absent or `null` - `inst-web-search-flag`
5. [x] - `p1` - **IF** `attachment_ids` contains the same id more than once, or more than `rag.max_documents_per_chat` plus `rag.max_images_per_message` ids - `inst-if-ids-invalid`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[attachment].reason` `invalid_attachment` - `inst-return-ids-invalid`
6. [x] - `p1` - **RETURN** the normalized request; an absent `attachment_ids` is an empty list - `inst-return-normalized`

### Idempotency and Parallel-Turn Guard

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`

**Input**: The loaded chat, `request_id`

**Output**: `replay` with the turn row, `request_id_conflict`, `turn_already_running`, or `proceed`

**Steps**:
1. [x] - `p1` - DB: SELECT `chat_turns` through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is the loaded chat and `request_id` is the request value, including soft-deleted rows (the idempotency key stays taken by `UNIQUE (chat_id, request_id)`) - `inst-select-key`
2. [x] - `p1` - **IF** a row exists - `inst-if-key-exists`
   1. [x] - `p1` - **IF** its `state` is `completed` and `deleted_at IS NULL` - `inst-if-completed`
      1. [x] - `p1` - **RETURN** `replay` with the row; no further check runs (replay is checked before the parallel-turn guard, so a completed turn replays while another turn of the chat is running) - `inst-return-replay`
   2. [x] - `p1` - **RETURN** `request_id_conflict` (`running`, `failed`, `cancelled`, or any soft-deleted state) - `inst-return-key-conflict`
3. [x] - `p1` - DB: SELECT `chat_turns` WHERE `chat_id` is the loaded chat, `state` is `running` and `deleted_at IS NULL` (any `request_id`) - `inst-select-running`
4. [x] - `p1` - **IF** a row exists - `inst-if-running`
   1. [x] - `p1` - **RETURN** `turn_already_running` - `inst-return-running`
5. [x] - `p1` - **RETURN** `proceed`; the database uniqueness of `(chat_id, request_id)` and of one running turn per chat remains the final arbiter inside the reserve transaction, where any unique violation is resolved by a re-read (see `cpt-cf-mini-chat-algo-streaming-turns-create-turn`, `inst-if-key-race`) - `inst-return-proceed`

### Completed Turn Replay

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-replay-completed`

**Input**: The loaded chat, the completed turn row

**Output**: An SSE stream of `stream_started`, one `delta` and `done`, or an internal error before the stream opens

**Steps**:
1. [x] - `p1` - DB: SELECT the `messages` row through Secure ORM (tenant column `tenant_id`) WHERE `id` is `chat_turns.assistant_message_id`, `chat_id` is the chat and `deleted_at IS NULL` - `inst-load-assistant`
2. [x] - `p1` - **IF** no row is found (a completed turn always has one, so this is corrupt data) - `inst-if-assistant-missing`
   1. [x] - `p1` - Log at `error` with the chat, turn and request ids and **RETURN** 500 `internal` without opening a stream - `inst-return-replay-internal`
3. [x] - `p1` - Set `effective_model` to `chat_turns.effective_model` and `selected_model` to `chats.model`; set `quota_decision` to `allow` when they are equal, otherwise `downgrade` with `downgrade_from` `selected_model`; never set `downgrade_reason` or `quota_warnings` - `inst-rebuild-decision`
4. [x] - `p1` - Open the 200 SSE response with the headers of `cpt-cf-mini-chat-algo-streaming-turns-relay-sse` and send `stream_started` with the turn's `request_id`, `message_id` the assistant message id and `is_new_turn` `false` - `inst-send-replay-started`
5. [x] - `p1` - Send exactly one `delta` with `type` `text` and `content` the full persisted assistant content (also when it is empty) - `inst-send-replay-delta`
6. [x] - `p1` - Send `done` with `usage` the assistant message's `input_tokens` and `output_tokens`, the two models and the rebuilt decision; send no `ping` and no `citations` - `inst-send-replay-done`
7. [x] - `p1` - Use a code path that has no access to the quota reserve, settlement, outbox or provider client: no reserve, no `quota_usage` change, no usage or audit event, no provider request, no metric - `inst-replay-side-effect-free`
8. [x] - `p1` - **RETURN** the stream and close it after `done` - `inst-return-replay-stream`

### Turn Input Loading

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`

**Input**: The loaded chat, the distinct `attachment_ids`, `context.recent_messages_limit`, `rag.max_images_per_message`

**Output**: Turn inputs: image attachments of the request (id, `provider_file_id`, `secondary_file_id`), the image count, `has_ready_file_search_documents`, `has_ready_code_interpreter_files`, the ready code-interpreter `provider_file_id` list, the chat vector store id, the ready-attachment map `provider_file_id` to (`attachment_id`, `filename`), the snapshot boundary, the thread summary, the recent messages; or `TOO_MANY_IMAGES`

**Steps**:
1. [x] - `p1` - **IF** `attachment_ids` is not empty, DB: SELECT `attachments` through Secure ORM (tenant column `tenant_id`) WHERE `id` is in `attachment_ids`, `chat_id` is the chat and `deleted_at IS NULL` - `inst-select-request-attachments`
2. [x] - `p1` - Set the image count to the number of returned rows with `attachment_kind` `image`; unknown, foreign or deleted ids count as no image here and are rejected by the in-transaction check - `inst-count-images`
3. [x] - `p1` - **IF** the image count is greater than `rag.max_images_per_message` (default 4), **RETURN** `TOO_MANY_IMAGES` - `inst-return-too-many-images`
4. [x] - `p1` - DB: SELECT the chat's `attachments` with `status` `ready` and `deleted_at IS NULL` through the `(tenant_id, chat_id)` index - `inst-select-ready-attachments`
5. [x] - `p1` - Set `has_ready_file_search_documents` when one of them has `for_file_search` `true`; set `has_ready_code_interpreter_files` when one has `for_code_interpreter` `true` and a non-NULL `provider_file_id`, and collect those `provider_file_id` values in `created_at`, `id` order; build the map from every non-NULL `provider_file_id` to its `attachment_id` and `filename` - `inst-ready-flags`
6. [x] - `p1` - DB: SELECT `chat_vector_stores` WHERE `tenant_id` is the chat tenant and `chat_id` is the chat (never by `vector_store_id` alone) - `inst-select-vector-store`
7. [x] - `p1` - DB: SELECT the chat's latest `messages` row with `request_id IS NOT NULL` and `deleted_at IS NULL` ordered by `created_at` descending and then `id` descending; its `(created_at, id)` is the snapshot boundary, immutable for the request (no boundary when the chat has no message) - `inst-snapshot-boundary`
8. [x] - `p1` - DB: SELECT `thread_summaries` WHERE `tenant_id` and `chat_id` match; keep `summary_text`, the frontier `(summarized_up_to_created_at, summarized_up_to_message_id)` and `token_estimate` - `inst-select-summary`
9. [x] - `p1` - **IF** a boundary exists and `context.recent_messages_limit` is greater than 0, DB: SELECT `messages` WHERE `chat_id` is the chat, `request_id IS NOT NULL`, `deleted_at IS NULL`, `is_compressed` is `false`, `role` is `user` or `assistant`, `(created_at, id)` is at most the boundary and, only when a summary exists, greater than the frontier; ordered by `created_at` and then `id` descending, limited to `context.recent_messages_limit` rows; reverse the result to chronological order - `inst-select-recent`
10. [x] - `p1` - Run every read of this algorithm before the reserve transaction opens; none of them locks - `inst-reads-before-tx`
11. [x] - `p1` - **RETURN** the turn inputs - `inst-return-inputs`

### Tool Enablement Decision

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`

**Input**: The preflight decision (effective catalog entry, `web_search_allowed`, `code_interpreter_allowed`, snapshot kill switches), the turn inputs, the provider resolution of `knowledge_search.provider_id`, the `knowledge_search` and `context` configuration, the tenant id

**Output**: The ordered tool list, the guard texts, the `metadata.feature` label, the citation map (only with `file_search`), the knowledge-search parameters or none

**Steps**:
1. [x] - `p1` - Start from an empty tool list; the decision uses the effective model's catalog `tool_support`, never the adapter kind (an adapter that drops a tool still gets its guard, surcharge and quota check) - `inst-tool-basis`
2. [x] - `p1` - **IF** `has_ready_file_search_documents`, the effective model's `tool_support.file_search` is `true`, `disable_file_search` is `false` and the chat vector store row has a non-NULL `vector_store_id` - `inst-if-file-search`
   1. [x] - `p1` - Add `file_search` with the chat vector store id and the catalog `max_num_results`, add the `context.file_search_guard` text, and keep the citation map; retrieval always covers the whole chat vector store, `attachment_ids` never filters it - `inst-add-file-search`
3. [x] - `p1` - **IF** the decision's `web_search_allowed` is `true` (the request enabled web search and the effective model supports it; `disable_web_search` was already rejected by the preflight) - `inst-if-web-search`
   1. [x] - `p1` - Add `web_search` with the catalog `web_search_context_size` and add the `context.web_search_guard` text; a request flag on a model without support adds neither tool nor guard - `inst-add-web-search`
4. [x] - `p1` - **IF** the decision's `code_interpreter_allowed` is `true` (ready code-interpreter attachments, `tool_support.code_interpreter`, `disable_code_interpreter` off) - `inst-if-code-interpreter`
   1. [x] - `p1` - Add `code_interpreter` with the container file ids of the turn inputs and no guard; under `disable_code_interpreter` the tool is silently omitted and the stream is not rejected - `inst-add-code-interpreter`
5. [x] - `p1` - **IF** `knowledge_search.enabled` is `true` and `file_search` was not added - `inst-if-knowledge`
   1. [x] - `p1` - **IF** the provider entry named by `knowledge_search.provider_id` has kind `openai_responses` or `anthropic_messages`, an alias for the tenant, a non-empty `api_version`, and the knowledge retriever is configured - `inst-if-knowledge-ready`
      1. [x] - `p1` - Add the `search_knowledge` function tool (parameters `query` required, `top_k` optional) and the `knowledge_search.guard` text, and keep the knowledge-search parameters - `inst-add-knowledge`
   2. [x] - `p1` - **ELSE** log a warning naming the missing item; knowledge search is off for the turn (no tool, no guard) - `inst-knowledge-off`
6. [x] - `p1` - Never add an MCP tool, whatever the catalog `tool_support.mcp` says (`cpt-cf-mini-chat-adr-mcp-deferred`) - `inst-no-mcp`
7. [x] - `p1` - Set the `metadata.feature` label to `none`, or the added built-in tools joined by `+` in the order `file_search`, `web_search`, `code_interpreter` - `inst-feature-label`
8. [x] - `p1` - **RETURN** the tools, the guards in the order `file_search`, `web_search`, `search_knowledge`, the label, the citation map and the knowledge-search parameters - `inst-return-tools`

### Context Assembly and Truncation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-assemble-context`

**Input**: The effective catalog entry (`system_prompt`, `context_window`, `max_input_tokens`, `estimation_budgets`), `max_output_tokens_applied` of the decision, the tools and guards, `content`, the image count, the thread summary, the recent messages

**Output**: The ContextPlan (instructions, optional summary item, kept history in chronological order, current message with its images), `thread_summary_applied`, `assembled_context_tokens`, `messages_truncated`, the effective input budget; or `CONTEXT_BUDGET_EXCEEDED`

**Steps**:
1. [x] - `p1` - Estimate every item with the text formula of `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve` (steps `inst-clamp-ratio`, `inst-base-estimate`, `inst-text-tokens`) over its UTF-8 bytes with the effective model's `estimation_budgets`, and each current image as `image_token_budget`; no provider tokenizer is used - `inst-item-estimate`
2. [x] - `p1` - **IF** `max_output_tokens_applied` is at least `context_window` - `inst-if-no-room`
   1. [x] - `p1` - **RETURN** `CONTEXT_BUDGET_EXCEEDED` - `inst-return-no-room`
3. [x] - `p1` - Set the input limit to `context_window` minus `max_output_tokens_applied`, lowered to `max_input_tokens` when that is greater than 0 and smaller - `inst-input-limit`
4. [x] - `p1` - Set the token budget to the input limit minus `tool_surcharge_tokens` when `file_search` is sent, minus `web_search_surcharge_tokens` when `web_search` is sent, minus `code_interpreter_surcharge_tokens` when `code_interpreter` is sent (none for `search_knowledge`), minus `fixed_overhead_tokens`; **IF** the result is 0 or negative, **RETURN** `CONTEXT_BUDGET_EXCEEDED` - `inst-token-budget`
5. [x] - `p1` - Set the instructions to `system_prompt` followed by the guards, joined by a blank line (the system prompt is configuration and never a `messages` row) - `inst-build-instructions`
6. [x] - `p1` - **IF** the estimates of the instructions, `content` and the current images together exceed the token budget - `inst-if-mandatory-over`
   1. [x] - `p1` - **RETURN** `CONTEXT_BUDGET_EXCEEDED`; the system prompt, the guards, the current message and its images are never truncated - `inst-return-mandatory-over`
7. [x] - `p1` - Set the remaining budget to the token budget minus the mandatory estimates - `inst-remaining`
8. [x] - `p1` - **IF** a summary exists - `inst-if-summary`
   1. [x] - `p1` - Build the summary item as a `user`-role message whose text is the B.5.5 preamble ("This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after."), a blank line and `summary_text`; **IF** its estimate fits the remaining budget, keep it, subtract it and set `thread_summary_applied` to `{ token_estimate: thread_summaries.token_estimate }`; **ELSE** drop it entirely - `inst-keep-summary`
9. [x] - `p1` - **FOR EACH** recent message from the newest to the oldest - `inst-for-each-recent`
   1. [x] - `p1` - **IF** its estimate fits the remaining budget, keep it and subtract it; **ELSE** drop it and every older message, set `messages_truncated`, and stop - `inst-keep-or-drop`
10. [x] - `p1` - **WHILE** the kept history starts with an `assistant` message - `inst-while-leading-assistant`
    1. [x] - `p1` - Drop that message and set `messages_truncated`, so an answer is never sent without its question (whole turns are dropped) - `inst-drop-leading-assistant`
11. [x] - `p1` - Set `assembled_context_tokens` to the sum of the estimates of the instructions, the kept summary, the kept history, `content` and the images; keep it, `messages_truncated` and the effective budget `min(max_input_tokens, context_window - max_output_tokens_applied)` for the thread-summary trigger of `cpt-cf-mini-chat-feature-background-workers` - `inst-assembled-tokens`
12. [x] - `p1` - Order the plan as instructions, summary item, kept history in `(created_at, id)` ascending order, current message with its images; identical inputs and the same boundary always give the same plan - `inst-plan-order`
13. [x] - `p1` - **RETURN** the ContextPlan and the values above - `inst-return-plan`

### Reserve Transaction and Turn Creation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-create-turn`

**Input**: The security context, the chat, the normalized request, the preflight decision, the current time

**Output**: The created turn (turn id, user message id, pre-allocated assistant message id, `started_at`) and the six preflight columns; `replay` with the completed turn row of a concurrent winner; or an error

**Transaction discipline** (normative): see §1.4. The transaction is the reserve transaction of `cpt-cf-mini-chat-algo-quota-billing-reserve-write` in `send` mode, opened through `SeaQuotaRepository::reserve_with`; its first statement is the daily `total` UPSERT, and every statement below runs in its closure after the re-check.

**Steps**:
1. [x] - `p1` - Before the transaction, generate the turn id, the user message id and the assistant message id (UUID v4) and take the current time as `now`; the closure captures them by value and keeps no state across attempts - `inst-pregenerate`
2. [x] - `p1` - Open the reserve transaction write-first and run the reserve write with its limit re-check (see `cpt-cf-mini-chat-algo-quota-billing-reserve-write`); **IF** the re-check fails, the closure is not called and **RETURN** 429 `quota_exceeded` with subject `tokens` - `inst-reserve`
3. [x] - `p1` - DB: INSERT the user `messages` row: the user message id, `tenant_id`, `chat_id`, `request_id`, `role` `user`, `content`, `content_type` `text`, `token_estimate` 0, `request_kind` `chat`, `features_used` `[]`, all token counts 0, `model` NULL, `is_compressed` `false`, `created_at` `now` - `inst-insert-user-message`
4. [x] - `p1` - DB: UPDATE `chats` SET `updated_at` = `now` WHERE `id` is the chat, through the owner-scoped chat scope - `inst-touch-chat`
5. [x] - `p1` - **IF** `attachment_ids` is not empty, DB: SELECT `attachments` WHERE `id` is in the list, `tenant_id` is the request tenant, `chat_id` is the chat and `deleted_at IS NULL` - `inst-select-attachments-tx`
6. [x] - `p1` - **IF** any id has no row, or a row has `uploaded_by_user_id` other than the requesting user, or `status` other than `ready` - `inst-if-attachment-invalid`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` `invalid_attachment` from the closure; the transaction rolls back the reserve and the user message, and no validation relies on a provider failure - `inst-return-attachment-invalid`
7. [x] - `p1` - DB: INSERT one `message_attachments` row per id with `tenant_id`, `chat_id` taken from the user message, `message_id` the user message id, `attachment_id` and `created_at` `now` - `inst-insert-links`
8. [x] - `p1` - DB: INSERT the `chat_turns` row: the turn id, `tenant_id`, `chat_id`, `request_id`, `requester_type` `user`, `requester_user_id` the requesting user, `state` `running`, the six preflight columns returned by the reserve write, `started_at` and `last_progress_at` `now`, `web_search_enabled` the request flag, the three completed counts 0, `updated_at` `now`; `assistant_message_id`, `error_code`, `completed_at` NULL - `inst-insert-turn`
9. [x] - `p1` - Commit; the user message, its attachment links, the running turn, the chat bump and the bucket increments become visible together, before any provider call - `inst-commit-reserve`
10. [x] - `p1` - **IF** any statement of the closure failed with `DomainError::UniqueViolation`, roll back the transaction; nothing is left behind. The repository surfaces every unique-constraint violation of either engine (SQLite `SQLITE_CONSTRAINT_UNIQUE` / `SQLITE_CONSTRAINT_PRIMARYKEY`, PostgreSQL `23505`) as the typed `DomainError::UniqueViolation` of `domain/error.rs` and never inspects the constraint name. The rule is the same whichever index fired: `messages (chat_id, request_id, role)` of the user message INSERT, `chat_turns (chat_id, request_id)`, or the one-running-turn-per-chat index. The engines differ in which one fires first: on PostgreSQL the loser usually blocks on the `messages` index until the winner commits. A unique violation is not a serialization conflict and is not retried. It is resolved here and never reaches the foundation error mapping as `already_exists` - `inst-if-key-race`
    1. [x] - `p1` - DB: re-read outside any transaction: SELECT `chat_turns` through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is the chat and `request_id` is the request value, soft-deleted rows included - `inst-reread-key`
    2. [x] - `p1` - **IF** a turn with that `request_id` exists, apply the idempotency outcome of `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`: **RETURN** `replay` with the row when it is `completed` and `deleted_at IS NULL`, otherwise **RETURN** 409 `request_id_conflict` - `inst-return-key-race`
    3. [x] - `p1` - **ELSE IF** DB: SELECT `chat_turns` WHERE `chat_id` is the chat, `state` is `running` and `deleted_at IS NULL` finds a row - `inst-if-running-race`
       1. [x] - `p1` - **RETURN** 409 `turn_already_running` - `inst-return-running-race`
    4. [x] - `p1` - **ELSE** log at `error` with the chat and request ids and **RETURN** 500 `internal` (the conflicting row is gone or the violation has another cause) - `inst-return-unique-internal`
11. [x] - `p1` - **IF** the transaction failed otherwise (database error, conflict retries exhausted), **RETURN** 500 `internal`; nothing is left behind - `inst-return-tx-error`
12. [x] - `p1` - **RETURN** the created turn - `inst-return-turn`

### Provider Request Construction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`

**Input**: The provider resolution (kind, alias, `api_path`), the effective catalog entry, the ContextPlan, the tools, `max_output_tokens_applied`, the request images with their file ids, the tenant, user and chat ids, the `metadata.feature` label

**Output**: The HTTP request for `proxy_request`, or an internal error

**Steps**:
1. [x] - `p1` - Set the URI to `/{alias}{api_path}` with `{model}` replaced by the entry's `provider_model_id` and the query part of `api_path` kept; method `POST`; headers `Content-Type: application/json` and `Accept: text/event-stream`; never a credential header - `inst-build-uri`
2. [x] - `p1` - Set the `user` value to the simple hex forms of the tenant id and the user id concatenated (64 characters), falling back to `{tenant_id}:{user_id}` only when an id is not a UUID - `inst-user-value`
3. [x] - `p1` - **MATCH** the provider kind - `inst-match-kind`
   - [x] - `p1` - **CASE** `openai_responses`: build the body of the §1.4 "OpenAI Responses wire contract" (`model`, `instructions`, `input`, `tools`, `include`, `max_tool_calls`, `max_output_tokens`, `stream` `true`, `store` `false`, `user`, `metadata`, set sampling parameters, `reasoning.effort`) - `inst-case-openai-responses`
   - [x] - `p1` - **CASE** `vllm_responses`: the same body without `tools`, `include`, `max_tool_calls` and `metadata` (the adapter drops all tools, function tools included) - `inst-case-vllm`
   - [x] - `p1` - **CASE** `openai_chat_completions`: `model`, `messages` (a `system` message with the instructions when not empty, the summary as a `user` message, the history as `user` and `assistant` messages with string content, the current message as a `user` message), `tools` holding only the `search_knowledge` function tool when present (`file_search`, `web_search`, `code_interpreter` are dropped), `stream` `true`, `stream_options` `{ include_usage: true }`, `max_completion_tokens` = `max_output_tokens_applied`, `stop` when non-empty, set sampling parameters, `reasoning_effort` when set, `user` - `inst-case-chat-completions`
   - [x] - `p1` - **CASE** `anthropic_messages`: `model`, `system` (the instructions, omitted when empty), `messages` (summary, history and current message as `user` / `assistant` turns; each current image as an `image` block with source type `file` and the attachment's `secondary_file_id`, dropped with a warning when the image has no secondary copy), `max_tokens` = `max_output_tokens_applied`, `stream` `true`, `metadata` `{ user_id: the user value }`, `tools` (the `web_search` and code-execution server tools for `web_search` and `code_interpreter`, the `search_knowledge` tool; `file_search` is dropped), `stop_sequences` when non-empty, `temperature` and `top_p` when set; headers `anthropic-version` (adapter constant) and the Files API beta header when an image block is present - `inst-case-anthropic`
4. [x] - `p1` - Merge the keys of `general_config.api_params.extra_body` at the top level of the body for every kind except `anthropic_messages`; ignore, with one warning per key, any key the request controls (`model`, `input`, `messages`, `instructions`, `system`, `stream`, `stream_options`, `max_output_tokens`, `max_completion_tokens`, `max_tokens`, `max_tool_calls`, `tools`, `tool_choice`, `include`, `store`, `previous_response_id`, `user`, `metadata`) - `inst-merge-extra-body`
5. [x] - `p1` - Never place an MCP tool, a credential, `tool_choice` or `previous_response_id` in any body - `inst-never-send`
6. [x] - `p1` - **IF** the body cannot be serialized, **RETURN** internal error (the send flow returns 500 before the reserve) - `inst-return-build-error`
7. [x] - `p1` - **RETURN** the request - `inst-return-request`

### Provider Stream Translation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`

**Input**: The provider kind, one parsed provider SSE event (event name, `data`)

**Output**: Zero or more internal events: `text_delta` (text, item id, content index), `reasoning_delta`, `tool_start` (name, details), `tool_done` (name, details), `annotation`, `completed` (usage, response id, incomplete reason or none), `tool_use` (function calls, usage), `provider_error` (code, message, usage or none), or nothing

**Steps**:
1. [x] - `p1` - **IF** the kind is `openai_responses` or `vllm_responses`, take the event name from the SSE `event:` line, or from the `type` field of the `data` JSON when the line is missing or `message` - `inst-event-name`
2. [x] - `p1` - **MATCH** the `openai_responses` event name - `inst-match-responses`
   - [x] - `p1` - **CASE** `response.output_text.delta`: `text_delta` with `data.delta`, `data.item_id`, `data.content_index` - `inst-case-text-delta`
   - [x] - `p1` - **CASE** `response.file_search_call.searching`: `tool_start` `file_search`, details `{}` - `inst-case-fs-start`
   - [x] - `p1` - **CASE** `response.file_search_call.completed`: `tool_done` `file_search`, details `files_searched` = the length of `data.results` (0 when absent) - `inst-case-fs-done`
   - [x] - `p1` - **CASE** `response.web_search_call.searching`: `tool_start` `web_search`, details `{}` - `inst-case-ws-start`
   - [x] - `p1` - **CASE** `response.web_search_call.completed`: `tool_done` `web_search`, details `{}` - `inst-case-ws-done`
   - [x] - `p1` - **CASE** `response.code_interpreter_call.in_progress`: `tool_start` `code_interpreter`, details `{}` - `inst-case-ci-start`
   - [x] - `p1` - **CASE** `response.output_item.done` whose `item.type` is `code_interpreter_call`: `tool_done` `code_interpreter`, details `output` = the `logs` entries of `item.outputs` joined by a newline, cut to 8192 characters with `...[truncated]` appended when longer; any other item type: nothing, except a `function_call` item, which is kept for the `tool_use` outcome - `inst-case-ci-done`
   - [x] - `p1` - **CASE** `response.output_text.annotation.added`: `annotation` with `data.annotation`, `data.item_id`, `data.content_index` - `inst-case-annotation`
   - [x] - `p1` - **CASE** `response.completed`: `completed` with usage from `response.usage` (`input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens` as cache read, `output_tokens_details.reasoning_tokens`, cache write 0; missing fields 0; no usage object means none), `response.id` and, when no annotation event was received, the annotations of the `output_text` parts of `response.output`; when `response.output` holds `function_call` items the outcome is `tool_use` with those calls and the usage instead - `inst-case-completed`
   - [x] - `p1` - **CASE** `response.incomplete`: `completed` with the usage and `response.incomplete_details.reason` as the incomplete reason; never an error code - `inst-case-incomplete`
   - [x] - `p1` - **CASE** `response.failed`: `provider_error` with code and message from `response.error`, falling back to a top-level `error`, and the usage of `response.usage` when present - `inst-case-failed`
   - [x] - `p1` - **CASE** `error`: `provider_error` parsed like `response.failed`, then as a flat `{ code, message }`; data that does not parse becomes the message - `inst-case-error-event`
   - [x] - `p1` - **DEFAULT**: nothing (`response.created`, `response.in_progress`, `response.output_item.added`, `response.content_part.added`, `response.content_part.done`, `response.output_text.done`, `response.code_interpreter_call.interpreting`, `response.code_interpreter_call.completed` and any unknown event) - `inst-case-ignored`
3. [x] - `p1` - **IF** the kind is `vllm_responses`, apply the same table; text inside `<think>` and `</think>` is emitted as `reasoning_delta` (the tags are removed), the rest as `text_delta` - `inst-vllm-think`
4. [x] - `p1` - **IF** the kind is `openai_chat_completions`, read each `data:` JSON chunk: `choices[0].delta.content` gives `text_delta`; `choices[0].delta.tool_calls` entries give `tool_start` `function_call` (details `index`, `call_id`, `name`) on their first fragment and are accumulated; `finish_reason` `stop` marks completion, `length` marks completion with reason `max_tokens`, `content_filter` with reason `content_filter`, `tool_calls` marks a `tool_use` outcome and emits `tool_done` `function_call` (details `call_id`, `name`, `arguments`) per call; the `usage` chunk (`prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens`, `completion_tokens_details.reasoning_tokens`) completes it; `data: [DONE]` ends the stream and releases the pending outcome - `inst-chat-completions`
5. [x] - `p1` - **IF** the kind is `anthropic_messages`, map `message_start` (message id, input and cache usage), `content_block_start` of a `text` block (nothing), of a `server_tool_use` named `web_search` (`tool_start` `web_search`) or `code_execution` (`tool_start` `code_interpreter`), of a `web_search_tool_result` block (`tool_done` `web_search`), of a code-execution result block (`tool_done` `code_interpreter`, details `{}`), of a `tool_use` block (`tool_start` named `search_knowledge`, `load_files` or `unknown_tool` by tool name, no `done`); `content_block_delta` `text_delta` (`text_delta`), `citations_delta` of type `web_search_result_location` (`annotation`); `message_delta` (`stop_reason`, output usage); `message_stop` (`completed`, reason `max_tokens` when `stop_reason` is `max_tokens`, `tool_use` outcome when it is `tool_use`); `error` (`provider_error`); `ping` (nothing); input usage is normalized as stated in §1.4 - `inst-anthropic`
6. [x] - `p1` - **RETURN** the internal events; provider events are never forwarded to clients as such - `inst-return-translated`

### Provider Error Classification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`

**Input**: The `proxy_request` result (a `CanonicalError`, or a response with status, headers, `ErrorSource` extension and body), or a read error of the streamed body

**Output**: A streaming error code and the client message

**Steps**:
1. [x] - `p1` - **IF** `proxy_request` returned an error, or the gateway answered HTTP 504 with its own `deadline_exceeded` `Problem` (error source gateway), or the body read failed with a gateway timeout - `inst-if-gateway-timeout`
   1. [x] - `p1` - **RETURN** `provider_timeout` with the generic timeout text when the cause is a timeout or deadline, otherwise `provider_error` with the generic unavailable text - `inst-return-gateway`
2. [x] - `p1` - **IF** the status is 429 - `inst-if-429`
   1. [x] - `p1` - **RETURN** `rate_limited`; the message is the generic rate-limit text, followed by the delay in seconds when the provider sent a numeric `Retry-After` (OAGW passes the 429 through and never retries) - `inst-return-429`
3. [x] - `p1` - **IF** the status is not 2xx (including a provider's own HTTP 504 with its JSON error body) - `inst-if-http-error`
   1. [x] - `p1` - **RETURN** `provider_error` with the provider's `error.message` (or a flat `message`) of the JSON body sanitized by `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`, or the generic unavailable text when there is none - `inst-return-http-error`
4. [x] - `p1` - **IF** the status is 2xx but `Content-Type` does not start with `text/event-stream`, or the stream ends without a terminal event, or the body read fails for another reason - `inst-if-invalid-stream`
   1. [x] - `p1` - **RETURN** `provider_error` with the generic unavailable text - `inst-return-invalid-stream`
5. [x] - `p1` - **RETURN** `provider_error` with the sanitized message for a parsed provider error event - `inst-return-provider-event`

### Error Message Sanitization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`

**Input**: A provider-originated message text

**Output**: The text safe for the SSE `error.message` and for `error_detail`

**Steps**:
1. [x] - `p1` - Replace every URL (`http://` or `https://` up to the next whitespace) with `[url]`. This includes URLs that carry user info (`user:pass@host`) or a query string, so no credential or signed parameter in a URL survives - `inst-replace-urls`
2. [x] - `p1` - Replace each of the following with `[credential]`: every `Bearer` in any letter case (`Bearer`, `bearer`, `BEARER`, ...) followed by whitespace and a non-space token; and every `sk-` followed by at least 10 characters of `[A-Za-z0-9_-]` (`sk-[A-Za-z0-9_-]{10,}`, which covers `sk-proj-...` keys) - `inst-replace-credentials`
3. [x] - `p1` - Replace the value after every `x-request-id` header name (any letter case, followed by optional `:` or `=`, whitespace and quotes) with `[provider_id]`, up to the next whitespace, quote, comma or semicolon; the header name itself stays - `inst-replace-request-id-header`
4. [x] - `p1` - Replace every UUID (`8-4-4-4-12` hexadecimal digits, any letter case) with `[provider_id]`; this covers UUID request and correlation ids - `inst-replace-uuids`
5. [x] - `p1` - Replace every response or completion id with `[provider_id]`. This means the DESIGN forms (`resp_`, `chatcmpl-`, `cmpl-`, `msg_` followed by letters and digits), plus every prefixed id at a word start, matching `(resp|chatcmpl|cmpl|msg|req|org|proj|call|fc|rs|ws|ci|toolu|file|vs|assistant|thread|run|step)[-_][A-Za-z0-9_-]{6,}` whose tail contains at least one digit. Examples are `req_...`, `org-...`, `proj_...`, `call_...`, `toolu_...`, `thread_...` and `run_...` - `inst-replace-response-ids`
6. [x] - `p1` - Replace every file or vector store id (`file-`, `file_`, `assistant-`, `vs_` followed by at least 12 letters and digits) with `[provider_id]`. Words such as `file-based`, `file_search` or `step-by-step` stay intact, because they miss this step's prefixes or its 12-character floor and have no digit for step 5 - `inst-replace-file-ids`
7. [x] - `p1` - Leave every other character unchanged; **IF** the result is empty, use the generic text of the code - `inst-keep-rest`
8. [x] - `p1` - Sanitize the full provider text before any truncation. The 1024-character cut of `error_detail` (`inst-case-final-failed`) and any length cap of the SSE `message` apply only to the sanitized text, so a cut can never leave a fragment of an identifier that the patterns no longer recognize - `inst-sanitize-before-truncate`
9. [x] - `p1` - **RETURN** the sanitized text. The raw text is only logged, never sent, and no response body, SSE payload or `Problem` `detail` ever carries a provider identifier. The patterns beyond DESIGN §3.3 are the "Sanitizer patterns (widening)" deviation of §1.4 - `inst-return-sanitized`

### Citation Mapping

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-map-citations`

**Input**: The collected annotations with their output text parts, the citation map of the turn (only when `file_search` was sent)

**Output**: The `citations.items` array (possibly empty)

**Steps**:
1. [x] - `p1` - **FOR EACH** annotation in provider order - `inst-for-each-annotation`
   1. [x] - `p1` - **IF** it is a web annotation (`url_citation`, or Anthropic `web_search_result_location`) - `inst-if-web`
      1. [x] - `p1` - Add an item with `source` `web`, `url`, `title`, `snippet` the annotation text when present and non-empty, else the characters from `start_index` to `end_index` of the `output_text` part that carries it (character offsets; a range outside that text gives `""`), and `span` `{ start, end }` only when both indexes are present - `inst-add-web-item`
   2. [x] - `p1` - **IF** it is a file annotation (`file_citation` with `file_id`) - `inst-if-file`
      1. [x] - `p1` - **IF** the citation map has `file_id` (a ready, non-deleted attachment of this chat), add an item with `source` `file`, `attachment_id`, `title` the attachment `filename`, `snippet` `""` and no `span`; **ELSE** omit the annotation; the raw `file_id` is never placed in an item - `inst-add-file-item`
2. [x] - `p1` - Never set `score` - `inst-no-score`
3. [x] - `p1` - **RETURN** the items - `inst-return-items`

### Provider Task

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`

**Input**: The created turn, the preflight decision, the ContextPlan, the tools and knowledge-search parameters, the provider resolution and request, the channel sender, the cancellation token, the request `SecurityContext`

**Output**: Events on the channel and one terminal outcome handed to the finalization

**Steps**:
1. [x] - `p1` - Record `mini_chat_stream_started{provider,model}` and increment `mini_chat_active_streams` (see `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`) - `inst-start-metrics`
2. [x] - `p1` - Race every await of this task that can wait on the provider or on the client against the cancellation token, as a biased select whose cancellation arm is polled first. This covers the `proxy_request` call (step 4), each read of the next provider event (step 6.2), each knowledge-search retrieval call (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`, `inst-retrieve`) and each channel send (steps 6.3 and 8). When the cancellation arm wins, the pending operation is dropped at once, which drops the response body and closes the OAGW upstream connection. The outcome is then set to `cancelled`, unless a provider terminal event (completion, incomplete or error) was already received, in which case that outcome is kept. No await of this task waits on the provider or on channel capacity outside such a race - `inst-race-cancel`
3. [x] - `p1` - **IF** the token is already cancelled before the provider is called (the handler or the relay was dropped after the reserve commit, see `inst-handler-gone`), set the outcome to `cancelled` without calling the provider, with no usage and no accumulated text, and **GO TO** step 9 - `inst-cancelled-before-call`
4. [x] - `p1` - API: `proxy_request` with the request `SecurityContext` and the built request, raced against the token (step 2); the response body is consumed as a stream, event by event - `inst-proxy-request`
5. [x] - `p1` - **IF** the call fails, or the response is not a 2xx `text/event-stream` - `inst-if-call-failed`
   1. [x] - `p1` - Set the outcome to `failed` with the code of `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error` and no usage, and **GO TO** step 9 - `inst-call-failed-outcome`
6. [x] - `p1` - **WHILE** no terminal outcome is set - `inst-while-streaming`
   1. [x] - `p1` - **IF** the cancellation token is cancelled (client disconnect), whether checked at the top of the iteration or won as the cancellation arm of a raced await (step 2), drop the response body and set the outcome to `cancelled`, unless a provider terminal event was already received - `inst-if-cancelled`
   2. [x] - `p1` - Read the next provider SSE event, raced against the token (step 2) and against the liveness timer of step 6.6, and translate it (see `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`). On the first content event, record `mini_chat_ttft_provider_ms`. An OAGW upstream idle or total timeout ends the read with a gateway timeout and gives `provider_timeout` (`inst-if-gateway-timeout`) - `inst-read-event`
   3. [x] - `p1` - **MATCH** the internal event - `inst-match-internal`
      - [x] - `p1` - **CASE** `text_delta`: append the text to the accumulated assistant text and to its output part, send `delta` `text` on the channel immediately (record `mini_chat_ttft_overhead_ms` for the first one), and refresh `last_progress_at` when 30 s passed since the last refresh - `inst-case-relay-text`
      - [x] - `p1` - **CASE** `reasoning_delta`: send `delta` `reasoning`; not accumulated - `inst-case-relay-reasoning`
      - [x] - `p1` - **CASE** `tool_start` of `web_search` or `code_interpreter`: add 1 to that tool's start count and check it (see `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit`); **IF** `exceeded`, drop the response body, do not relay the event and set the outcome to `failed` with `web_search_calls_exceeded` or `code_interpreter_calls_exceeded` and no usage; **ELSE** relay `tool` `start` and refresh progress - `inst-case-limited-start`
      - [x] - `p1` - **CASE** another `tool_start`: relay `tool` `start` and refresh progress - `inst-case-other-start`
      - [x] - `p1` - **CASE** `tool_done`: add 1 to the completed count of `web_search`, `code_interpreter` or `file_search`, persist it in the matching `chat_turns.*_completed_count` with `last_progress_at`, and relay `tool` `done` - `inst-case-tool-done`
      - [x] - `p1` - **CASE** `annotation`: collect it - `inst-case-collect-annotation`
      - [x] - `p1` - **CASE** `completed`: keep the usage, the provider response id and the incomplete reason; set the outcome to `completed` - `inst-case-outcome-completed`
      - [x] - `p1` - **CASE** `tool_use`: run the knowledge-search iteration (see `cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`); **IF** it issues the next provider request, continue the loop on its stream; **ELSE** set the outcome to `failed` with its code (`agentic_iterations_exceeded`, `unexpected_tool_use`) and the usage of the last request - `inst-case-tool-use`
      - [x] - `p1` - **CASE** `provider_error`: set the outcome to `failed` with the code of `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`, the sanitized message and the usage when reported - `inst-case-outcome-error`
   4. [x] - `p1` - **IF** the stream ends without an outcome, set the outcome to `failed` with `provider_error` and no usage - `inst-if-eof`
   5. [x] - `p1` - **IF** a channel send fails because the receiver was dropped, treat it as the client disconnect of step 6.1 - `inst-if-send-failed`
   6. [x] - `p1` - **IF** at least `streaming.sse_ping_interval_seconds` passed since the last liveness check (the liveness timer fired), check whether the turn is still `running`. The check is DB: SELECT `chat_turns.state` WHERE `id` is the turn id. It runs off the relay hot path like the progress writes: at most one check in flight, and a pending check never delays the read or the send of an event. A failed check is logged and retried at the next interval - `inst-check-turn-running`
      1. [x] - `p1` - **IF** the state is no longer `running` (the orphan watchdog or another finalizer already moved the turn), drop the response body and **GO TO** step 10 without finalizing: no CAS attempt, and nothing is settled, persisted, enqueued or sent. The relay then sends `stream_interrupted` when the client is connected - `inst-if-turn-moved`
7. [x] - `p1` - Every channel send waits for capacity (bounded channel, backpressure), raced against the token (step 2); no event is held back, batched or collected into a full response before it is sent - `inst-no-buffering`
8. [x] - `p1` - **IF** the outcome is `completed` without an incomplete reason, map the citations (see `cpt-cf-mini-chat-algo-streaming-turns-map-citations`) and send one `citations` event when the items are not empty - `inst-send-citations`
9. [x] - `p1` - Finalize the turn with the outcome (see `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`) - `inst-finalize`
10. [x] - `p1` - Decrement `mini_chat_active_streams`, and record the stream metrics of the outcome when there is one (see `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`) - `inst-end-metrics`
11. [x] - `p1` - **RETURN**. When the task ends without having sent a terminal event (lost CAS, turn moved by another finalizer, panic), the relay sends `stream_interrupted` - `inst-return-task`

### Knowledge Search Iteration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`

**Input**: The `tool_use` outcome (function calls, usage), the knowledge-search parameters or none, the iteration number, the retrieval count, `knowledge_search.max_calls_per_message`, `knowledge_search.top_k`, `knowledge_search.max_chunk_chars`

**Output**: The next provider request, or a terminal code

**Steps**:
1. [x] - `p1` - **IF** knowledge search is off for the turn, or a call names a function other than `search_knowledge` - `inst-if-unexpected`
   1. [x] - `p1` - **RETURN** `unexpected_tool_use` - `inst-return-unexpected`
2. [x] - `p1` - **IF** the next request would exceed `knowledge_search.max_calls_per_message` plus 2 provider iterations - `inst-if-iterations`
   1. [x] - `p1` - **RETURN** `agentic_iterations_exceeded` - `inst-return-iterations`
3. [x] - `p1` - **FOR EACH** `search_knowledge` call - `inst-for-each-call`
   1. [x] - `p1` - Add 1 to the in-memory knowledge call count (reported as `file_search_calls` in the usage and audit events, failed retrievals included) - `inst-count-call`
   2. [x] - `p1` - **IF** the retrieval count reached `knowledge_search.max_calls_per_message` (default 3), answer the call with a "search limit reached" output and no retrieval - `inst-if-limit-output`
   3. [x] - `p1` - **ELSE** API: `POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version={ver}` through OAGW with `query` and `top_k` capped at `knowledge_search.top_k`, raced against the cancellation token with the cancellation arm first (`inst-race-cancel`; a cancellation drops the retrieval and ends the turn as `cancelled`); trim each chunk to `knowledge_search.max_chunk_chars`; on success add 1 to `file_search_completed_count` (persisted with `last_progress_at`) and record `mini_chat_knowledge_search{result="ok"}`, its latency and chunk count; on failure record `result="error"` and answer the call with a failure output - `inst-retrieve`
4. [x] - `p1` - Append each call and its output to the provider input (`function_call` and `function_call_output` items for `openai_responses`, `tool_use` and `tool_result` blocks for `anthropic_messages`) and build the next request with the same tools - `inst-append-output`
5. [x] - `p1` - **RETURN** the next request; only the usage of the final iteration is settled (`cpt-cf-mini-chat-adr-quota-policy-scope`) - `inst-return-next`

### Stream Finalization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`

**Input**: The created turn, the preflight decision, the chat (`chats.model`), the outcome (`completed` with usage, response id and incomplete reason; `failed` with code, sanitized message and usage or none; `cancelled`), the accumulated text, the tool counts, whether the client is still connected

**Output**: The committed terminal state and the terminal SSE event, or nothing

**Transaction discipline** (normative): see §1.4. The transaction is the finalization transaction of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` (stream variant), opened through `SeaQuotaRepository::finalize_turn_with`; its first statement is the CAS UPDATE, and the statements below run in its closure after the settlement and the usage enqueue.

**Steps**:
1. [x] - `p1` - **MATCH** the outcome - `inst-match-outcome`
   - [x] - `p1` - **CASE** `completed` (also `incomplete`): classification `completed`; terminal columns `state` `completed`, `assistant_message_id` the pre-allocated id, `provider_response_id`, `error_code` NULL (the incomplete reason is only logged and used as the metric `reason`) - `inst-case-final-completed`
   - [x] - `p1` - **CASE** `failed`: classification `failed` with the code; terminal columns `state` `failed`, `error_code`, `error_detail` (provider code and the message sanitized first and then cut to at most 1024 characters, see `inst-sanitize-before-truncate`; or NULL), `provider_response_id` when known, no `assistant_message_id` - `inst-case-final-failed`
   - [x] - `p1` - **CASE** `cancelled`: classification `cancelled`; terminal columns `state` `cancelled`, `assistant_message_id` the pre-allocated id only when the accumulated text is not empty; no usage is passed - `inst-case-final-cancelled`
2. [x] - `p1` - Build the finalization request: variant stream with the turn's `tenant_id`, the preflight period keys and `selected_model` `chats.model`; the usage of the outcome (none for `cancelled`); the tool counts. `web_search` and `code_interpreter` are their in-memory completed counts. `file_search` is selected, never summed: when `search_knowledge` was offered in the turn, it is the in-memory knowledge call count (`inst-count-call`, failed retrievals included); otherwise it is the in-memory count of native `file_search` `tool_done` events (`inst-case-tool-done`). The persisted `chat_turns.file_search_completed_count` is never added to an in-memory count, and finalization uses in-memory counts only; the settlement snapshot the preflight snapshot (no plugin call); `now` - `inst-build-finalize-request`
3. [x] - `p1` - Precompute the assistant message row and the audit payload (see `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`) before the transaction; the closure captures them by value - `inst-precompute`
4. [x] - `p1` - Run the CAS-guarded finalization with the closure (see `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`): CAS on `state` `running`, settlement, usage event, then in the closure - `inst-run-finalize`
   1. [x] - `p1` - **IF** an assistant message id is set, DB: INSERT the assistant `messages` row: that id, `tenant_id`, `chat_id`, the turn's `request_id`, `role` `assistant`, `content` the full accumulated text (possibly empty for a completed turn), `content_type` `text`, `token_estimate` 0, `provider_response_id`, `request_kind` `chat`, `features_used` `[]`, `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens` from the usage (0 when absent), `model` the effective model, `is_compressed` `false`, `created_at` the later of `now` and the user message `created_at` plus 1 microsecond - `inst-insert-assistant`
   2. [x] - `p1` - DB: enqueue the turn audit event to `outbox.audit_queue_name` partitioned by `tenant_id` - `inst-enqueue-audit`
5. [x] - `p1` - **MATCH** the result - `inst-match-finalize-result`
   - [x] - `p1` - **CASE** `lost`: another finalizer (the orphan watchdog, or an earlier terminal path) owns the turn; settle nothing, enqueue nothing, send no terminal event, and **RETURN** - `inst-case-lost`
   - [x] - `p1` - **CASE** `won`: record the settlement metrics (see `cpt-cf-mini-chat-algo-quota-billing-record-metrics`) and `mini_chat_finalization_latency_ms`, and write the `turn finalized` record - `inst-case-won`
   - [x] - `p1` - **CASE** the assistant message INSERT failed on a `completed` outcome: run step 4 again with classification `failed` / `message_persistence_failed`, no message and the same usage; on `won` the terminal event is `error` `message_persistence_failed`, on any error `error` `finalization_failed` - `inst-case-persist-failed`
   - [x] - `p1` - **CASE** the assistant message INSERT failed on a `cancelled` outcome: log at `warn` and run step 4 again as `cancelled` without a message - `inst-case-partial-failed`
   - [x] - `p1` - **CASE** any other error: the turn stays `running` for the orphan watchdog; the terminal event is `error` `finalization_failed` for a `completed` outcome, `error` with the original code for a `failed` outcome, nothing for `cancelled` - `inst-case-finalize-error`
6. [x] - `p1` - **IF** the CAS was won and the committed state is `completed` - `inst-if-done`
   1. [x] - `p1` - Compute `quota_warnings` with the user limits captured by the preflight (see `cpt-cf-mini-chat-algo-quota-billing-quota-view`, warnings mode); **IF** the read fails, log it and omit the key - `inst-compute-warnings`
   2. [x] - `p1` - Build `done` with `usage` `{ input_tokens, output_tokens }` of the provider usage (0 when absent), `effective_model`, `selected_model`, `quota_decision`, `downgrade_from` and `downgrade_reason` when downgraded, and `quota_warnings` - `inst-build-done`
7. [x] - `p1` - **IF** the CAS was won and the committed state is `failed`, build `error` with the code and the sanitized message (the generic text of the code when there is no provider message) - `inst-build-error`
8. [x] - `p1` - **IF** the client is still connected and a terminal event was built, send it on the channel as the last event; the relay closes the stream after it; a `done` is never sent before the commit and never for a state other than `completed` - `inst-send-terminal`
9. [x] - `p1` - **RETURN** the committed state - `inst-return-final`

### Turn Audit Event

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`

**Input**: The turn, the chat, the preflight decision, the outcome, the usage, the tool counts, the latency, `now`

**Output**: One encoded `TurnAuditEvent` payload

**Steps**:
1. [x] - `p1` - Set `event_type` to `turn_completed` for a `completed` turn and `turn_failed` for `failed` and `cancelled` turns (there is no cancelled type) - `inst-audit-type`
2. [x] - `p1` - Fill `tenant_id`, `requester_user_id`, `chat_id`, `turn_id`, `request_id`, `selected_model` (`chats.model`), `effective_model`, `usage` `{ input_tokens, output_tokens }` (0 when unknown), `latency_ms`, `tool_calls` `{ web_search, file_search }` (no code interpreter count; `file_search` is the selected in-memory count of `inst-build-finalize-request`: the knowledge call count when `search_knowledge` was offered, else the native `file_search` done count, never a sum and never the DB column), `policy_decisions.quota.decision` (`allow` or `downgrade` from the preflight) and `timestamp` - `inst-audit-fields`
3. [x] - `p1` - Leave `prompt` and `response` empty, `attachments` an empty array and `license` and `quota_scope` `null`; no content reaches the audit plugin (`cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`) - `inst-audit-empty-content`
4. [x] - `p1` - Encode the event (see `cpt-cf-mini-chat-algo-gear-foundation-encode-events`); delivery to the audit plugin happens later in `cpt-cf-mini-chat-algo-gear-foundation-handle-audit` - `inst-audit-encode`
5. [x] - `p1` - **RETURN** the payload; it is enqueued only by a won finalization, never by replay, a lost CAS or a pre-reserve rejection - `inst-return-audit`

### SSE Relay

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-relay-sse`

**Input**: The channel receiver, the cancellation token, `streaming.sse_ping_interval_seconds`, the `stream_started` payload

**Output**: The HTTP SSE response body

**Steps**:
1. [x] - `p1` - Answer 200 with `Content-Type: text/event-stream` and `Cache-Control: no-cache`, without `Connection: keep-alive`; the route is excluded from response compression and from any body-buffering middleware; each event is flushed as soon as it is written; the HTTP server adds an SSE comment keep-alive every 30 s - `inst-response-headers`
2. [x] - `p1` - Write `stream_started` first - `inst-write-started`
3. [x] - `p1` - **WHILE** no terminal event was written - `inst-while-relay`
   1. [x] - `p1` - Wait for the next channel event; while no `delta` or `tool` was written yet, also wait for the ping timer - `inst-wait-event`
   2. [x] - `p1` - **IF** the ping timer fires first, write `event: ping` with `data: {}` and restart the timer; every written event also restarts it; after the first `delta` or `tool` no `ping` is written - `inst-write-ping`
   3. [x] - `p1` - **IF** an event arrives, write it in arrival order as `event: <name>` and `data: <compact JSON>` - `inst-write-event`
   4. [x] - `p1` - **IF** the event is `done` or `error`, close the response right after it; nothing follows a terminal event - `inst-close-after-terminal`
   5. [x] - `p1` - **IF** the channel closes without a terminal event and the client is connected, write `error` `stream_interrupted` and close - `inst-write-interrupted`
4. [x] - `p1` - **IF** the client disconnects, the dropped body cancels the token (see `cpt-cf-mini-chat-flow-streaming-turns-client-disconnect`) and nothing more is written - `inst-on-drop`
5. [x] - `p1` - **RETURN** - `inst-return-relay`

### Stream Metrics Recording

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`

**Input**: Stream lifecycle points of a turn (start, first content, outcome, disconnect, finalization), the provider id, the effective model id

**Output**: Recorded instruments and the `turn finalized` record

**Steps**:
1. [x] - `p1` - At provider-task start count `mini_chat_stream_started{provider,model}` and add 1 to `mini_chat_active_streams`; subtract 1 when the task ends, whatever the outcome - `inst-metric-start`
2. [x] - `p1` - Record `mini_chat_ttft_provider_ms{provider,model}` and `mini_chat_ttft_overhead_ms{provider,model}` once per turn at the first content event - `inst-metric-ttft`
3. [x] - `p1` - On a committed `completed` turn count `mini_chat_stream_completed{provider,model}`, and additionally `mini_chat_stream_incomplete{provider,model,reason}` for an incomplete response; on a committed `failed` turn count `mini_chat_stream_failed{provider,model,error_code}`; record `mini_chat_stream_total_latency_ms{provider,model}` for every terminal outcome - `inst-metric-outcome`
4. [x] - `p1` - On a disconnect count `mini_chat_cancel_requested{trigger}` (token path only), `mini_chat_stream_disconnected{stage}`, `mini_chat_time_to_abort_ms{trigger}`, and after a won `cancelled` finalization `mini_chat_cancel_effective{trigger}` and `mini_chat_streams_aborted{trigger="client_disconnect"}`; `trigger` is always `disconnect` - `inst-metric-cancel`
5. [x] - `p1` - After the reserve commit record `mini_chat_image_inputs_per_turn` with the image count; after each won finalization record `mini_chat_finalization_latency_ms` - `inst-metric-turn`
6. [x] - `p1` - Record `mini_chat_knowledge_search{result}`, `mini_chat_knowledge_search_latency_ms` and `mini_chat_knowledge_search_chunks` per retrieval - `inst-metric-knowledge`
7. [x] - `p1` - Write the `turn finalized` record per won finalization with chat, turn and request ids, provider id, effective model, terminal state, error code, input and output tokens, web search, file search and code interpreter call counts (the in-memory counts of `inst-build-finalize-request`, with the same `file_search` selection), TTFT and total latency - `inst-metric-cost-record`
8. [x] - `p1` - Use only the labels `provider`, `model`, `error_code`, `reason`, `stage`, `trigger`, `result`; never `tenant_id`, `user_id`, `chat_id`, `request_id`, `provider_response_id` or free text; never record the declared-but-unrecorded `stream_replay`, `tool_calls`, `tool_call_limited`, `citations_count`, `citations_by_source`, `context_truncation`, `provider_requests`, `provider_errors`, `provider_latency_ms`, `tokens_after_cancel`, `time_from_ui_disconnect_to_cancel_ms`, `cancel_orphan`, `web_search_disabled` or `unknown_error_code` - `inst-metric-labels`
9. [x] - `p1` - **RETURN** - `inst-return-metrics`

## 4. States (CDSL)

### Turn Lifecycle State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle`

**States**: `running`, `completed`, `failed`, `cancelled`

**Initial State**: `running`

The `chat_turns` row is created `running` by the reserve transaction, after every pre-provider check passed; a rejected send leaves no row. Every transition is the CAS `WHERE state = 'running'` of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`; the first terminal write wins and terminal states are immutable (`cpt-cf-mini-chat-design-turn-lifecycle`). The billing state of each transition is derived by `cpt-cf-mini-chat-state-quota-billing-turn-billing`.

**Transitions**:
1. [x] - `p1` - **FROM** `running` **TO** `completed` **WHEN** the provider ends with a completion or an incomplete response and the finalization commits the assistant message (possibly empty) with the CAS; `assistant_message_id` is set and `error_code` stays NULL - `inst-to-completed`
2. [x] - `p1` - **FROM** `running` **TO** `failed` **WHEN** the finalization commits a provider error (`provider_error`, `provider_timeout`, `rate_limited`), a tool limit breach, `agentic_iterations_exceeded`, `unexpected_tool_use` or `message_persistence_failed`; no assistant message is persisted - `inst-to-failed`
3. [x] - `p1` - **FROM** `running` **TO** `cancelled` **WHEN** the client disconnected before the provider's terminal event and the finalization commits; the non-empty partial text is persisted as the assistant message, an empty one is not - `inst-to-cancelled`
4. [x] - `p1` - **FROM** `running` **TO** `failed` **WHEN** the orphan watchdog of `cpt-cf-mini-chat-feature-background-workers` wins the CAS of a stalled turn (`orphan_timeout`); the stream path then loses its CAS and the relay sends `stream_interrupted` - `inst-to-orphaned`
5. [x] - `p1` - **FROM** `running` **TO** `running` **WHEN** the finalization transaction fails; the turn is left for the orphan watchdog and the client gets `finalization_failed`, the original error code, or nothing after a disconnect - `inst-stay-running`
6. [x] - `p1` - **FROM** `completed`, `failed` or `cancelled` **TO** the same state **WHEN** a later terminal signal loses the CAS; nothing is persisted, settled, enqueued or sent - `inst-terminal-immutable`

### SSE Stream State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-streaming-turns-sse-stream`

**States**: `awaiting_content`, `streaming`, `closed`

**Initial State**: `awaiting_content`

The stream exists only after the reserve commit (new turn) or after the idempotency check (replay); every earlier failure is a JSON `Problem`.

**Transitions**:
1. [x] - `p1` - **FROM** `awaiting_content` **TO** `awaiting_content` **WHEN** `streaming.sse_ping_interval_seconds` pass after `stream_started` or the last `ping` without another event; a `ping` is written - `inst-ping`
2. [x] - `p1` - **FROM** `awaiting_content` **TO** `streaming` **WHEN** the first `delta` or `tool` is written; no `ping` is written afterwards - `inst-first-content`
3. [x] - `p1` - **FROM** `awaiting_content` or `streaming` **TO** `closed` **WHEN** `done` or `error` is written; the connection is closed immediately - `inst-terminal-written`
4. [x] - `p1` - **FROM** `awaiting_content` or `streaming` **TO** `closed` **WHEN** the channel closes without a terminal event; `error` `stream_interrupted` is written when the client is connected - `inst-interrupted`
5. [x] - `p1` - **FROM** `awaiting_content` or `streaming` **TO** `closed` **WHEN** the client disconnects; nothing is written and the turn is cancelled - `inst-client-gone`
6. [x] - `p1` - **FROM** `closed` **TO** `closed` **WHEN** any later event arrives; it is discarded - `inst-closed-final`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Send Endpoint and Request Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-send-endpoint`

The system **MUST** serve `POST /v1/chats/{id}/messages:stream` as specified in `cpt-cf-mini-chat-flow-streaming-turns-send-message` (`cpt-cf-mini-chat-contract-sse-streaming`, `cpt-cf-mini-chat-fr-chat-streaming`). It accepts the `StreamMessageRequest` body (`content`, `request_id`, `attachment_ids`, `web_search`; no `model`), validates content and attachment ids per `cpt-cf-mini-chat-algo-streaming-turns-validate-request`, authorizes `send_message`, and runs the check order of the §1.4 deviation table. Every failure before the reserve commit is a canonical `Problem` and opens no stream (Option A). The preflight hook validates content, limits and `attachment_ids` (same tenant, uploader and chat, `ready`, no duplicates, count limits, image count and vision of the effective model) before any provider call. The handler runs the setup in a spawned task so a disconnect during setup cannot strand a committed turn. After the reserve commit, the setup task itself records the reserve metrics, creates the channel, the cancellation token and the relay drop guard, and spawns the provider task. The handler only receives the receiver (in its relay handle) and the `stream_started` payload. When the handler is already gone, the dropped handle cancels the token, and the provider task finalizes the turn as `cancelled` without calling the provider.

**Implements**:
- `cpt-cf-mini-chat-flow-streaming-turns-send-message`
- `cpt-cf-mini-chat-algo-streaming-turns-validate-request`
- `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-model-image-capability`, `cpt-cf-mini-chat-constraint-model-locked-per-chat`, `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments` (read), `cpt-cf-mini-chat-dbtable-chat-vector-stores` (read)
- Sequence: `cpt-cf-mini-chat-seq-send-message`
- Entities: Chat turn, Message

### Idempotency, Replay and Parallel-Turn Guard

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-idempotency`

The system **MUST** resolve `(chat_id, request_id)` before the parallel-turn guard as specified in `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`. A completed, non-deleted turn is replayed side-effect-free by a separate code path (`cpt-cf-mini-chat-algo-streaming-turns-replay-completed`): `stream_started` with `is_new_turn` `false`, one `delta` with the full persisted text, `done` with the rebuilt decision, and no provider call, reserve, `quota_usage` change, usage, audit or metric. Any other state, or a soft-deleted turn, is 409 `request_id_conflict`; any `running` turn of the chat is 409 `turn_already_running`. The insert race is resolved by the database with one engine-independent rule. Any unique violation inside the reserve closure arrives as the typed `DomainError::UniqueViolation`, whichever of `messages (chat_id, request_id, role)`, `chat_turns (chat_id, request_id)` or the one-running-turn index fired. It rolls the transaction back and re-reads `(chat_id, request_id)` outside it. The result is `replay` or `request_id_conflict` when a turn with that key exists, otherwise `turn_already_running` when a running turn exists, otherwise 500. A new turn is accepted as soon as the previous one is terminal.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`
- `cpt-cf-mini-chat-algo-streaming-turns-replay-completed`
- `cpt-cf-mini-chat-algo-streaming-turns-create-turn`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages` (read)
- Design: `cpt-cf-mini-chat-design-turn-lifecycle`

### Turn Creation in the Reserve Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-turn-creation`

The system **MUST** create every new turn in the single reserve transaction of `cpt-cf-mini-chat-algo-streaming-turns-create-turn`, opened through `SeaQuotaRepository::reserve_with` after every fallible pre-provider step. The transaction is write-first (the daily `total` UPSERT first, or `IMMEDIATE` on SQLite) and holds the reserve with its re-check, the user message, the in-transaction `attachment_ids` check, the `message_attachments` rows, the `running` `chat_turns` row with the six preflight columns, `started_at` and `last_progress_at`, and the `chats.updated_at` bump. A failed check or a database error rolls everything back. Conflicts re-run the deterministic closure at most 3 attempts in total. The user message is durable before the turn is `running` and before any provider call (DESIGN §5.7 user message persistence invariant), and the reserve metrics are recorded after the commit.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-create-turn`
- `cpt-cf-mini-chat-flow-streaming-turns-send-message`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-quota-usage` (through the quota helper)
- Components: `cpt-cf-mini-chat-component-chat-store`, `cpt-cf-mini-chat-component-quota-service`
- Entities: Chat turn, Message

### Context Assembly and Token Budget

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-context-assembly`

The system **MUST** assemble the ContextPlan as specified in `cpt-cf-mini-chat-algo-streaming-turns-assemble-context` and `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs` (`cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-fr-token-budget`). The order is system prompt with tool guards, thread summary as a `user` message with the B.5.5 preamble, recent messages (`context.recent_messages_limit`, non-compressed, non-deleted, at most the snapshot boundary, after the summary frontier), then the current message with its images. The budget is computed with the effective model after the downgrade. The current message above `max_input_tokens` is 400 `INPUT_TOO_LONG`; mandatory items above the budget are 400 `CONTEXT_BUDGET_EXCEEDED`. Truncation drops the summary when it does not fit, then the oldest whole turns, never the system prompt or the current message. The plan is deterministic for the same boundary, `thread_summary_applied` reports a kept summary, and `assembled_context_tokens` and `messages_truncated` are available to the thread-summary trigger.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-assemble-context`
- `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`

**Constraints**: `cpt-cf-mini-chat-constraint-context-budget`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-messages` (read), `cpt-cf-mini-chat-dbtable-thread-summaries` (read)
- Entities: ContextPlan, ThreadSummary

### Tool Enablement and Guidance

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-tool-enablement`

The system **MUST** decide the tools of every provider request as specified in `cpt-cf-mini-chat-algo-streaming-turns-decide-tools` from the request, the chat's ready attachments, the kill switches and the effective model's `tool_support`, never from the adapter kind. `file_search` is sent only with a ready document and the chat vector store, over the whole store. `web_search` is sent only when the request enables it and the model supports it. `code_interpreter` is sent with the ready XLSX file ids unless the kill switch or the model excludes it, in which case it is silently omitted. `search_knowledge` is sent only when enabled, configured and `file_search` is absent. The guards `context.file_search_guard`, `context.web_search_guard` and `knowledge_search.guard` are appended to the system prompt only with their tools (`cpt-cf-mini-chat-design-web-search`, `cpt-cf-mini-chat-design-code-interpreter`, `cpt-cf-mini-chat-design-code-interpreter-config`). Tool events are reported and counted, the per-turn limits are enforced on start events, and citations are mapped to internal ids only.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`
- `cpt-cf-mini-chat-algo-streaming-turns-map-citations`
- `cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-attachments` (read), `cpt-cf-mini-chat-dbtable-chat-vector-stores` (read), `cpt-cf-mini-chat-dbtable-chat-turns` (completed counts)
- Design: `cpt-cf-mini-chat-design-rag-quality-controls`, `cpt-cf-mini-chat-design-vector-store-scope`, `cpt-cf-mini-chat-design-model-catalog`

### OpenAI Responses Adapter

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-openai-responses-adapter`

The system **MUST** implement the `openai_responses` adapter in the infrastructure `llm_provider` library (`cpt-cf-mini-chat-component-llm-provider`, `cpt-cf-mini-chat-adr-llm-provider-as-library`, `cpt-cf-mini-chat-adr-internal-transport`). It sends the exact request body of the §1.4 "OpenAI Responses wire contract" to `/{alias}{api_path}` through `ServiceGatewayClientV1::proxy_request` (default path `/v1/responses`, Azure `/openai/v1/responses?api-version=...`), with the effective model's `provider_model_id`, the input items, the tools, `max_output_tokens`, `stream` `true` and `store` `false`. It parses the streamed body incrementally, taking event names from the `event:` line or the `type` field, and translates the provider events as specified in `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`. The adapter maps HTTP and gateway failures per `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`; OAGW injects the credentials and the gear never holds a key.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`
- `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`
- `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible`, `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- External: OAGW proxy (`ServiceGatewayClientV1`), provider Responses API
- Components: `cpt-cf-mini-chat-component-llm-provider`

### Other Provider Adapters

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-other-adapters`

The system **MUST** implement the `openai_chat_completions`, `vllm_responses` and `anthropic_messages` adapters per `cpt-cf-mini-chat-adr-multi-provider-adapters`, with the request bodies of `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request` and the translations of `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`, producing the same internal events and therefore the same SSE contract. The Chat Completions adapter keeps only function tools, vLLM drops all tools and emits `<think>` text as `reasoning` deltas, and Anthropic drops `file_search`, maps `web_search` and `code_interpreter` to its server tools and sends `metadata.user_id`. Every adapter sends the `user` identity its protocol supports. The adapter is selected by the provider entry `kind` of the effective model's provider (also after a cross-provider downgrade).

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`
- `cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible`

**Touches**:
- Components: `cpt-cf-mini-chat-component-llm-provider`

### SSE Event Contract and Relay

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-sse-contract`

The system **MUST** emit exactly the event names and `data` shapes of the §1.4 "SSE event contract" in the order `stream_started ping* (delta | tool)* citations? (done | error)` (`cpt-cf-mini-chat-contract-sse-streaming`). It relays each provider event as soon as it is translated, through bounded channels of `streaming.sse_channel_capacity` with backpressure and no buffering in any layer (`cpt-cf-mini-chat-constraint-no-buffering`, `cpt-cf-mini-chat-constraint-bounded-channels`, `cpt-cf-mini-chat-principle-streaming-first`). Pings run only before the first content event, the response headers are `text/event-stream` and `no-cache` without compression, and the connection closes after the single terminal event. `done` exposes usage, models and the quota outcome but never a provider id, `request_id`, message id or settlement detail. `error` is terminal and sanitized.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-relay-sse`
- `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`
- `cpt-cf-mini-chat-state-streaming-turns-sse-stream`

**Constraints**: `cpt-cf-mini-chat-constraint-no-buffering`, `cpt-cf-mini-chat-constraint-bounded-channels`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream` (response)
- Config: `streaming.sse_ping_interval_seconds`, `streaming.sse_channel_capacity`

### Cancellation on Disconnect

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-cancellation`

The system **MUST** cancel the provider call when the client disconnects, as specified in `cpt-cf-mini-chat-flow-streaming-turns-client-disconnect` (`cpt-cf-mini-chat-fr-streaming-cancellation`, `cpt-cf-mini-chat-seq-cancellation`). The dropped relay (or a failed channel send) cancels the token. Every await of the provider task that waits on the provider or the client is raced against the token, cancellation arm first: `proxy_request`, each provider event read, each knowledge-search retrieval and each channel send. As a result, the provider task drops the proxied response at once, even when the provider sends nothing, and this closes the upstream connection through OAGW. A turn whose token is cancelled before the provider call is finalized `cancelled` without a provider request. The provider task stops without finalizing when a liveness check, run at most once per ping interval, finds that the orphan watchdog already moved the turn. The turn is finalized `cancelled` with the non-empty partial text persisted and the `aborted` / `estimated` settlement of the quota helper, and no SSE event is written. A provider terminal event received before the disconnect is finalized as is. There is no other cancellation source. The cancellation metrics are recorded, with `mini_chat_time_to_abort_ms` p99 below 200 ms as the alert threshold.

**Implements**:
- `cpt-cf-mini-chat-flow-streaming-turns-client-disconnect`
- `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- Sequence: `cpt-cf-mini-chat-seq-cancellation`
- Design: `cpt-cf-mini-chat-design-nonterminal-reconciliation`

### Turn Finalization and Persistence

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-finalization`

The system **MUST** finalize every stream turn as specified in `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`, through `SeaQuotaRepository::finalize_turn_with` (stream variant of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`). In one write-first transaction it runs the CAS on `running`, the settlement, the usage event, the assistant message INSERT (completed, or cancelled with text) and the turn audit event. The terminal decision follows `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle`, and the billing outcome is derived from the classification, never from the stored state. `done` is sent only after the commit of a `completed` turn, with `quota_warnings` from the preflight limits. A lost CAS sends no terminal event, a message persistence failure re-finalizes as `message_persistence_failed`, and a failed transaction leaves the turn `running` and reports `finalization_failed` or the original code. A `completed` turn always has a committed assistant message (possibly empty), and the user message precedes the assistant message in `(created_at, id)` order (`cpt-cf-mini-chat-design-quota-usage-accounting`, `cpt-cf-mini-chat-design-terminal-error-reconciliation`).

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`
- `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-quota-usage` (through the quota helper)
- DB: shared outbox tables (`mini-chat.usage_snapshot`, `mini-chat.audit`)
- Components: `cpt-cf-mini-chat-component-chat-service`, `cpt-cf-mini-chat-component-quota-service`
- Entities: Chat turn, Message, AuditEvent

### Turn Status API

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-turn-status`

The system **MUST** serve `GET /v1/chats/{id}/turns/{request_id}` (`cpt-cf-mini-chat-interface-turn-status`) as specified in `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`. It authorizes `read_turn`, maps `running` / `completed` / `failed` / `cancelled` to `running` / `done` / `error` / `cancelled`, and returns exactly `request_id`, `state`, `error_code` (error only), `assistant_message_id` (done, cancelled with partial content) and `updated_at`. A soft-deleted, unknown or foreign turn is 404 with the turn resource type, and provider ids and billing data are never exposed. The UX recovery contract of `cpt-cf-mini-chat-fr-ux-recovery` and `cpt-cf-mini-chat-nfr-resilience-recovery` relies on it.

**Implements**:
- `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`

**Touches**:
- API: `GET /v1/chats/{id}/turns/{request_id}`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (read)

### Error Mapping and Sanitization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-error-mapping`

The system **MUST** report every pre-stream failure of this feature as the canonical `Problem` of `cpt-cf-mini-chat-algo-gear-foundation-map-error` (`cpt-cf-mini-chat-adr-canonical-error-contract`). It **MUST** report every post-open failure as one terminal `error` `{ code, message }` with a streaming error code from §1.4, classified by `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`. Provider-originated text is sanitized by `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error` before it reaches the client or `error_detail`. Quota exhaustion is never an SSE error, a client disconnect never produces one, and the raw provider text is only logged.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`
- `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`
- `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`

**Touches**:
- API: both routes of this feature
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (`error_code`, `error_detail`)

### Turn Audit Events and Request Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-audit-metrics`

The system **MUST** enqueue one `TurnAuditEvent` per won stream finalization in the finalization transaction, as specified in `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event` (`cpt-cf-mini-chat-fr-audit`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`). The system **MUST** record the streaming, cancellation, finalization, image and knowledge-search instruments and the `turn finalized` record of `cpt-cf-mini-chat-algo-streaming-turns-record-metrics` with the low-cardinality labels only (`cpt-cf-mini-chat-fr-cost-metrics`, request group; `cpt-cf-mini-chat-nfr-observability-supportability`; `cpt-cf-mini-chat-nfr-streaming-latency` with `mini_chat_ttft_overhead_ms` p99 below 50 ms).

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`
- `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`

**Touches**:
- DB: shared outbox tables (queue `mini-chat.audit`)
- Observability: `mini_chat_*` meter (instruments registered by `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`)
- Entities: AuditEvent

### MCP Never Injected

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-mcp-absent`

The system **MUST NOT** discover, inject or execute MCP tools in any provider request (`cpt-cf-mini-chat-fr-mcp-tool-discovery`, `cpt-cf-mini-chat-fr-mcp-tool-execution`, `cpt-cf-mini-chat-adr-mcp-deferred`): the tool decision and every adapter ignore `tool_support.mcp`, and the agentic loop handles only `search_knowledge`. Any other tool use ends the turn with `unexpected_tool_use`.

**Implements**:
- `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`
- `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`

**Touches**:
- Design (Future, absent): `cpt-cf-mini-chat-design-mcp-servers`, `cpt-cf-mini-chat-interface-mcp-api`

### REST Registration and OpenAPI

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-rest-routes`

The system **MUST** register, under `url_prefix` and with the base license feature (`cpt-cf-mini-chat-interface-public-api`), `POST {prefix}/v1/chats/{id}/messages:stream` as operation `mini_chat.stream_message`, tag `Mini Chat Messages`, body `StreamMessageRequest`, response 200 `text/event-stream` `MiniChatSseEvent` and the 400, 401, 403, 404, 409, 422, 429, 500 and 503 (`Retry-After`) problem responses. It **MUST** register `GET {prefix}/v1/chats/{id}/turns/{request_id}` as operation `mini_chat.get_turn`, tag `Mini Chat Turns`, response 200 `TurnStatusResponse` and the 400, 401, 403, 404, 500 and 503 problem responses. The stream service, its adapters and the turn status query are added to the shared application services.

**Implements**:
- `cpt-cf-mini-chat-flow-streaming-turns-send-message`
- `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream`, `GET /v1/chats/{id}/turns/{request_id}`

### Cross-Cutting Quality and Verification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-streaming-turns-quality-gates`

The system **MUST** meet these cross-cutting requirements and completion signals:

- **Security**: fail-closed authorization of `send_message` and `read_turn` (deny 403 `AUTHZ_DENIED`, PDP failure 503 `Retry-After: 5`); every chat query owner-scoped and every child query tenant-scoped and bound to the owner-scoped chat (`cpt-cf-mini-chat-nfr-tenant-isolation`, `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`); `attachment_ids` and citation file ids resolved only within the chat; no provider identifier, credential, URL from a provider error, driver message or content in any response, SSE payload, metric label or audit event; credentials only injected by OAGW.
- **Reliability**: every external call bounded (PDP 5 s, policy calls and registry query 10 s, the provider stream by the OAGW upstream timeouts); every reserved turn ends in exactly one terminal state through the CAS, or is left `running` for the orphan watchdog; a disconnect during setup cannot strand a committed turn; replay never touches quota, outbox or provider.
- **Data integrity**: the reserve and finalization transactions follow the transaction discipline of §1.4 (write-first or `IMMEDIATE` on SQLite, every read before the transaction, deterministic closures, bounded conflict retry, no plugin or provider call inside); `completed` implies a committed assistant message; the user message precedes the assistant message; one running turn per chat and one turn per `(chat_id, request_id)` enforced by the database.
- **Observability**: the instruments and the `turn finalized` record of `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`; raw provider errors, unknown incomplete reasons, dropped Anthropic images and knowledge-search configuration gaps are logged with ids, never with message content.
- **Rollback**: the feature adds no migration; removing it removes the two routes and the stream service; existing turns, messages and outbox rows stay valid, and a turn left `running` by a removal is finalized by the orphan watchdog.
- **Test layering**: the pipeline is testable without a real LLM. Unit tests cover request validation, the idempotency matrix, replay event building, the truncation algorithm (summary dropped, oldest whole turns dropped, leading assistant removed, `CONTEXT_BUDGET_EXCEEDED`, `INPUT_TOO_LONG`), the tool decision for every kill switch and `tool_support` combination, each adapter's request body (golden JSON per kind) and translation table (scripted provider SSE per kind), error classification, sanitization patterns, citation mapping and the relay ordering with a paused clock for pings. Service-level tests run the full send flow on SQLite and PostgreSQL with a fake `ServiceGatewayClientV1` that returns scripted SSE bodies, HTTP errors, gateway timeouts and mid-stream stalls, a mock PDP and the static policy and audit plugins. They check the rows, outbox messages, quota buckets and the exact SSE byte stream, including disconnect, CAS-race and finalization-failure cases. End-to-end tests drive the REST routes with an OpenAI-compatible mock LLM server behind a real OAGW upstream provisioned by the gear, which asserts the exact `/v1/responses` request body and streams `response.output_text.delta` ... `response.completed`. No live provider is called.
- **Compile-time gates**: domain code depends on ports (turn repository, provider adapter, OAGW client, retriever), not on SeaORM entities or the OAGW SDK types; `make dylint` passes.
- **Completion signals**: automated tests exist and pass for every owned acceptance item listed in §1.4; `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass; every implemented CDSL instruction of this feature is wrapped in `@cpt-begin:{cpt-id}:p1:inst-{id}` / `@cpt-end:{cpt-id}:p1:inst-{id}` code markers (FULL traceability) and every flow, algorithm, state and definition-of-done ID of this feature appears in code; the checkboxes of this document and the featstatus are set to `[x]` only when their markers and tests exist.

**Implements**:
- `cpt-cf-mini-chat-flow-streaming-turns-send-message`
- `cpt-cf-mini-chat-algo-streaming-turns-create-turn`
- `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Build: `make gear-ci GEAR=mini-chat`, `make dylint`, `cfs validate`

## 6. Acceptance Criteria

- [ ] Principles & Constraints — "Context window budget enforced, for both the input message and the full assembled request": on a model with `max_input_tokens` 1000 and budgets 4 / 100 / 10, a 4000-byte message (estimate 1210) returns 400 `out_of_range` `INPUT_TOO_LONG` while a 2000-byte message (estimate 660) passes, and the 4000-byte message also passes on a model with `max_input_tokens` 0. A catalog whose `max_output_tokens_applied` is at least `context_window`, or whose system prompt plus current message exceed the budget, returns 400 `CONTEXT_BUDGET_EXCEEDED`. In each case no SSE stream is opened, no turn, message or reserve exists, and the mock provider receives nothing. A chat whose history exceeds the budget sends the provider the system prompt, the current message and only the newest whole turns, never an assistant message without its user message, and drops the summary only when it alone does not fit; the budget uses the effective (downgraded) model's `context_window`.
- [ ] Principles & Constraints — "Streaming responses are never buffered before relaying": with the fake OAGW client releasing provider events one at a time and holding the next, each `delta` reaches the SSE client before the next provider event is released; a 1000-delta response is relayed as 1000 `delta` events; with a client that stops reading, the provider task blocks once the channel holds `streaming.sse_channel_capacity` events, and memory does not grow; the response has `Content-Type: text/event-stream` and `Cache-Control: no-cache` and is not compressed.
- [ ] Messages API — "Message count and chronological ordering are tracked correctly across turns": after three completed turns `GET /v1/chats/{id}` reports `message_count` 6 and `GET /v1/chats/{id}/messages` returns user, assistant, user, assistant, user, assistant in `created_at, id` order, each pair sharing its `request_id`, the assistant items carrying `model` (the effective model) and the token counts; a failed turn adds only its user message, a cancelled turn with partial text adds two messages and one without text adds one; a rejected (pre-stream) send adds none; every completed send moves the chat to the top of `GET /v1/chats`.
- [ ] Streaming: Send Message — "Send-message endpoint streams a response, correlated by a request id": a send with a client `request_id` returns 200 `text/event-stream` whose first event is `stream_started` with that `request_id`, a `message_id` and `is_new_turn` `true`; a send without `request_id` gets a server UUID v4 in `stream_started`; the mock's `response.output_text.delta` events arrive as `delta` `{type: text, content}` in order and `response.completed` ends the stream with `done`; the `message_id` of `stream_started` is the id of the persisted assistant message, and `GET /v1/chats/{id}/turns/{request_id}` returns `done` with that `assistant_message_id`.
- [ ] Streaming: Send Message — "Preflight validation (content, attachments, limits) runs before any provider call": `content` `""` or `"   "` gives 400 `EMPTY_CONTENT` (also on a foreign chat); duplicate `attachment_ids`, an id of another chat, of another uploader, a deleted, `pending`, `uploaded` or `failed` attachment, and more ids than `rag.max_documents_per_chat + rag.max_images_per_message` give 400 `invalid_attachment`; five images give 400 `TOO_MANY_IMAGES`; an image with `disable_images` gives 400 `FEATURE_DISABLED` `images`; an image on an effective model without `VISION_INPUT` (also after a downgrade from a vision model) gives 400 `VISION_NOT_SUPPORTED`; `web_search.enabled` with `disable_web_search` gives 400 `FEATURE_DISABLED` `web_search`; a chat model removed from the catalog gives 400 `INVALID_MODEL`. In every case the mock provider receives no request, no stream is opened and no `chat_turns`, `messages`, `message_attachments` or `quota_usage` change remains.
- [ ] Streaming: Send Message — "Assistant message and usage are persisted once a stream completes": after `done` the database holds one `assistant` message with the full concatenated delta text, the effective model, `provider_response_id` and the provider's `input_tokens`, `output_tokens`, cached and reasoning counts; the turn is `completed` with `assistant_message_id` and `completed_at`; exactly one usage outbox message and one audit outbox message exist for the turn; `done` was observed by the client only after these rows were committed (a finalization fault injected before commit yields `error` `finalization_failed` and a `running` turn instead).
- [ ] SSE Event Contract — "Full streaming event contract (start, delta, tool activity, citations, completion, error, keepalive) and its ordering": with a mock that waits 2 intervals before its first delta and `sse_ping_interval_seconds` 5, two `ping` events with `data` `{}` arrive between `stream_started` and the first `delta`, and none afterwards even when the provider pauses later. `response.web_search_call.searching` / `.completed`, `response.file_search_call.searching` / `.completed` and `response.code_interpreter_call.in_progress` plus `response.output_item.done` (`code_interpreter_call`) yield `tool` start / done with the names and `details` of §1.4 (`files_searched`, `output` capped at 8192 characters). A `url_citation` annotation yields one `citations` event after the last `delta` and before `done`. Every stream matches `stream_started ping* (delta|tool)* citations? (done|error)` and is closed after the terminal event. Replay and `response.incomplete` streams carry no `citations`.
- [ ] SSE Event Contract — "Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers": `done` has exactly `usage` `{input_tokens, output_tokens}`, `effective_model`, `selected_model`, `quota_decision`, and for a downgraded turn `downgrade_from` (= `selected_model`) and `downgrade_reason`, plus `quota_warnings` on a completed turn (`[]` when no period warns, an entry with `next_reset` when a period passed the threshold); it has no `request_id`, message id, `provider_response_id`, `resp_` value, credits or settlement field; `response.incomplete` also ends with `done` and a `completed` turn whose `error_code` is NULL.
- [ ] SSE Event Contract — "Error event is terminal and carries a sanitized message": a mock `response.failed` with message "Invalid file file-abc123def456ghi in resp_9xY2 see https://x.example/a with key sk-ABCDEFGHIJ12" yields exactly one `error` with `code` `provider_error` and a `message` containing `[provider_id]`, `[url]`, `[credential]` and none of the raw values, after which the connection closes with no further event. A provider HTTP 429 with `Retry-After: 7` yields `rate_limited` mentioning 7 seconds, and a gateway 504 `deadline_exceeded` yields `provider_timeout`. A stream that ends without a terminal event yields `provider_error`. A third `web_search` start with limit 2 yields `web_search_calls_exceeded`. In each case the turn is `failed` with that `error_code`.
- [ ] Idempotency & Replay — "Replaying a known request id returns the stored result without side effects (no provider call, no quota change)": re-sending a completed `request_id` returns 200 with `stream_started` (`is_new_turn` `false`, `message_id` the persisted assistant id), one `delta` with the full stored text and `done` with the stored usage, the rebuilt `quota_decision` / `downgrade_from` and no `downgrade_reason` or `quota_warnings`; the mock receives no request and `quota_usage`, the outbox tables, `chat_turns`, `messages` and the metrics are unchanged; replay also works after the chat's model was removed from the catalog and while another turn of the chat is running.
- [ ] Idempotency & Replay — "Conflicting reuse of a request id across turn states is rejected consistently": reusing the `request_id` of a `running`, `failed`, `cancelled` or soft-deleted (retried, edited, deleted) turn returns 409 `aborted` `context.reason` `request_id_conflict` with no stream, no provider call and no reserve; two concurrent sends with the same new `request_id` yield one stream and one 409 `request_id_conflict` (not `turn_already_running`). This holds on SQLite and PostgreSQL alike, whichever unique index (`messages`, `chat_turns` key, running-turn index) fires first for the loser, and the loser leaves no reserve, message or turn.
- [ ] Idempotency & Replay — "Replay is checked before the parallel-turn guard": while a turn with `request_id` B is running in the chat (mock holding its stream open), a send reusing the completed `request_id` A replays A, and a send with a new `request_id` C returns 409 `turn_already_running`.
- [ ] Parallel Turn Enforcement — "Only one turn may run per chat at a time": a second send with a different or no `request_id` while a turn is `running` returns 409 `turn_already_running` before any quota step; two concurrent first sends to an idle chat yield exactly one stream and one 409 `turn_already_running` from the running-turn unique index, and the loser leaves no reserve, message or turn; sends to different chats of the same user run in parallel.
- [ ] Parallel Turn Enforcement — "A new turn is accepted once the previous one reaches a terminal state": immediately after a turn's terminal event (`done`, `error`) or after its cancellation is committed, a new send with a new `request_id` is accepted; a turn finalized by the orphan watchdog also unblocks the chat.
- [ ] Turn Lifecycle — "Turn state machine (running → completed / cancelled / failed) is consistent end to end": the turn is `running` with `last_progress_at` set from the reserve commit until finalization; completion gives `completed` / `done`, a provider error `failed` / `error`, a disconnect `cancelled` / `cancelled` in the Turn Status API; a provider completion racing a disconnect produces exactly one terminal state, one usage event and one audit event, and the loser changes nothing; when the watchdog wins the CAS the open stream ends with `error` `stream_interrupted`, also when the mock holds its stream open without sending. In that case the end comes within one `streaming.sse_ping_interval_seconds` of the watchdog commit, the provider task closes the mock's stream, and the stream path writes no second terminal state, usage event or audit event. A finalization failure after retries leaves the turn `running`.
- [ ] Turn Lifecycle — "Partial and null-content cases on cancellation or failure are handled correctly": a disconnect after two deltas closes the provider connection (the mock sees its stream closed), finalizes `cancelled` with the two deltas persisted as the assistant message and `assistant_message_id` in the turn status, settles `aborted` / `estimated` and sends nothing; a disconnect before any delta finalizes `cancelled` with no assistant message and no `assistant_message_id`; a provider error after deltas persists no assistant message; a `response.completed` without any delta finalizes `completed` with an empty assistant message; an injected assistant INSERT failure on completion gives `error` `message_persistence_failed` and a `failed` turn settled on the reported usage.
- [ ] Disconnect after the reserve commit (feature-local): a test hook drops the handler right after the reserve transaction commits and before the setup result is delivered. The turn ends `cancelled` without an assistant message and is settled `aborted` / `estimated`, with exactly one usage event and one audit event (`turn_failed`). The mock provider receives no request, the turn is never left `running`, `mini_chat_active_streams` returns to its prior value, and a following send to the chat with a new `request_id` is accepted.
- [ ] Context Assembly — "System prompt, thread summary, and recent history are assembled and truncated deterministically within budget": the mock receives `instructions` = catalog `system_prompt` (plus the guards of the sent tools), then the summary as a `user` `input_text` starting with the B.5.5 preamble when a summary exists, then at most `context.recent_messages_limit` non-compressed, non-deleted messages after the summary frontier in chronological order, then the current message; messages persisted after the snapshot boundary are excluded; two identical sends against the same chat state produce byte-identical `input` arrays; `stream_started.thread_summary_applied.token_estimate` equals the stored summary `token_estimate` when the summary was kept and is absent otherwise.
- [ ] Context Assembly — "Tool availability and guidance are reflected correctly in the assembled request": with no ready document the request has no `file_search`; with one ready document it has `file_search` with the chat vector store and `max_num_results` and the file-search guard; `web_search.enabled` on a model with `tool_support.web_search` adds `web_search` with the catalog `search_context_size` and the web-search guard, and on a model without support adds neither; a ready XLSX adds `code_interpreter` with its file id and `include` `["code_interpreter_call.outputs"]`, and `disable_code_interpreter` omits it without rejecting the send; enabled knowledge search adds `search_knowledge` only without `file_search`; no request ever carries an MCP tool, `previous_response_id` or `tool_choice`; `metadata.feature` names the built-in tools sent.
- [ ] Error Mapping & Sanitization — "All errors map to the canonical error contract, consistently across REST and streaming" (streaming part): every pre-stream rejection of this feature is an `application/problem+json` `Problem` with the category, status and reason of the foundation mapping table and no SSE stream; every failure after the stream opened is one SSE `error` with a code from the streaming error code list and never a `Problem`; a non-UUID `request_id` path parameter on the turn status route is 400 `invalid_path_params`; a soft-deleted turn is 404 with `gts.cf.core.mini_chat.turn.v1~`.
- [ ] Error Mapping & Sanitization — "Provider-originated error details are sanitized before reaching the client": for each pattern of `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`, a provider message containing it reaches the client and `chat_turns.error_detail` with the pattern replaced by its placeholder. The cases include a `req_…` id (`req_7f3a9b2c1d`), an `sk-proj-…` key (`sk-proj-AbC_12-xyZ987`), a UUID request id (`3f2b8c1e-9a4d-4e7f-b6a1-0c5d2e8f9a7b`), a lower-case `bearer` token, an `x-request-id: abc123` value and a URL with a query string. Meanwhile `file-based`, `file_search` and `step-by-step` stay intact. A provider message longer than 1024 characters with an id spanning character 1024 stores an `error_detail` that holds no fragment of that id; no response body, SSE payload, `Problem` `detail`, audit event or metric label of this feature contains a provider identifier.
- [ ] Web Search (reporting and citations part) — "Web search tool use is reported, cited, accounted, and quota-limited correctly": a turn in which the mock runs one web search emits `tool` `{phase: start, name: web_search, details: {}}` and `{phase: done, name: web_search, details: {}}`, then a `citations` item with `source` `web`, `url`, `title`, the annotation `snippet` and `span` from `start_index` / `end_index`; the turn's `web_search_completed_count` is 1 during the stream and the usage and audit events report 1 web search call; a file citation is returned with the internal `attachment_id` and the filename as `title`, and an unknown or deleted file id is omitted.
- [ ] Knowledge-search call counting (feature-local): a turn with `search_knowledge` offered, in which the mock asks for two searches and the first retrieval fails and the second succeeds, reports `file_search_calls` 2 in the usage event, `tool_calls.file_search` 2 in the audit event and 2 in the `turn finalized` record. Meanwhile `chat_turns.file_search_completed_count` is 1. A turn with native `file_search` and one `response.file_search_call.completed` reports 1, never 2.
- [ ] OpenAI Responses request (feature-local): against the OpenAI-compatible mock behind OAGW, the request goes to `POST /{alias}/v1/responses` with JSON `model` = `provider_model_id`, `input`, `max_output_tokens` = `max_output_tokens_applied`, `stream` `true`, `store` `false`, `user` = 64 hex characters, `metadata.request_type` `chat`, and no extra keys beyond the §1.4 table; `extra_body` keys are merged except the controlled ones; the Chat Completions, vLLM and Anthropic adapters pass their golden-body and scripted-stream tests and produce the same SSE events for equivalent provider streams.
- [ ] Cancellation and audit metrics (feature-local): a disconnect records `mini_chat_cancel_requested{trigger="disconnect"}`, `mini_chat_cancel_effective{trigger="disconnect"}`, `mini_chat_streams_aborted{trigger="client_disconnect"}` and `mini_chat_time_to_abort_ms`. With a mock that holds its stream open without sending any event, the mock sees its stream closed within 200 ms of the client disconnect. This holds both after the response headers and while the `proxy_request` call itself is still pending. The turn is `cancelled` and `mini_chat_active_streams` decrements back to its value before the send; a completed turn records `mini_chat_stream_started`, `mini_chat_stream_completed`, both TTFT histograms and `mini_chat_stream_total_latency_ms` with `provider` and `model` labels only; the audit event of a completed turn is `turn_completed` with the selected and effective models, token usage, latency, web and file search counts, quota decision and empty content fields, and a cancelled turn's is `turn_failed`.
- [ ] Authorization (operations of this feature): the send evaluates `send_message` and the turn status `read_turn`, each with the chat id as `resource.id`; PDP deny returns 403 `AUTHZ_DENIED` and PDP failure 503 with `Retry-After: 5`, both without a stream; a chat of another user or tenant returns 404 on both routes, also when the PDP returns only a tenant predicate.
- [ ] Quality gates: `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass, and every implemented CDSL ID of this feature has `@cpt-begin` / `@cpt-end` markers.
