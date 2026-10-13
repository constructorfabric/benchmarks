---
description: "Turn mutations feature of the mini-chat gear: retry, edit and delete of the latest terminal turn of the requesting user, with deterministic latest-turn selection, NOT_LATEST_TURN and GENERATION_IN_PROGRESS conflicts, the preflight-before-replacement order, the write-first replacement transaction that soft-deletes the old turn and inserts the new running turn and user message with carried-forward attachments and web search flag, setup-failure marking after the commit, thread summary invalidation, mutation audit events and the reuse of the streaming send pipeline."
---

# Feature: Turn Mutations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-turn-mutations-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-turn-mutations`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Retry Turn](#retry-turn)
  - [Edit Turn](#edit-turn)
  - [Delete Turn](#delete-turn)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Mutation Preview](#mutation-preview)
  - [Mutation Input Loading](#mutation-input-loading)
  - [Replacement Pipeline](#replacement-pipeline)
  - [Replacement Transaction](#replacement-transaction)
  - [Turn Delete Transaction](#turn-delete-transaction)
  - [Summary Invalidation](#summary-invalidation)
  - [Mutation Audit Event](#mutation-audit-event)
  - [Replacement History Loading](#replacement-history-loading)
  - [Replacement Reserve](#replacement-reserve)
  - [Setup Failure Marking](#setup-failure-marking)
  - [Mutation Metrics Recording](#mutation-metrics-recording)
- [4. States (CDSL)](#4-states-cdsl)
  - [Turn Visibility State Machine](#turn-visibility-state-machine)
  - [Replacement Turn Setup State Machine](#replacement-turn-setup-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Mutation Eligibility](#mutation-eligibility)
  - [Retry and Edit Through the Send Pipeline](#retry-and-edit-through-the-send-pipeline)
  - [Replacement Transaction and Carry-Forward](#replacement-transaction-and-carry-forward)
  - [Setup Failure After the Commit](#setup-failure-after-the-commit)
  - [Turn Delete](#turn-delete)
  - [Concurrent Mutation Resolution](#concurrent-mutation-resolution)
  - [Summary Invalidation on Mutation](#summary-invalidation-on-mutation)
  - [Replaced Request IDs and Idempotency](#replaced-request-ids-and-idempotency)
  - [Mutation Audit Events and Metrics](#mutation-audit-events-and-metrics)
  - [REST Registration and OpenAPI](#rest-registration-and-openapi)
  - [Cross-Cutting Quality and Verification](#cross-cutting-quality-and-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

This feature lets a user retry, edit or delete the latest turn of a chat (DESIGN §3.9 "Turn Mutation Rules (P1)" `cpt-cf-mini-chat-design-turn-mutations`, "Turn Mutation API Contracts", "Summary Interaction on Turn Mutation", "Audit Events for Turn Mutations"; §3.6 `cpt-cf-mini-chat-seq-send-message` "Retry / edit variant"). It owns `POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH /v1/chats/{id}/turns/{request_id}` and `DELETE /v1/chats/{id}/turns/{request_id}`. It owns the read-only mutation preview: the target turn, its owner, its terminal state, and the deterministic latest-turn selection by `(started_at, id)` with 409 `NOT_LATEST_TURN`. It owns the preflight-before-replacement order for retry and edit, and the write-first replacement transaction. That transaction inserts the new `running` turn first (the one-running-turn index decides concurrent mutations with 409 `GENERATION_IN_PROGRESS`), soft-deletes the old turn with `replaced_by_request_id`, soft-deletes its messages, inserts the new user message (the original content on retry, the new content on edit), copies the non-deleted attachment links and the `web_search_enabled` flag, invalidates a covering thread summary, bumps `chats.updated_at` and enqueues the `turn_retry` / `turn_edit` audit event. After the commit it runs the context assembly, provider resolution and the fill-mode quota reserve of the send pipeline, marks the new turn `failed` (`context_length_exceeded`, `turn_setup_failed`, `quota_exceeded`) when one of them fails, and otherwise streams exactly as `POST /messages:stream` does. Delete soft-deletes the turn and its messages, invalidates a covering summary, enqueues `turn_delete` and answers 204.

The send pipeline is not redefined here. Validation, turn inputs, the tool decision, context assembly, provider request construction, the provider task, the relay, the stream finalization with its audit event, and the turn lifecycle come from `cpt-cf-mini-chat-feature-streaming-turns`. The preflight, the fill-mode reserve write and the CAS-guarded finalization come from `cpt-cf-mini-chat-feature-quota-billing`. Encoding and delivery of the mutation audit events, authorization and error mapping come from `cpt-cf-mini-chat-feature-gear-foundation`. All of them are called by reference.

**Implementation slices** (implement in this order; each slice is independently testable):

| # | Slice | CDSL elements |
|---|-------|---------------|
| 1 | Preview, delete and summary invalidation (no provider call) | `cpt-cf-mini-chat-algo-turn-mutations-preview`, `cpt-cf-mini-chat-algo-turn-mutations-remove-turn`, `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`, `cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`, `cpt-cf-mini-chat-flow-turn-mutations-delete-turn`, `cpt-cf-mini-chat-state-turn-mutations-turn-visibility` |
| 2 | Replacement transaction and carry-forward | `cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs`, `cpt-cf-mini-chat-algo-turn-mutations-replace-turn` |
| 3 | Post-commit setup, fill reserve and setup failures | `cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history`, `cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement`, `cpt-cf-mini-chat-algo-turn-mutations-fail-setup`, `cpt-cf-mini-chat-state-turn-mutations-replacement-setup` |
| 4 | Retry and edit end to end with the stream | `cpt-cf-mini-chat-algo-turn-mutations-run-replacement`, `cpt-cf-mini-chat-flow-turn-mutations-retry-turn`, `cpt-cf-mini-chat-flow-turn-mutations-edit-turn`, `cpt-cf-mini-chat-algo-turn-mutations-record-metrics` |

**Out of scope** (owned by other features): the send pipeline steps reused here, the SSE event contract, replay and the idempotency matrix of `POST /messages:stream` (`cpt-cf-mini-chat-feature-streaming-turns`); quota arithmetic, the preflight cascade, the reserve write with its re-check and the fill mode, settlement and the usage event (`cpt-cf-mini-chat-feature-quota-billing`); the ready-attachment state and the attachment lock on delete (`cpt-cf-mini-chat-feature-attachments`); the message list and the message count, which simply exclude soft-deleted rows (`cpt-cf-mini-chat-feature-chat-crud`); summary generation, the frontier-deleted and base-missing checks of the summary commit, and the orphan watchdog that finalizes a replacement turn left `running` (`cpt-cf-mini-chat-feature-background-workers`). Editing or deleting arbitrary historical messages, branching and multi-version conversations are P2.

### 1.2 Purpose

A user who gets a bad answer, mistyped the question or wants to undo the last exchange needs a way to correct the end of the conversation without breaking its linear history. The mutation must never destroy the previous answer when the replacement cannot even start. It must decide concurrent clicks deterministically, keep the replaced turn for audit, keep the context the model sees consistent with the visible conversation (including a summary that covered the removed turn), and bill and stream the replacement exactly like a new message.

**Requirements**: `cpt-cf-mini-chat-fr-turn-mutations`, `cpt-cf-mini-chat-fr-thread-summary`, `cpt-cf-mini-chat-fr-audit`, `cpt-cf-mini-chat-nfr-tenant-isolation`

`cpt-cf-mini-chat-fr-thread-summary` is covered only for its "Retry, edit and delete" paragraph: the mutation deletes a covering summary and clears `is_compressed` in its transaction; summary generation belongs to `cpt-cf-mini-chat-feature-background-workers`. `cpt-cf-mini-chat-fr-audit` is covered for the three mutation event types; the finalization event of the replacement turn is enqueued by `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`.

**Principles**: `cpt-cf-mini-chat-principle-linear-conversation`, `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Retries or edits the latest terminal turn of an own chat and receives the SSE stream of the replacement; deletes the latest terminal turn; learns the server-generated `request_id` of the replacement from `stream_started` |
| LLM provider (behind OAGW) | Receives the replacement request through the provider task of `cpt-cf-mini-chat-feature-streaming-turns`; in tests an OpenAI-compatible mock server behind OAGW, or a fake `ServiceGatewayClientV1` |
| ToolKit runtime (gear host) | Registers the three routes; runs authentication, license middleware and extractors; hosts the shared outbox that stores the mutation audit events |
| Model policy plugin (via the model-policy gateway) | Supplies the catalog for the chat-model check and, through the quota preflight, the snapshot, kill switches and limits of the replacement |
| Thread summary worker (`cpt-cf-mini-chat-feature-background-workers`) | Commits summaries concurrently with mutations; its frontier-message row lock orders it against the message soft-delete of the mutation |
| Orphan watchdog (`cpt-cf-mini-chat-feature-background-workers`) | Finalizes a replacement turn left `running` by a crash between the mutation commit and the reserve, without settlement (NULL reserve fields) |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-mini-chat-fr-turn-mutations` ("Message Actions (P1 Scope)"), `cpt-cf-mini-chat-fr-thread-summary` ("Retry, edit and delete"), `cpt-cf-mini-chat-fr-audit`, UC-007 `cpt-cf-mini-chat-usecase-retry-turn`, UC-008 `cpt-cf-mini-chat-usecase-edit-turn`, UC-009 `cpt-cf-mini-chat-usecase-delete-turn`
- **Design**: [DESIGN.md](../DESIGN.md) — §2.1 `cpt-cf-mini-chat-principle-linear-conversation`; §2.2 "Model Capability Constraint (Images)" (retry and edit re-send the original images); §3.3 "Streaming Contract" (`stream_started.request_id` is the new turn's id), "Turn Status API" (soft-deleted turn is 404), the `error_code` list (`context_length_exceeded`, `turn_setup_failed`, `quota_exceeded`), REST error mapping (`NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`, `turn_state` / `STATE`, foreign requester 403); §3.6 `cpt-cf-mini-chat-seq-send-message` "Retry / edit variant"; §3.7 `chats.updated_at`, `messages` (partial unique index on live rows), `chat_turns` (`deleted_at`, `replaced_by_request_id`, `web_search_enabled`, "Preflight columns on retry/edit", the latest-turn and one-running-turn indexes), `message_attachments`, `thread_summaries` (frontier semantics); §3.8 per-operation matrix (`retry_turn`, `edit_turn`, `delete_turn`, PDP evaluated once); §3.9 (all of it); §4 "Thread Summary" (summary commit checks the frontier message), "Metrics (OTLP) and Alerts (P1)" (`mini_chat_turn_mutation`, `mini_chat_turn_mutation_latency_ms`); §5.7 "Unstarted retry/edit turn", "Turn Lifecycle" (retry/edit setup failure); §5.8 billing outcome rows of `context_length_exceeded`, `turn_setup_failed`, `quota_exceeded`; §5.6 reserve on retry/edit
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.6
- **Acceptance checklist**: [acceptance-criteria.md](../acceptance-criteria.md) — owned items: Turn Mutations (all four items). The mutation side of Cleanup & Recovery "Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly" (the summary row deleted and `is_compressed` cleared in the mutation transaction) is delivered here; the item itself is owned by `cpt-cf-mini-chat-feature-background-workers`. Authorization "Every operation is scoped to its owner" is covered for the three operations of this feature.
- **ADRs**: `cpt-cf-mini-chat-adr-canonical-error-contract`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`, `cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`
- **Foundation elements reused by reference** (from `cpt-cf-mini-chat-feature-gear-foundation`): request gate `cpt-cf-mini-chat-flow-gear-foundation-request-gate`; PEP `cpt-cf-mini-chat-algo-gear-foundation-authorize` (actions `retry_turn`, `edit_turn`, `delete_turn`); chat-scoped access `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`; DomainError to `Problem` mapping `cpt-cf-mini-chat-algo-gear-foundation-map-error`; plugin resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`; provider resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`; outbox queues `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`; event encoding `cpt-cf-mini-chat-algo-gear-foundation-encode-events` (`inst-encode-mutation-audit`); audit delivery `cpt-cf-mini-chat-algo-gear-foundation-handle-audit`; metric instruments `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`; configuration `cpt-cf-mini-chat-algo-gear-foundation-load-config` (`outbox.audit_queue_name`, `rag`, `context`, `streaming`); schema `cpt-cf-mini-chat-dod-gear-foundation-schema`
- **Quota elements reused by reference** (from `cpt-cf-mini-chat-feature-quota-billing`): the quota steps of a turn `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps` (`inst-return-reserve-rejected`, retry and edit branch); preflight `cpt-cf-mini-chat-algo-quota-billing-preflight`; text estimate `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`; reserve write in fill mode `cpt-cf-mini-chat-algo-quota-billing-reserve-write` (`inst-if-fill-mode`, `inst-fill-turn`, `inst-if-fill-missed`, `inst-retry-reserve`); CAS-guarded finalization `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` (its `inst-if-no-reserve` branch for an unreserved replacement turn); billing state `cpt-cf-mini-chat-state-quota-billing-turn-billing`; quota metrics `cpt-cf-mini-chat-algo-quota-billing-record-metrics`
- **Streaming elements reused by reference** (from `cpt-cf-mini-chat-feature-streaming-turns`): the send flow `cpt-cf-mini-chat-flow-streaming-turns-send-message` (setup task `inst-spawn-setup`, chat-model check `inst-check-chat-model`, `INPUT_TOO_LONG` check `inst-if-input-too-long`, image guards `inst-image-guards`, and the post-reserve steps `inst-record-reserve` to `inst-return-stream`); request validation `cpt-cf-mini-chat-algo-streaming-turns-validate-request`; turn inputs `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`; tool decision `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`; context assembly `cpt-cf-mini-chat-algo-streaming-turns-assemble-context`; turn creation `cpt-cf-mini-chat-algo-streaming-turns-create-turn` (its id pre-generation, transaction discipline and unique-violation typing; its send-mode statements are not used); provider request `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`; provider task `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`; relay `cpt-cf-mini-chat-algo-streaming-turns-relay-sse`; finalization `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`; turn audit event `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`; idempotency `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`; replay `cpt-cf-mini-chat-algo-streaming-turns-replay-completed`; turn status `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`; turn lifecycle `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle`; stream metrics `cpt-cf-mini-chat-algo-streaming-turns-record-metrics`
- **Chat CRUD elements reused by reference** (from `cpt-cf-mini-chat-feature-chat-crud`): message list `cpt-cf-mini-chat-flow-chat-crud-list-messages` and `message_count` of `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`, which exclude soft-deleted messages; the chat ordering by `updated_at` of `cpt-cf-mini-chat-algo-chat-crud-paginate`
- **Dependencies**: `cpt-cf-mini-chat-feature-streaming-turns`, `cpt-cf-mini-chat-feature-attachments`

**Source precedence**: DESIGN is authoritative for details. Where DESIGN delegates schemas to the generated OpenAPI (`docs/api/api.json`), the schemas `EditTurnRequest`, `MiniChatSseEvent` and `Problem` and the operation ids `mini_chat.retry_turn`, `mini_chat.edit_turn`, `mini_chat.delete_turn` apply. The gear docs win over `config/mini-chat.yaml` and platform docs (DECOMPOSITION task-level overrides).

**REST contract** (paths under `{url_prefix}`, default `/mini-chat`; every route requires authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`; PEP resource type `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, `require_constraints` `true`, `resource.id` the chat id, evaluated once per request; both path ids must be UUIDs, otherwise 400 `invalid_path_params`):

| Method and path | PEP action | Request | Success | Domain errors (besides gate, extractor and PEP errors), all as `Problem` before any stream |
|---|---|---|---|---|
| `POST /v1/chats/{id}/turns/{request_id}/retry` | `retry_turn` | no body (a sent body is ignored) | 200, `text/event-stream` (the `POST /messages:stream` contract) | 400 `failed_precondition` `turn_state` / `STATE`, `INVALID_MODEL`, `TOO_MANY_IMAGES`, `INPUT_TOO_LONG`, `FEATURE_DISABLED` (`web_search`, `images`), `VISION_NOT_SUPPORTED`, `CONTEXT_BUDGET_EXCEEDED` (after the commit); 403 `AUTHZ_DENIED` (foreign requester); 404 chat or turn; 409 `NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`; 429 `quota_exceeded` (`tokens`, `web_search`, `code_interpreter`); 500 `internal` |
| `PATCH /v1/chats/{id}/turns/{request_id}` | `edit_turn` | JSON `EditTurnRequest` `{ "content": string }` (required) | 200, `text/event-stream` | as retry, plus 400 `EMPTY_CONTENT`; a body that does not match the schema is 422 `invalid_json_body`, malformed JSON 400 `json_syntax_error`, a non-JSON `Content-Type` 415 |
| `DELETE /v1/chats/{id}/turns/{request_id}` | `delete_turn` | none | 204, no body | 400 `turn_state` / `STATE`; 403 `AUTHZ_DENIED` (foreign requester); 404 chat or turn; 409 `NOT_LATEST_TURN`; 500 `internal` |

Every 409 is category `aborted` with the reason in `context.reason` and resource type `gts.cf.core.mini_chat.turn.v1~`; the 400 `turn_state` is category `failed_precondition` with `violations[{subject: turn_state, type: STATE}]`; the turn 404 has `context.resource_type` `gts.cf.core.mini_chat.turn.v1~` and the chat 404 the chat type. The client never supplies the new `request_id`: the server generates a UUID v4 and returns it in `stream_started.request_id`; `stream_started.is_new_turn` is `true` and `stream_started.message_id` is the pre-allocated assistant message id of the replacement.

**Check order** (normative for this feature):

| # | Retry and edit | Delete | Leaves the previous turn intact |
|---|----------------|--------|---------------------------------|
| 1 | Edit only: `EMPTY_CONTENT` (pure body check, before authorization) | — | yes |
| 2 | PEP `retry_turn` / `edit_turn` (403, 503) | PEP `delete_turn` (403, 503) | yes |
| 3 | Chat load (404 chat) | Chat load (404 chat) | yes |
| 4 | Mutation preview: 404 turn, 403 foreign requester, 409 `NOT_LATEST_TURN` (soft-deleted target), 400 `turn_state` (running target), 409 `NOT_LATEST_TURN` (a newer live turn) | same preview | yes |
| 5 | Chat-model check `INVALID_MODEL`, turn inputs `TOO_MANY_IMAGES`, quota preflight (400 `FEATURE_DISABLED` `web_search`, 429), `INPUT_TOO_LONG`, image guards (`FEATURE_DISABLED` `images`, `VISION_NOT_SUPPORTED`) | — | yes |
| 6 | Replacement transaction: 409 `GENERATION_IN_PROGRESS`, 409 `NOT_LATEST_TURN`, 500 | Delete transaction: 409 `NOT_LATEST_TURN`, 500 | yes (rolled back) |
| 7 | After the commit: history load, tools, context assembly (400 `CONTEXT_BUDGET_EXCEEDED`, turn `failed` / `context_length_exceeded`), provider resolution and request building (500, `turn_setup_failed`), fill reserve (429 `quota_exceeded` `tokens`, turn `failed` / `quota_exceeded`; 500, `turn_setup_failed`) | — | no: the previous turn is already replaced |
| 8 | 200 SSE stream (provider task, relay and finalization of `cpt-cf-mini-chat-feature-streaming-turns`) | 204 | no |

**DB writes** (exact; every table is accessed through Secure ORM with the tenant predicate and bound to the owner-scoped chat):

| Operation | Transaction | Table and statement | Values |
|-----------|-------------|---------------------|--------|
| Retry, edit | Replacement (first statement) | INSERT `chat_turns` | new turn id (UUID v4), `tenant_id` the chat tenant, `chat_id`, `request_id` the new UUID v4, `requester_type` `user`, `requester_user_id` the caller, `state` `running`, the six preflight columns NULL, `provider_name`, `provider_response_id`, `assistant_message_id`, `error_code`, `error_detail`, `deleted_at`, `replaced_by_request_id`, `completed_at` NULL, `started_at` and `last_progress_at` and `updated_at` the mutation time, `web_search_enabled` the old turn's value, the three completed counts 0 |
| Retry, edit | Replacement | UPDATE `chat_turns` (old turn) | `deleted_at` and `updated_at` the mutation time, `replaced_by_request_id` the new `request_id`; guarded `WHERE id` is the old turn, `deleted_at IS NULL` and `state` is `completed`, `failed` or `cancelled`; `state`, `assistant_message_id`, `error_code`, the preflight columns and the counts stay unchanged |
| Retry, edit, delete | Replacement or delete | UPDATE `messages` (old turn) | `deleted_at` the mutation time WHERE `chat_id` is the chat, `request_id` is the old `request_id` and `deleted_at IS NULL` (the user message and, when present, the assistant message) |
| Retry, edit | Replacement | INSERT `messages` (new user message) | new id, `tenant_id`, `chat_id`, `request_id` the new id, `role` `user`, `content` the original user content (retry) or the edit `content` as sent (edit), `content_type` `text`, `token_estimate` 0, `request_kind` `chat`, `features_used` `[]`, all token counts 0, `provider_response_id` and `model` NULL, `is_compressed` `false`, `created_at` the mutation time |
| Retry, edit | Replacement | INSERT `message_attachments` | one row per link of the old user message whose attachment row has `deleted_at IS NULL`: `tenant_id`, `chat_id`, `message_id` the new user message, `attachment_id`, `created_at` the mutation time; the old links stay on the soft-deleted message |
| Retry, edit, delete | Replacement or delete | DELETE `thread_summaries`; UPDATE `messages` SET `is_compressed` `false` | only when the summary frontier is at or after the old user message `(created_at, id)`; the UPDATE covers every message of the chat with `is_compressed` `true`, soft-deleted rows included |
| Retry, edit | Replacement | UPDATE `chats` | `updated_at` the mutation time (owner-scoped chat scope); delete does not bump `updated_at` |
| Retry, edit, delete | Replacement or delete | Outbox enqueue to `outbox.audit_queue_name` | one `turn_retry`, `turn_edit` or `turn_delete` payload, partitioned by `tenant_id` |
| Retry, edit | Reserve (after the commit) | `quota_usage` UPSERTs, then UPDATE `chat_turns` (new turn) | the reserve write in fill mode: the six preflight columns WHERE `id` is the new turn, `state` `running` and `reserve_tokens IS NULL` |
| Retry, edit | Setup failure (after the commit) | UPDATE `chat_turns` (new turn) | `state` `failed`, `error_code` (`context_length_exceeded`, `turn_setup_failed` or `quota_exceeded`), `completed_at` and `updated_at` now WHERE `id` is the new turn and `state` is `running`; no settlement, no usage event, no audit event, no assistant message |
| Retry, edit | Finalization | as `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream` | the CAS, the settlement, the usage event, the assistant message and the `turn_completed` / `turn_failed` audit event of the new turn |

No mutation changes a `quota_usage` row of the old turn, re-settles it, or enqueues a usage event for it; its committed usage stays as it was.

**Transaction discipline** (normative, shared with `cpt-cf-mini-chat-algo-streaming-turns-create-turn` and `cpt-cf-mini-chat-algo-quota-billing-reserve-write`): every read that decides the mutation outside the transaction (chat, target turn, latest turn, original user message, carried links, attachment state, policy, preflight) runs before the transaction opens and is re-checked inside it by guarded writes. The replacement transaction is write-first: its first statement is the INSERT of the new `running` turn, so on SQLite it takes the write lock before any read (or the transaction is begun `IMMEDIATE`) and never upgrades from a read lock. On PostgreSQL that INSERT waits on the one-running-turn unique index for a concurrent uncommitted running turn of the chat and fails with a unique violation once that turn commits. The delete transaction is write-first with the guarded UPDATE of the target turn as its first statement, which takes the row lock on PostgreSQL. Every later statement runs in the order of the algorithm. The guarded UPDATE of the old turn and the latest-turn re-check replace the `SELECT FOR UPDATE` of DESIGN §3.9 rule 7 with an equivalent: the guard and the row lock make two concurrent mutations of the same turn serialize, and the second one finds the turn already soft-deleted. Both closures are deterministic: ids (new turn, new `request_id`, new user message, pre-allocated assistant message), the mutation time, the content and the audit payload are computed before the transaction and captured by value, because a lock or serialization conflict (`SQLITE_BUSY`, PostgreSQL `40001` / `40P01`) re-runs the whole transaction, at most 3 attempts in total with the 20 ms / 100 ms backoff. A unique violation is not a conflict and is not retried. A transaction that exhausts its attempts or fails otherwise is 500 `internal` with nothing left behind. The fill-mode reserve after the commit is the reserve transaction of `cpt-cf-mini-chat-algo-quota-billing-reserve-write`, opened through `SeaQuotaRepository::reserve_with` (first statement the daily `total` UPSERT), with no statement of this feature in its closure besides the fill. No plugin call and no provider call runs inside any of these transactions.

**Replay and idempotency interplay**: the path `request_id` of a mutation names the target turn; it is not an idempotency key and the mutation never runs `cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`. The replaced or deleted turn keeps its `request_id` in `chat_turns` (`UNIQUE (chat_id, request_id)` covers soft-deleted rows), so that key stays taken forever: `POST /messages:stream` with it returns 409 `request_id_conflict` (`inst-return-key-conflict`), `GET /turns/{request_id}` returns 404 with the turn type, and a retry, edit or delete of it returns 409 `NOT_LATEST_TURN`. Its messages leave the partial unique index `messages (chat_id, request_id, role)` (live rows only), which the new messages never collide with because the new `request_id` is fresh. The replacement turn is an ordinary turn of `cpt-cf-mini-chat-feature-streaming-turns`: once completed, `POST /messages:stream` with its server-generated `request_id` replays it side-effect-free, and it can itself be retried, edited or deleted while it is the latest terminal turn. Repeating a retry is not idempotent: a second retry of the same old `request_id` after the first committed returns `NOT_LATEST_TURN`, and a retry of the replacement creates another replacement.

**Feature-local deviations and interpretations** (review owner: mini-chat gear maintainers):

| Topic | Deviation or interpretation | Rationale |
|-------|-----------------------------|-----------|
| Statement order of the replacement transaction | The new `running` turn is inserted first, then the old turn is soft-deleted with a guard, then the latest turn is re-checked; DESIGN §3.9 rule 7 lists the checks first and names `SELECT FOR UPDATE` "or equivalent" | The first statement is a write (write-first on SQLite without `IMMEDIATE`), and the one-running-turn index decides the concurrent race exactly as rule 7 and the error table describe ("lost the insert race on the one-running-turn-per-chat index" is `GENERATION_IN_PROGRESS`). The guarded UPDATE plus the in-transaction latest check is the equivalent of the row lock |
| Which conflict a concurrent loser gets | A mutation whose transaction runs while another mutation's (or a send's) new turn of the chat is `running` gets 409 `GENERATION_IN_PROGRESS`; a mutation whose preview or transaction runs after the winner committed and the winner's turn is no longer `running` gets 409 `NOT_LATEST_TURN` | DESIGN rule 1 (`GENERATION_IN_PROGRESS` only from the running-turn index), rule 9 (a soft-deleted target is `NOT_LATEST_TURN`); each outcome is determined by the committed state the loser meets |
| `INPUT_TOO_LONG` position | The `INPUT_TOO_LONG` check of the (original or edited) content against the effective model runs before the commit, after the preflight, like the image guards | It needs only the content and the preflight decision; DESIGN rule 8 lists the pre-commit rejections with "...", and a rejection before the commit keeps the previous answer |
| `EMPTY_CONTENT` position | The edit body check runs before authorization, also on a foreign chat | Same rule as `cpt-cf-mini-chat-feature-streaming-turns` for `messages:stream`: pure body checks need no PDP or database call |
| Foreign requester | Checked right after the target turn is loaded, before the deleted and state checks. In P1 a chat has one owner and every user turn of an owner-scoped chat has that owner as `requester_user_id`, so the branch is reached only by a system turn or corrupt data; another user's chat is 404 | DESIGN rule 2 and the error table (403 `AUTHZ_DENIED`); the chat scope already hides foreign chats |
| Mutation time | The mutation time is the clock time, raised to 1 microsecond after the later of the old turn's `started_at` and its user message `created_at` when the clock is not past them | The new turn must sort after the old one by `(started_at, id)` and the new user message after the remaining history by `(created_at, id)`, also with clock skew between pods |
| Carried attachments | The links are copied from the old user message's `message_attachments` rows, filtered by `attachments.deleted_at IS NULL` inside the transaction; no status filter (a `ready` attachment never leaves `ready`), no ownership re-check (the original send checked it) | DESIGN "Deleted attachment handling"; the copy set read in the transaction is also the set whose images are re-sent |
| History of the replacement | After the commit, the snapshot boundary is the latest live message with a non-NULL `request_id` other than the new `request_id`; the summary and the recent messages are read after the commit | The new user message is the current message and must not appear in the history; a summary deleted by the mutation must not be used; DESIGN step 4 runs context assembly after the commit |
| Reserve of the replacement | The 0004 `create-turn` statements (user message, attachment check, links, turn INSERT, chat bump) are not run; only the fill-mode reserve write runs after the commit | The mutation transaction already wrote those rows; DESIGN "Preflight columns on retry/edit" |
| Setup failure mark | A failure after the commit and before the hand-off marks the new turn `failed` with a plain guarded UPDATE (no CAS-guarded finalization helper); when that UPDATE itself fails, the error is logged, the turn stays `running` for the orphan watchdog, and the original error is still returned | DESIGN §5.7 "Unstarted retry/edit turn": no reserve exists, so there is nothing to settle and no outbox event |
| Error code of a failed setup step | `CONTEXT_BUDGET_EXCEEDED` gives `context_length_exceeded`; the reserve re-check gives `quota_exceeded`; every other failure (history read, provider resolution, request building, a reserve database error, a fill that updates no row) gives `turn_setup_failed` | DESIGN §3.6 step 4 and the `error_code` list |
| Delete and `chats.updated_at` | Turn delete does not bump `chats.updated_at` | DESIGN §3.7 lists create, rename, chat delete, send, retry and edit only |
| Mutation metric labels | `mini_chat_turn_mutation{op,result}` uses `result` `ok`, `not_latest_turn`, `generation_in_progress`, `not_terminal`, `rejected` (every other refusal before the commit), `setup_failed` (a failure after the commit) and `error` (500 before the commit); `mini_chat_turn_mutation_latency_ms{op}` runs from the handler accepting the request to the mutation commit and is recorded for committed mutations only | DESIGN names the instruments and `op` but not the `result` values |

**External-call timeouts** (foundation and quota constants, reused unchanged):

| Constant | Value | Call in this feature | Outcome on timeout |
|----------|-------|----------------------|--------------------|
| `PDP_EVALUATION_TIMEOUT` | 5 s | PEP evaluation of `retry_turn`, `edit_turn`, `delete_turn` | 503 `service_unavailable`, `Retry-After: 5`; nothing changes |
| `REGISTRY_QUERY_TIMEOUT` | 10 s | Model-policy plugin resolution of the chat-model check and of the preflight | 500 `internal`; the previous turn stays |
| `POLICY_PLUGIN_CALL_TIMEOUT` | 10 s | `get_current_policy_version`, `get_policy_snapshot` (chat-model check) and the preflight calls | 500 `internal`; the previous turn stays |
| OAGW upstream timeouts (OAGW configuration) | per upstream | The provider request of the replacement turn (provider task of `cpt-cf-mini-chat-feature-streaming-turns`) | SSE `error` `provider_timeout`, the replacement turn is `failed` |

**Audit and metrics**: the feature enqueues one `TurnMutationAuditEvent` per committed mutation inside the mutation transaction (`cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`). The replacement turn additionally gets the finalization `TurnAuditEvent` of `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event` when its stream is finalized; a replacement that fails setup after the commit gets no finalization audit event. The feature records `mini_chat_turn_mutation` and `mini_chat_turn_mutation_latency_ms` (`cpt-cf-mini-chat-algo-turn-mutations-record-metrics`); the stream and quota instruments of the replacement are recorded by the reused algorithms.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-mini-chat-usecase-retry-turn`, `cpt-cf-mini-chat-usecase-edit-turn`, `cpt-cf-mini-chat-usecase-delete-turn`

### Retry Turn

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-retry-turn`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A retry of the latest terminal turn (`completed`, `failed` or `cancelled`) of an own chat opens a 200 SSE stream whose `stream_started` carries a new server-generated `request_id` and `is_new_turn` `true`. The old turn and its messages are soft-deleted with `replaced_by_request_id` set, a new user message with the original content and the non-deleted original attachments is inserted, and the new turn runs with the original `web_search_enabled` flag through the send pipeline. The `turn_retry` audit event is committed with the replacement.

**Error Scenarios**:
- Gate and PEP errors: 401, 403 `LICENSE_FEATURE_REQUIRED`, 403 `AUTHZ_DENIED`, 503 `Retry-After: 5`, 400 `invalid_path_params`.
- Before the commit (the previous turn stays): 404 chat or turn, 403 foreign requester, 409 `NOT_LATEST_TURN`, 400 `turn_state`, 400 `INVALID_MODEL`, `TOO_MANY_IMAGES`, `FEATURE_DISABLED`, `INPUT_TOO_LONG`, `VISION_NOT_SUPPORTED`, 429 `quota_exceeded`, 409 `GENERATION_IN_PROGRESS`, 500.
- After the commit (the previous turn is replaced, the new turn is `failed`): 400 `CONTEXT_BUDGET_EXCEEDED`, 429 `quota_exceeded` `tokens`, 500 `internal`.

**Steps**:
1. [x] - `p1` - User calls `POST {url_prefix}/v1/chats/{id}/turns/{request_id}/retry` through the request gate (see `cpt-cf-mini-chat-flow-gear-foundation-request-gate`); the path extractor requires both ids to be UUIDs; no body is read - `inst-call-retry`
2. [x] - `p1` - The handler runs steps 3 to 6 in a separately spawned setup task and awaits only its result, as the send handler does (see `cpt-cf-mini-chat-flow-streaming-turns-send-message`, `inst-spawn-setup`); a client disconnect therefore never interrupts a started transaction and never strands a committed replacement turn - `inst-spawn-retry-setup`
3. [x] - `p1` - Authorize action `retry_turn` on the chat resource with `resource.id` the path chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); the PDP is evaluated once for the whole request; **IF** it returns an error, **RETURN** 403 `AUTHZ_DENIED` or 503 with `Retry-After: 5` - `inst-authorize-retry`
4. [x] - `p1` - Load the chat through the compiled scope (see `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`); **IF** it is missing, deleted or foreign, **RETURN** 404 with `gts.cf.core.mini_chat.chat.v1~` - `inst-load-chat-retry`
5. [x] - `p1` - Run the mutation preview for the path `request_id` (see `cpt-cf-mini-chat-algo-turn-mutations-preview`); **IF** it returns an error, **RETURN** its `Problem` - `inst-preview-retry`
6. [x] - `p1` - Run the replacement pipeline with operation `retry` and `content` the original user message content returned by the preview (see `cpt-cf-mini-chat-algo-turn-mutations-run-replacement`); **IF** it returns an error, **RETURN** its `Problem` - `inst-run-retry`
7. [x] - `p1` - In the handler, open the 200 SSE response and start the relay with the received relay handle and `stream_started` payload (see `cpt-cf-mini-chat-algo-streaming-turns-relay-sse`) - `inst-open-retry-stream`
8. [x] - `p1` - **RETURN** the SSE stream; it follows the contract and terminal semantics of `POST /messages:stream` - `inst-return-retry-stream`

### Edit Turn

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-edit-turn`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- An edit of the latest terminal turn of an own chat opens a 200 SSE stream for a new turn with a server-generated `request_id`; the new user message carries the edited `content` as sent and the non-deleted attachments of the original user message; the old turn and its messages are soft-deleted with `replaced_by_request_id` set; the `turn_edit` audit event is committed with the replacement.

**Error Scenarios**:
- 400 `EMPTY_CONTENT` (before authorization); 422 `invalid_json_body`, 400 `json_syntax_error`, 415 `missing_json_content_type` from the platform JSON extractor.
- Every error scenario of `cpt-cf-mini-chat-flow-turn-mutations-retry-turn`, with action `edit_turn`.

**Steps**:
1. [x] - `p1` - User calls `PATCH {url_prefix}/v1/chats/{id}/turns/{request_id}` with the JSON `EditTurnRequest` body through the request gate; the platform extractors reject a body that does not match the schema before the handler runs; unknown body fields are ignored - `inst-call-edit`
2. [x] - `p1` - The handler runs steps 3 to 7 in a separately spawned setup task and awaits only its result (as `inst-spawn-retry-setup`) - `inst-spawn-edit-setup`
3. [x] - `p1` - **IF** `content` is empty or consists only of whitespace, **RETURN** 400 `invalid_argument` with `field_violations[content].reason` `EMPTY_CONTENT` (the rule of `cpt-cf-mini-chat-algo-streaming-turns-validate-request`, `inst-if-empty-content`); otherwise keep `content` unchanged, not trimmed - `inst-validate-edit`
4. [x] - `p1` - Authorize action `edit_turn` with `resource.id` the path chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); the PDP is evaluated once; **IF** it returns an error, **RETURN** 403 `AUTHZ_DENIED` or 503 with `Retry-After: 5` - `inst-authorize-edit`
5. [x] - `p1` - Load the chat through the compiled scope; **IF** it is missing, deleted or foreign, **RETURN** 404 with the chat type - `inst-load-chat-edit`
6. [x] - `p1` - Run the mutation preview for the path `request_id` (see `cpt-cf-mini-chat-algo-turn-mutations-preview`); **IF** it returns an error, **RETURN** its `Problem` - `inst-preview-edit`
7. [x] - `p1` - Run the replacement pipeline with operation `edit` and `content` the edited content (see `cpt-cf-mini-chat-algo-turn-mutations-run-replacement`); **IF** it returns an error, **RETURN** its `Problem` - `inst-run-edit`
8. [x] - `p1` - In the handler, open the 200 SSE response and start the relay (see `cpt-cf-mini-chat-algo-streaming-turns-relay-sse`) - `inst-open-edit-stream`
9. [x] - `p1` - **RETURN** the SSE stream; ordering, terminal semantics, error mapping, downgrade metadata and billing finalization are those of `POST /messages:stream`, with no separate settlement or outbox path - `inst-return-edit-stream`

### Delete Turn

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-delete-turn`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A delete of the latest terminal turn of an own chat soft-deletes the turn and its user and assistant messages, invalidates a summary that covers the turn, commits the `turn_delete` audit event and returns 204 with no body; the previous turn, if any, becomes the latest turn.

**Error Scenarios**:
- Gate and PEP errors; 404 chat or turn; 403 foreign requester; 409 `NOT_LATEST_TURN` (not the latest, already deleted or replaced, or a concurrent mutation won); 400 `turn_state` (running); 500 `internal`. No quota preflight, chat-model check or stream is involved.

**Steps**:
1. [x] - `p1` - User calls `DELETE {url_prefix}/v1/chats/{id}/turns/{request_id}` through the request gate; the path extractor requires both ids to be UUIDs - `inst-call-delete`
2. [x] - `p1` - Authorize action `delete_turn` with `resource.id` the path chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); **IF** it returns an error, **RETURN** 403 `AUTHZ_DENIED` or 503 with `Retry-After: 5` - `inst-authorize-delete`
3. [x] - `p1` - Load the chat through the compiled scope; **IF** it is missing, deleted or foreign, **RETURN** 404 with the chat type - `inst-load-chat-delete`
4. [x] - `p1` - Run the mutation preview for the path `request_id` (see `cpt-cf-mini-chat-algo-turn-mutations-preview`); **IF** it returns an error, **RETURN** its `Problem` - `inst-preview-delete`
5. [x] - `p1` - Run the delete transaction (see `cpt-cf-mini-chat-algo-turn-mutations-remove-turn`); **IF** it returns an error, **RETURN** its `Problem`; nothing was changed - `inst-run-delete`
6. [x] - `p1` - Record `mini_chat_turn_mutation{op="delete"}` and the latency (see `cpt-cf-mini-chat-algo-turn-mutations-record-metrics`) - `inst-record-delete`
7. [x] - `p1` - **RETURN** 204 No Content with no body - `inst-return-204`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Mutation Preview

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-preview`

**Input**: The compiled scope, the loaded chat, the path `request_id`, the caller's `subject.id`

**Output**: The target turn row and its live user message (`id`, `content`, `created_at`); or an error

**Steps**:
1. [x] - `p1` - DB: SELECT `chat_turns` through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is the loaded chat and `request_id` is the path value, soft-deleted rows included - `inst-select-target`
2. [x] - `p1` - **IF** no row is found (unknown `request_id`, or a turn of another chat) - `inst-if-target-missing`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.turn.v1~` - `inst-return-target-404`
3. [x] - `p1` - **IF** the row's `requester_user_id` is not the caller's `subject.id` (NULL included) - `inst-if-foreign-requester`
   1. [x] - `p1` - **RETURN** 403 `permission_denied` with reason `AUTHZ_DENIED` - `inst-return-foreign-403`
4. [x] - `p1` - **IF** the row has `deleted_at` set (replaced by retry or edit, or deleted) - `inst-if-target-deleted`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-return-deleted-not-latest`
5. [x] - `p1` - **IF** the row's `state` is `running` (checked before the latest-turn check) - `inst-if-target-running`
   1. [x] - `p1` - **RETURN** 400 `failed_precondition` with `violations[{subject: turn_state, type: STATE}]`; the client must wait for the terminal event or disconnect to cancel - `inst-return-not-terminal`
6. [x] - `p1` - DB: SELECT the latest turn of the chat: `chat_turns` WHERE `chat_id` is the chat and `deleted_at IS NULL`, ordered by `started_at` descending and then `id` descending, limited to 1 row (index `(chat_id, started_at DESC) WHERE deleted_at IS NULL`); equal `started_at` values are decided by the greater `id` - `inst-select-latest`
7. [x] - `p1` - **IF** the latest turn is not the target (a newer turn exists, including a newer `running` turn) - `inst-if-not-latest`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-return-not-latest`
8. [x] - `p1` - DB: SELECT the target's user message: `messages` WHERE `chat_id` is the chat, `request_id` is the target `request_id`, `role` is `user` and `deleted_at IS NULL` - `inst-select-user-message`
9. [x] - `p1` - **IF** no row is found (a turn always has its user message, so this is corrupt data) - `inst-if-user-message-missing`
   1. [x] - `p1` - Log at `error` with the chat, turn and request ids and **RETURN** 500 `internal` - `inst-return-user-message-500`
10. [x] - `p1` - Run every read of this algorithm without a lock and outside any transaction; the replacement and delete transactions re-check the outcome with guarded writes - `inst-preview-read-only`
11. [x] - `p1` - **RETURN** the target turn and its user message - `inst-return-preview`

### Mutation Input Loading

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs`

**Input**: The compiled scope, the chat, the target's user message id, `rag.max_images_per_message`

**Output**: The carried attachment ids, the image attachments among them with their file ids, the image count, `has_ready_file_search_documents`, `has_ready_code_interpreter_files`, the ready code-interpreter file ids, the chat vector store id and the citation map; or `TOO_MANY_IMAGES`

**Steps**:
1. [x] - `p1` - DB: SELECT `message_attachments` WHERE `tenant_id` is the chat tenant, `chat_id` is the chat and `message_id` is the target's user message - `inst-select-old-links`
2. [x] - `p1` - Set the carried attachment ids to those link ids whose `attachments` row has `deleted_at IS NULL` (deleted attachments are silently excluded, never an error) - `inst-carried-ids`
3. [x] - `p1` - Run the attachment part of `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs` with `attachment_ids` the carried ids: steps `inst-select-request-attachments` to `inst-select-vector-store` (image count, `TOO_MANY_IMAGES` against `rag.max_images_per_message`, the chat's ready attachments and flags, the vector store); the history steps `inst-snapshot-boundary` to `inst-select-recent` are not run here - `inst-attachment-inputs`
4. [x] - `p1` - **IF** it returns `TOO_MANY_IMAGES`, **RETURN** 400 `out_of_range` with `field_violations[image_count].reason` `TOO_MANY_IMAGES` - `inst-return-mutation-too-many-images`
5. [x] - `p1` - **RETURN** the inputs; every read runs before the replacement transaction opens - `inst-return-mutation-inputs`

### Replacement Pipeline

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-run-replacement`

**Input**: The request `SecurityContext`, the compiled scope, the chat, the operation (`retry` or `edit`), the target turn and its user message, `content`, the acceptance instant

**Output**: The relay handle and the `stream_started` payload; or an error

**Steps**:
1. [x] - `p1` - Read the current policy version and its snapshot through the model-policy gateway and reject a `chats.model` absent from `model_catalog` with 400 `invalid_argument` `field_violations[model].reason` `INVALID_MODEL`, exactly as `cpt-cf-mini-chat-flow-streaming-turns-send-message` step `inst-check-chat-model`; a failed call is 500 `internal` - `inst-mutation-check-model`
2. [x] - `p1` - Load the mutation inputs (see `cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs`); **IF** it returns an error, **RETURN** it - `inst-load-mutation-inputs`
3. [x] - `p1` - Run the quota preflight with the tenant, the caller, the chat id, `chats.model`, the UTF-8 byte length of `content`, the image count, `web_search_enabled` the target turn's `web_search_enabled` and the two ready-attachment flags (see `cpt-cf-mini-chat-algo-quota-billing-preflight`); **IF** it returns an error, **RETURN** its `Problem` (400 `FEATURE_DISABLED` `web_search`, 429 `quota_exceeded`, 500) - `inst-mutation-preflight`
4. [x] - `p1` - **IF** the effective model's `max_input_tokens` is greater than 0 and the text estimate of `content` exceeds it (the check of `cpt-cf-mini-chat-flow-streaming-turns-send-message`, `inst-if-input-too-long`), **RETURN** 400 `out_of_range` `INPUT_TOO_LONG` - `inst-mutation-input-too-long`
5. [x] - `p1` - **IF** the image count is greater than 0, apply the image guards of `cpt-cf-mini-chat-flow-streaming-turns-send-message` step `inst-image-guards`: `disable_images` gives 400 `failed_precondition` `violations[{subject: images, type: FEATURE_DISABLED}]`, an effective model without vision gives 400 `VISION_NOT_SUPPORTED`; images are never dropped silently - `inst-mutation-image-guards`
6. [x] - `p1` - Up to this step nothing was written: every rejection returns a JSON `Problem` and leaves the previous turn, its messages, the summary and the chat unchanged - `inst-no-write-before-commit`
7. [x] - `p1` - Run the replacement transaction (see `cpt-cf-mini-chat-algo-turn-mutations-replace-turn`); **IF** it returns an error, **RETURN** its `Problem` (409 `GENERATION_IN_PROGRESS`, 409 `NOT_LATEST_TURN`, 500); nothing was committed - `inst-replace`
8. [x] - `p1` - Record `mini_chat_turn_mutation_latency_ms{op}` (see `cpt-cf-mini-chat-algo-turn-mutations-record-metrics`) - `inst-record-commit-latency`
9. [x] - `p1` - **TRY** - `inst-try-setup`
   1. [x] - `p1` - Load the history of the replacement (see `cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history`) - `inst-load-history`
   2. [x] - `p1` - Decide the tools with the preflight decision, the mutation inputs and the history (see `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`) - `inst-mutation-decide-tools`
   3. [x] - `p1` - Assemble the context with `content`, the carried images, the summary and the recent messages (see `cpt-cf-mini-chat-algo-streaming-turns-assemble-context`) - `inst-mutation-assemble-context`
   4. [x] - `p1` - Resolve the provider of the effective model's `provider_id` for the chat tenant (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`) and build the provider request (see `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`) - `inst-mutation-resolve-and-build`
   5. [x] - `p1` - Run the replacement reserve, the last setup step (see `cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement`) - `inst-mutation-reserve`
10. [x] - `p1` - **CATCH** a failure of any step of the **TRY** block - `inst-catch-setup`
    1. [x] - `p1` - Mark the new turn failed (see `cpt-cf-mini-chat-algo-turn-mutations-fail-setup`) with `context_length_exceeded` for `CONTEXT_BUDGET_EXCEEDED`, `quota_exceeded` for the reserve re-check and `turn_setup_failed` for anything else - `inst-mark-setup-failed`
    2. [x] - `p1` - **RETURN** the `Problem` of the original failure: 400 `out_of_range` `CONTEXT_BUDGET_EXCEEDED`, 429 `quota_exceeded` with subject `tokens`, or 500 `internal`; no SSE stream is opened - `inst-return-setup-failed`
11. [x] - `p1` - Continue exactly as the send flow after its reserve commit, steps `inst-record-reserve` to `inst-spawn-provider-task` of `cpt-cf-mini-chat-flow-streaming-turns-send-message`: record the reserve and image metrics, create the bounded channel, the cancellation token and the relay handle, and spawn the provider task with the new turn (new turn id, new `request_id`, the pre-allocated assistant message id, `user_message_at` the mutation time), the decision, the tools and the built request. A disconnected handler drops the relay handle, and the provider task finalizes the new turn `cancelled` (`inst-handler-gone`) - `inst-hand-off`
12. [x] - `p1` - Record `mini_chat_turn_mutation{op, result="ok"}` - `inst-record-ok`
13. [x] - `p1` - **RETURN** the relay handle and the `stream_started` payload: `request_id` the new `request_id`, `message_id` the pre-allocated assistant message id, `is_new_turn` `true`, `thread_summary_applied` when the summary was kept - `inst-return-replacement`

### Replacement Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-replace-turn`

**Input**: The chat, the caller, the operation, the target turn and its user message, `content`, the carried attachment ids (as read before the transaction), the clock

**Output**: The new turn (turn id, `request_id`, user message id, pre-allocated assistant message id, mutation time); or 409 `GENERATION_IN_PROGRESS`, 409 `NOT_LATEST_TURN`, 500

**Transaction discipline** (normative): see §1.4. Write-first: the INSERT of the new turn is the first statement.

**Steps**:
1. [x] - `p1` - Before the transaction, generate the new turn id, the new `request_id`, the new user message id and the assistant message id (UUID v4, server-generated; the client never supplies the new `request_id`), and set the mutation time to the clock time raised to 1 microsecond after the later of the target's `started_at` and its user message `created_at` when the clock is not past them - `inst-pregenerate-replacement`
2. [x] - `p1` - Before the transaction, build the mutation audit payload (see `cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`); the closure captures every value by value and keeps no state across attempts - `inst-precompute-replacement`
3. [x] - `p1` - DB: INSERT the new `chat_turns` row with the values of the §1.4 "DB writes" table: `state` `running`, the six preflight columns NULL, `requester_type` `user`, `requester_user_id` the caller, `web_search_enabled` the target's `web_search_enabled`, `started_at`, `last_progress_at` and `updated_at` the mutation time - `inst-insert-new-turn`
4. [x] - `p1` - **IF** the INSERT fails with `DomainError::UniqueViolation` (the one-running-turn-per-chat index: a concurrent mutation or send of the chat committed a `running` turn first; on PostgreSQL the INSERT waits for that transaction and fails once it commits) - `inst-if-running-race`
   1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `GENERATION_IN_PROGRESS`; never 500, never retried - `inst-return-generation-in-progress`
5. [x] - `p1` - DB: UPDATE the target `chat_turns` row SET `deleted_at` and `updated_at` the mutation time and `replaced_by_request_id` the new `request_id`, WHERE `id` is the target, `deleted_at IS NULL` and `state` is `completed`, `failed` or `cancelled` - `inst-soft-delete-old-turn`
6. [x] - `p1` - **IF** 0 rows are updated (a concurrent retry, edit or delete committed first) - `inst-if-old-turn-gone`
   1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-return-old-turn-gone`
7. [x] - `p1` - DB: SELECT `chat_turns` WHERE `chat_id` is the chat, `deleted_at IS NULL`, `id` is not the new turn, and `(started_at, id)` is greater than the target's `(started_at, id)`, limited to 1 row - `inst-recheck-latest`
8. [x] - `p1` - **IF** a row is found (a newer turn committed between the preview and this transaction) - `inst-if-newer-turn`
   1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-return-newer-turn`
9. [x] - `p1` - DB: UPDATE `messages` SET `deleted_at` the mutation time WHERE `chat_id` is the chat, `request_id` is the target `request_id` and `deleted_at IS NULL` (the user message and the assistant message when present); on PostgreSQL this takes the row locks that order the mutation against a summary commit locking its frontier message - `inst-soft-delete-old-messages`
10. [x] - `p1` - DB: INSERT the new user `messages` row: the new user message id, `tenant_id`, `chat_id`, `request_id` the new `request_id`, `role` `user`, `content` (retry: the original content unchanged; edit: the edited content as sent), `content_type` `text`, `token_estimate` 0, `request_kind` `chat`, `features_used` `[]`, all token counts 0, `model` NULL, `is_compressed` `false`, `created_at` the mutation time - `inst-insert-new-user-message`
11. [x] - `p1` - DB: SELECT the old user message's `message_attachments` rows joined to `attachments` WHERE `attachments.deleted_at IS NULL`, `attachments.tenant_id` is the chat tenant and `attachments.chat_id` is the chat - `inst-select-carried-links`
12. [x] - `p1` - **FOR EACH** carried link - `inst-for-each-carried-link`
    1. [x] - `p1` - DB: INSERT `message_attachments` with `tenant_id`, `chat_id`, `message_id` the new user message id, `attachment_id` and `created_at` the mutation time; the old link stays on the soft-deleted message - `inst-insert-carried-link`
13. [x] - `p1` - Invalidate a covering summary (see `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`) with the old user message `(created_at, id)` - `inst-replace-invalidate-summary`
14. [x] - `p1` - DB: UPDATE `chats` SET `updated_at` the mutation time WHERE `id` is the chat, through the owner-scoped chat scope - `inst-touch-chat-mutation`
15. [x] - `p1` - DB: enqueue the `turn_retry` or `turn_edit` payload to `outbox.audit_queue_name` partitioned by `tenant_id` - `inst-enqueue-replacement-audit`
16. [x] - `p1` - Commit; the replaced old turn, its soft-deleted messages, the new `running` turn, the new user message with its links, the summary change, the chat bump and the audit event become visible together; wake the audit queue after the commit - `inst-commit-replacement`
17. [x] - `p1` - **IF** the transaction failed otherwise (database error, conflict retries exhausted, audit payload above the outbox size limit), **RETURN** 500 `internal`; nothing is left behind and the previous turn stays - `inst-return-replacement-error`
18. [x] - `p1` - **RETURN** the new turn - `inst-return-new-turn`

### Turn Delete Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-remove-turn`

**Input**: The chat, the caller, the target turn and its user message, the clock

**Output**: Success; or 409 `NOT_LATEST_TURN`, 500

**Transaction discipline** (normative): see §1.4. Write-first: the guarded UPDATE of the target turn is the first statement.

**Steps**:
1. [x] - `p1` - Before the transaction, take the mutation time (as `inst-pregenerate-replacement`) and build the `turn_delete` payload (see `cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`) - `inst-precompute-delete`
2. [x] - `p1` - DB: UPDATE the target `chat_turns` row SET `deleted_at` and `updated_at` the mutation time WHERE `id` is the target, `deleted_at IS NULL` and `state` is `completed`, `failed` or `cancelled`; `replaced_by_request_id` stays NULL - `inst-soft-delete-turn`
3. [x] - `p1` - **IF** 0 rows are updated (a concurrent retry, edit or delete committed first) - `inst-if-delete-missed`
   1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-return-delete-missed`
4. [x] - `p1` - DB: SELECT `chat_turns` WHERE `chat_id` is the chat, `deleted_at IS NULL` and `(started_at, id)` is greater than the target's, limited to 1 row - `inst-recheck-latest-delete`
5. [x] - `p1` - **IF** a row is found (a send committed a newer turn after the preview) - `inst-if-newer-delete`
   1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-return-newer-delete`
6. [x] - `p1` - DB: UPDATE `messages` SET `deleted_at` the mutation time WHERE `chat_id` is the chat, `request_id` is the target `request_id` and `deleted_at IS NULL` - `inst-soft-delete-messages`
7. [x] - `p1` - Invalidate a covering summary (see `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`) with the target's user message `(created_at, id)` - `inst-delete-invalidate-summary`
8. [x] - `p1` - DB: enqueue the `turn_delete` payload to `outbox.audit_queue_name` partitioned by `tenant_id` - `inst-enqueue-delete-audit`
9. [x] - `p1` - Commit and wake the audit queue; `message_attachments` rows, reactions and `quota_usage` are not touched, and `chats.updated_at` is not bumped - `inst-commit-delete`
10. [x] - `p1` - **IF** the transaction failed otherwise, **RETURN** 500 `internal`; nothing is left behind - `inst-return-delete-error`
11. [x] - `p1` - **RETURN** success - `inst-return-deleted`

### Summary Invalidation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`

**Input**: The open replacement or delete transaction (after the old turn and its messages were soft-deleted), the chat, the mutated turn's user message `(created_at, id)`

**Output**: `invalidated` or `kept`

**Steps**:
1. [x] - `p1` - DB: SELECT `thread_summaries` WHERE `tenant_id` is the chat tenant and `chat_id` is the chat - `inst-select-summary-tx`
2. [x] - `p1` - **IF** no row exists - `inst-if-no-summary`
   1. [x] - `p1` - **RETURN** `kept` - `inst-return-no-summary`
3. [x] - `p1` - **IF** the frontier `(summarized_up_to_created_at, summarized_up_to_message_id)` is at or after the user message `(created_at, id)` in the order `(created_at ASC, id ASC)` (the summary covers the mutated turn, possible after a delete made the previous turn the latest) - `inst-if-summary-covers`
   1. [x] - `p1` - DB: DELETE the `thread_summaries` row of the chat - `inst-delete-summary`
   2. [x] - `p1` - DB: UPDATE `messages` SET `is_compressed` `false` WHERE `chat_id` is the chat and `is_compressed` is `true` (every message of the chat, soft-deleted rows included) - `inst-clear-compressed`
   3. [x] - `p1` - **RETURN** `invalidated`; until the next summary, context assembly reads the uncompressed history bounded by the token budget, and the next summary trigger builds a new summary with no base frontier - `inst-return-invalidated`
4. [x] - `p1` - **RETURN** `kept`; nothing changes, and after a retry or edit the next context is the kept summary followed by the replacement turn - `inst-return-kept`

### Mutation Audit Event

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`

**Input**: The operation, the chat (tenant, id), the caller, the target `request_id`, the new `request_id` (retry and edit), the mutation time

**Output**: One encoded `TurnMutationAuditEvent` payload

**Steps**:
1. [x] - `p1` - **MATCH** the operation - `inst-match-audit-op`
   - [x] - `p1` - **CASE** `retry`: `event_type` `turn_retry` with `tenant_id`, `actor_user_id` the caller, `chat_id`, `original_request_id` the target `request_id`, `new_request_id` the new `request_id`, `timestamp` the mutation time - `inst-case-audit-retry`
   - [x] - `p1` - **CASE** `edit`: `event_type` `turn_edit` with the same fields as `turn_retry` - `inst-case-audit-edit`
   - [x] - `p1` - **CASE** `delete`: `event_type` `turn_delete` with `tenant_id`, `actor_user_id`, `chat_id`, `request_id` the target `request_id`, `timestamp` - `inst-case-audit-delete`
2. [x] - `p1` - Never place message content, attachment data or provider identifiers in the payload (`cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`) - `inst-audit-no-content`
3. [x] - `p1` - Encode it as a mutation event (see `cpt-cf-mini-chat-algo-gear-foundation-encode-events`, `inst-encode-mutation-audit`); delivery to the audit plugin happens later in `cpt-cf-mini-chat-algo-gear-foundation-handle-audit` - `inst-encode-mutation-event`
4. [x] - `p1` - **RETURN** the payload; it is enqueued only by a committed mutation transaction, never by a rejected or rolled-back mutation, and a replacement that later fails setup keeps its committed mutation event - `inst-return-mutation-event`

### Replacement History Loading

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history`

**Input**: The compiled scope, the chat, the new `request_id`, `context.recent_messages_limit`

**Output**: The snapshot boundary, the thread summary (or none), the recent messages in chronological order

**Steps**:
1. [x] - `p1` - DB: SELECT the chat's latest `messages` row with `request_id IS NOT NULL`, `request_id` other than the new `request_id` and `deleted_at IS NULL`, ordered by `created_at` descending and then `id` descending; its `(created_at, id)` is the snapshot boundary (no boundary when only the new user message is live). The soft-deleted messages of the replaced turn and the new user message are therefore never history - `inst-replacement-boundary`
2. [x] - `p1` - Read the summary and the recent messages with that boundary as `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs` steps `inst-select-summary` and `inst-select-recent` do (non-compressed, non-deleted, `user` and `assistant` roles, after the summary frontier when a summary exists, at most `context.recent_messages_limit` rows) - `inst-replacement-recent`
3. [x] - `p1` - Run these reads after the replacement commit and before the reserve transaction; a summary deleted by the mutation is not found - `inst-history-after-commit`
4. [x] - `p1` - **RETURN** the history - `inst-return-history`

### Replacement Reserve

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement`

**Input**: The chat tenant, the caller, the new turn id, the preflight decision, the current time

**Output**: The six preflight column values; or quota exceeded with subject `tokens`; or an internal error

**Transaction discipline** (normative): see §1.4. The transaction is the reserve transaction of `cpt-cf-mini-chat-algo-quota-billing-reserve-write` opened through `SeaQuotaRepository::reserve_with`; its first statement is the daily `total` UPSERT.

**Steps**:
1. [x] - `p1` - Run the reserve write in mode `fill` with the new turn id (see `cpt-cf-mini-chat-algo-quota-billing-reserve-write`, `inst-if-fill-mode`): the bucket UPSERTs, the re-read and the limit re-check, then the UPDATE of the six preflight columns of the new turn WHERE `state` is `running` and `reserve_tokens IS NULL` (`inst-fill-turn`); the closure adds no other statement - `inst-reserve-fill`
2. [x] - `p1` - **IF** the re-check fails - `inst-if-fill-over`
   1. [x] - `p1` - **RETURN** 429 `quota_exceeded` with subject `tokens`; the transaction rolled back, no reserve exists (the retry and edit branch of `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`, `inst-return-reserve-rejected`) - `inst-return-fill-over`
3. [x] - `p1` - **IF** the fill updated no row (`inst-if-fill-missed`: the turn is no longer `running`), or the transaction failed otherwise (database error, conflict retries exhausted), **RETURN** internal error; nothing is left behind - `inst-return-fill-error`
4. [x] - `p1` - **RETURN** the six values; the turn is now `running` with its reserve and enters the billing state `in_progress` of `cpt-cf-mini-chat-state-quota-billing-turn-billing`, before any provider call - `inst-return-fill`

### Setup Failure Marking

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-fail-setup`

**Input**: The chat tenant, the new turn id, the error code (`context_length_exceeded`, `turn_setup_failed`, `quota_exceeded`), the current time

**Output**: Nothing; the original setup failure is returned by the caller in every case

**Steps**:
1. [x] - `p1` - DB: UPDATE `chat_turns` SET `state` `failed`, `error_code` the code, `completed_at` and `updated_at` the current time WHERE `id` is the new turn, `tenant_id` is the chat tenant and `state` is `running`, as a single autocommit statement; `error_detail` and `assistant_message_id` stay NULL - `inst-mark-failed`
2. [x] - `p1` - Run no settlement, enqueue no usage event and no audit event, and persist no assistant message: no reserve exists (DESIGN §5.7 "Unstarted retry/edit turn"); the turn status reports `error` with the code, which is never sent over SSE - `inst-no-settlement`
3. [x] - `p1` - **IF** the UPDATE fails or updates no row, log at `error` with the chat, turn and request ids; the turn stays `running` with NULL reserve fields and the orphan watchdog finalizes it without settlement (`cpt-cf-mini-chat-algo-quota-billing-finalize-turn`, `inst-if-no-reserve`) - `inst-if-mark-failed-error`
4. [x] - `p1` - Record `mini_chat_turn_mutation{op, result="setup_failed"}` - `inst-record-setup-failed`
5. [x] - `p1` - **RETURN**; the chat is no longer blocked by a `running` turn, the new user message stays live, and the failed turn is the latest terminal turn that can be retried, edited or deleted - `inst-return-fail-setup`

### Mutation Metrics Recording

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-record-metrics`

**Input**: The operation, the outcome, the acceptance instant, the commit instant

**Output**: Recorded instruments

**Steps**:
1. [x] - `p1` - Count `mini_chat_turn_mutation{op,result}` once per request with `op` `retry`, `edit` or `delete` and `result` `ok` (committed; for retry and edit, the stream was handed over), `not_latest_turn`, `generation_in_progress`, `not_terminal`, `rejected` (every other refusal before the commit, including 404, 403, 400, 429 and 503), `setup_failed` (a failure after the commit) or `error` (500 before the commit) - `inst-metric-mutation`
2. [x] - `p1` - Record `mini_chat_turn_mutation_latency_ms{op}` from the handler accepting the request to the mutation commit, for committed mutations only - `inst-metric-mutation-latency`
3. [x] - `p1` - Use only the labels `op` and `result`; never `tenant_id`, `user_id`, `chat_id` or `request_id`; the stream and quota instruments of a replacement are recorded by `cpt-cf-mini-chat-algo-streaming-turns-record-metrics` and `cpt-cf-mini-chat-algo-quota-billing-record-metrics` - `inst-metric-mutation-labels`
4. [x] - `p1` - **RETURN** - `inst-return-mutation-metrics`

## 4. States (CDSL)

### Turn Visibility State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-turn-mutations-turn-visibility`

**States**: `live`, `replaced`, `deleted`

**Initial State**: `live`

A `chat_turns` row is `live` while `deleted_at IS NULL`, `replaced` when `deleted_at` and `replaced_by_request_id` are set, and `deleted` when only `deleted_at` is set. Visibility is independent of the lifecycle `state` of `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle`, which a mutation never changes on the old turn. Only a `live` turn that is terminal and the latest of its chat can leave `live`.

**Transitions**:
1. [x] - `p1` - **FROM** `live` **TO** `replaced` **WHEN** a retry or edit of the latest terminal turn commits its replacement transaction; the turn's messages are soft-deleted and a new `live` `running` turn with a new `request_id` takes its place - `inst-to-replaced`
2. [x] - `p1` - **FROM** `live` **TO** `deleted` **WHEN** a delete of the latest terminal turn commits; the turn's messages are soft-deleted and the previous turn, if any, becomes the latest - `inst-to-deleted`
3. [x] - `p1` - **FROM** `live` **TO** `live` **WHEN** a mutation is rejected before or inside its transaction (not latest, running, foreign, preflight rejection, conflict); nothing is written - `inst-stay-live`
4. [x] - `p1` - **FROM** `replaced` or `deleted` **TO** the same state **WHEN** any later retry, edit or delete names it (409 `NOT_LATEST_TURN`), `messages:stream` reuses its `request_id` (409 `request_id_conflict`) or the turn status is read (404); soft-deleted turns are never restored or purged in P1 - `inst-soft-deleted-final`

### Replacement Turn Setup State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-turn-mutations-replacement-setup`

**States**: `running_unreserved`, `running_reserved`, `failed_setup`, `finalized`

**Initial State**: `running_unreserved`

The replacement turn is created by the replacement transaction with `state` `running` and NULL preflight columns. It blocks other turns of the chat through the one-running-turn index from that commit on. `finalized` stands for every terminal state reached through `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle`.

**Transitions**:
1. [x] - `p1` - **FROM** `running_unreserved` **TO** `running_reserved` **WHEN** the fill-mode reserve transaction commits the six preflight columns together with the bucket increments - `inst-to-reserved`
2. [x] - `p1` - **FROM** `running_unreserved` **TO** `failed_setup` **WHEN** the history load, context assembly, provider resolution, request building or the reserve fails; `error_code` is `context_length_exceeded`, `turn_setup_failed` or `quota_exceeded`, with no settlement and no outbox event - `inst-to-failed-setup`
3. [x] - `p1` - **FROM** `running_unreserved` **TO** `finalized` **WHEN** the orphan watchdog finalizes a turn left `running` by a crash or a failed setup mark; it skips settlement because the reserve fields are NULL - `inst-unreserved-orphaned`
4. [x] - `p1` - **FROM** `running_reserved` **TO** `finalized` **WHEN** the provider task finalizes the stream (`completed`, `failed`, `cancelled`) or the orphan watchdog wins the CAS, exactly as for a turn created by `POST /messages:stream` - `inst-to-finalized`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Mutation Eligibility

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-eligibility`

The system **MUST** allow retry, edit and delete only for the latest terminal turn of the requesting user, as specified in `cpt-cf-mini-chat-algo-turn-mutations-preview` (`cpt-cf-mini-chat-design-turn-mutations` rules 1 to 3 and 9, `cpt-cf-mini-chat-fr-turn-mutations`, `cpt-cf-mini-chat-principle-linear-conversation`). The latest turn is the non-deleted turn with the greatest `(started_at, id)` of the chat. An unknown turn is 404 with the turn type, a foreign requester 403 `AUTHZ_DENIED`, a soft-deleted target 409 `NOT_LATEST_TURN`, a `running` target 400 `failed_precondition` `turn_state` / `STATE` (checked before the latest-turn check), and any other non-latest target 409 `NOT_LATEST_TURN`. The PDP is evaluated once per request with `retry_turn`, `edit_turn` or `delete_turn` and `resource.id` the chat id; the quota preflight and the stream make no further PDP call.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-preview`
- `cpt-cf-mini-chat-flow-turn-mutations-retry-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-edit-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-delete-turn`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH /v1/chats/{id}/turns/{request_id}`, `DELETE /v1/chats/{id}/turns/{request_id}`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (read), `cpt-cf-mini-chat-dbtable-messages` (read)
- Components: `cpt-cf-mini-chat-component-chat-service`
- Entities: Chat turn

### Retry and Edit Through the Send Pipeline

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-replacement-pipeline`

The system **MUST** run retry and edit through the full send pipeline as specified in `cpt-cf-mini-chat-algo-turn-mutations-run-replacement` and the "Retry / edit variant" of `cpt-cf-mini-chat-seq-send-message`, in the order of the §1.4 check-order table. Before the commit it runs the chat-model check, the image count, the quota preflight with the original `web_search_enabled` flag (`cpt-cf-mini-chat-constraint-quota-before-outbound`), `INPUT_TOO_LONG` and the image guards on the carried images (`cpt-cf-mini-chat-constraint-model-image-capability`). Every rejection there leaves the previous turn and its answer intact. After the commit it runs the tool decision, context assembly with the token budget (`cpt-cf-mini-chat-constraint-context-budget`), provider resolution and the fill-mode reserve as the last setup step. It then hands over to the provider task, the relay and the finalization of `cpt-cf-mini-chat-feature-streaming-turns` with no separate execution path for settlement or outbox emission. The response is the `POST /messages:stream` SSE contract whose `stream_started` carries the new server-generated `request_id`, `is_new_turn` `true` and the pre-allocated assistant message id. Edit rejects empty or whitespace-only `content` with 400 `EMPTY_CONTENT` before authorization and stores the content as sent.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-run-replacement`
- `cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs`
- `cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history`
- `cpt-cf-mini-chat-algo-turn-mutations-reserve-replacement`
- `cpt-cf-mini-chat-flow-turn-mutations-retry-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-edit-turn`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-constraint-model-image-capability`, `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- API: `POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH /v1/chats/{id}/turns/{request_id}` (SSE response)
- Sequence: `cpt-cf-mini-chat-seq-send-message`
- DB Table: `cpt-cf-mini-chat-dbtable-quota-usage` (through the quota helper), `cpt-cf-mini-chat-dbtable-thread-summaries` (read)
- Components: `cpt-cf-mini-chat-component-quota-service`, `cpt-cf-mini-chat-component-chat-service`
- Entities: Chat turn, Message

### Replacement Transaction and Carry-Forward

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-replacement-transaction`

The system **MUST** commit every retry and edit in the single write-first replacement transaction of `cpt-cf-mini-chat-algo-turn-mutations-replace-turn`, with exactly the rows of the §1.4 "DB writes" table. The new `running` turn is inserted first (preflight columns NULL, `web_search_enabled` copied from the old turn, a server-generated UUID v4 `request_id`). The old turn gets `deleted_at` and `replaced_by_request_id` through a guarded UPDATE, the latest turn is re-checked, the old messages are soft-deleted, and the new user message is inserted with the original content (retry) or the edited content (edit). The links of the non-deleted original attachments are copied to new `message_attachments` rows while the old links stay. A covering summary is invalidated, `chats.updated_at` is bumped and the `turn_retry` / `turn_edit` audit event is enqueued. A conflict re-runs the deterministic closure at most 3 attempts in total; any failure rolls everything back, leaves the previous turn intact, and returns 500. No plugin or provider call runs inside it.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-replace-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs`
- `cpt-cf-mini-chat-state-turn-mutations-turn-visibility`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-thread-summaries`
- DB: shared outbox tables (queue `mini-chat.audit`)
- Components: `cpt-cf-mini-chat-component-chat-store`
- Entities: Chat turn, Message, AuditEvent

### Setup Failure After the Commit

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-setup-failure`

The system **MUST** mark the replacement turn `failed` when a setup step after the replacement commit fails, as specified in `cpt-cf-mini-chat-algo-turn-mutations-fail-setup` and `cpt-cf-mini-chat-state-turn-mutations-replacement-setup`. A context budget failure gives `context_length_exceeded` and 400 `CONTEXT_BUDGET_EXCEEDED`, a failed reserve re-check `quota_exceeded` and 429 `quota_exceeded` (`tokens`), and every other failure `turn_setup_failed` and 500. Each is a JSON `Problem` without an SSE stream, with `completed_at` set, no settlement, no usage event, no finalization audit event and no assistant message, so the chat is never left blocked by a `running` turn. When the mark itself fails, the turn stays `running` with NULL reserve fields for the orphan watchdog (`cpt-cf-mini-chat-design-nonterminal-reconciliation`).

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-fail-setup`
- `cpt-cf-mini-chat-state-turn-mutations-replacement-setup`
- `cpt-cf-mini-chat-algo-turn-mutations-run-replacement`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (`state`, `error_code`, `completed_at`)
- Design: `cpt-cf-mini-chat-design-turn-lifecycle`

### Turn Delete

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-delete`

The system **MUST** serve `DELETE /v1/chats/{id}/turns/{request_id}` as specified in `cpt-cf-mini-chat-flow-turn-mutations-delete-turn` and `cpt-cf-mini-chat-algo-turn-mutations-remove-turn`. After the preview it runs one write-first transaction that soft-deletes the turn (`deleted_at`, `replaced_by_request_id` NULL) and its user and assistant messages, re-checks the latest turn, invalidates a covering summary and enqueues `turn_delete`. It then answers 204 with no body. Delete runs no quota step, opens no stream, bumps no `chats.updated_at`, and leaves `message_attachments`, reactions and the quota of the deleted turn unchanged. The deleted turn and its messages disappear from the message list, the message count, the context and the turn status (404).

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-delete-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-remove-turn`

**Touches**:
- API: `DELETE /v1/chats/{id}/turns/{request_id}`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-thread-summaries`
- Entities: Chat turn, Message

### Concurrent Mutation Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-concurrency`

The system **MUST** resolve concurrent mutations of a chat deterministically through the database, as specified in `cpt-cf-mini-chat-algo-turn-mutations-replace-turn` and `cpt-cf-mini-chat-algo-turn-mutations-remove-turn` (DESIGN §3.9 rule 7). Exactly one mutation of a given turn commits. A loser whose INSERT of the new `running` turn meets a committed `running` turn of the chat (one-running-turn index, typed `DomainError::UniqueViolation` on both engines) gets 409 `aborted` `GENERATION_IN_PROGRESS`, never 500. A loser that finds the target already soft-deleted, by the preview or by the guarded UPDATE, or that finds a newer live turn, gets 409 `aborted` `NOT_LATEST_TURN`. Every loser rolls back completely: no second replacement turn, user message, link, summary change, chat bump or audit event remains. This holds on SQLite (write-first serialization) and PostgreSQL (unique-index wait and row locks) alike, and the same index keeps a concurrent `POST /messages:stream` and a mutation from both running.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-replace-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-remove-turn`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (one-running-turn index, latest-turn index)
- Design: `cpt-cf-mini-chat-design-turn-mutations`

### Summary Invalidation on Mutation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-summary-invalidation`

The system **MUST** delete the chat's `thread_summaries` row and clear `is_compressed` on all messages of the chat inside the retry, edit or delete transaction when the summary frontier is at or after the mutated turn's user message `(created_at, id)`, and keep the summary otherwise, as specified in `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary` (DESIGN "Summary Interaction on Turn Mutation", `cpt-cf-mini-chat-fr-thread-summary`, `cpt-cf-mini-chat-principle-summary-over-history`). The soft-delete of the mutated messages runs before the check and takes the row locks that order the mutation against a summary commit that locks its frontier message (`cpt-cf-mini-chat-seq-thread-summary`); summary regeneration is left to `cpt-cf-mini-chat-feature-background-workers`.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-messages` (`is_compressed`)
- Entities: ThreadSummary

### Replaced Request IDs and Idempotency

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-request-ids`

The system **MUST** keep the idempotency guarantees of `cpt-cf-mini-chat-feature-streaming-turns` across mutations, as stated in the §1.4 "Replay and idempotency interplay". Retry and edit generate the new `request_id` on the server, and the old turn's `replaced_by_request_id` names it. The `request_id` of a replaced or deleted turn stays taken, so `POST /messages:stream` with it is 409 `request_id_conflict` (`cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`), the turn status is 404 (`cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`), and a further mutation of it is 409 `NOT_LATEST_TURN`. A completed replacement turn replays side-effect-free by its new `request_id` (`cpt-cf-mini-chat-algo-streaming-turns-replay-completed`). Soft-deleted turns are excluded from the replay path, the context and the message list.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-replace-turn`
- `cpt-cf-mini-chat-state-turn-mutations-turn-visibility`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (`request_id`, `deleted_at`, `replaced_by_request_id`)
- API: `POST /v1/chats/{id}/messages:stream`, `GET /v1/chats/{id}/turns/{request_id}` (interplay only)

### Mutation Audit Events and Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-audit-metrics`

The system **MUST** enqueue exactly one `TurnMutationAuditEvent` (`turn_retry`, `turn_edit` or `turn_delete` with the fields of `cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`) per committed mutation, in the mutation transaction, to `outbox.audit_queue_name` partitioned by `tenant_id` (`cpt-cf-mini-chat-fr-audit`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`). No event is enqueued for a rejected or rolled-back mutation, and no content is included. The finalization event of a replacement turn comes from `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`, and a replacement that fails setup gets none. The system **MUST** record `mini_chat_turn_mutation{op,result}` and `mini_chat_turn_mutation_latency_ms{op}` as specified in `cpt-cf-mini-chat-algo-turn-mutations-record-metrics` (`cpt-cf-mini-chat-nfr-observability-supportability`).

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`
- `cpt-cf-mini-chat-algo-turn-mutations-record-metrics`

**Touches**:
- DB: shared outbox tables (queue `mini-chat.audit`)
- Observability: `mini_chat_*` meter (instruments registered by `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`)
- Entities: AuditEvent

### REST Registration and OpenAPI

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-rest-routes`

The system **MUST** register, under `url_prefix` and with the base license feature (`cpt-cf-mini-chat-interface-public-api`), the three routes with tag `Mini Chat Turns`. `POST {prefix}/v1/chats/{id}/turns/{request_id}/retry` is operation `mini_chat.retry_turn`, with no request body, response 200 `text/event-stream` `MiniChatSseEvent` and the 400, 401, 403, 404, 409, 429, 500 and 503 (`Retry-After`) problem responses. `PATCH {prefix}/v1/chats/{id}/turns/{request_id}` is operation `mini_chat.edit_turn`, with required body `EditTurnRequest` (`content` string, required), response 200 `text/event-stream` and the 400, 401, 403, 404, 409, 422, 429, 500 and 503 problem responses. `DELETE {prefix}/v1/chats/{id}/turns/{request_id}` is operation `mini_chat.delete_turn`, with response 204 and the 400, 401, 403, 404, 409, 500 and 503 problem responses. The SSE responses use the same headers, relay and compression exclusion as `POST /messages:stream` (`cpt-cf-mini-chat-contract-sse-streaming`).

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-retry-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-edit-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-delete-turn`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH /v1/chats/{id}/turns/{request_id}`, `DELETE /v1/chats/{id}/turns/{request_id}`

### Cross-Cutting Quality and Verification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-quality-gates`

The system **MUST** meet these cross-cutting requirements and completion signals:

- **Security**: fail-closed authorization of `retry_turn`, `edit_turn` and `delete_turn` (deny 403 `AUTHZ_DENIED`, PDP failure 503 `Retry-After: 5`), evaluated once per request; the chat is loaded owner-scoped and every turn, message, link and summary statement is tenant-scoped and bound to that chat (`cpt-cf-mini-chat-nfr-tenant-isolation`, `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`); a foreign requester is 403; no content, provider identifier or driver message in any `Problem`, audit payload or metric label.
- **Reliability**: every external call bounded (PDP 5 s, policy calls and registry query 10 s, the provider stream by the OAGW upstream timeouts); a rejection before the commit never touches the previous turn; a failure after the commit never leaves the chat blocked, except when the failed mark itself fails, which the orphan watchdog recovers; a client disconnect after the commit finalizes the replacement as `cancelled` through the spawned setup task.
- **Data integrity**: the replacement and delete transactions follow the transaction discipline of §1.4 (write-first, guarded writes re-checking the preview, deterministic closures, bounded conflict retry, no plugin or provider call inside); one running turn per chat and one turn per `(chat_id, request_id)` enforced by the database; the old turn's lifecycle columns and quota are never changed; soft-deleted rows are retained.
- **Observability**: the mutation instruments of `cpt-cf-mini-chat-algo-turn-mutations-record-metrics`; corrupt data (a turn without its user message) and a failed setup mark are logged at `error` with chat, turn and request ids, never with content.
- **Rollback**: the feature adds no migration (the columns and indexes come from `cpt-cf-mini-chat-dod-gear-foundation-schema`); removing it removes the three routes; soft-deleted turns and messages stay valid and hidden.
- **Test layering**: unit tests cover the preview matrix (unknown, foreign, deleted, running, not latest, equal `started_at` decided by `id`), the audit payloads, the summary coverage comparison at, before and after the frontier, the setup failure code mapping and the mutation time rule. Service-level tests run on SQLite and PostgreSQL with a fake `ServiceGatewayClientV1`, a mock PDP and the static policy and audit plugins. They check the exact rows of the §1.4 "DB writes" table, the outbox messages and the quota buckets. They cover the race of two concurrent retries, of a retry and a delete, and of a retry and a send, with the loser leaving nothing behind. End-to-end tests drive the three REST routes against the OpenAI-compatible mock LLM behind OAGW and assert the SSE stream of the replacement and the provider request body (original content and images for retry, edited content for edit). No live provider is called.
- **Compile-time gates**: the mutation service depends on ports (turn repository, quota service, stream service), not on SeaORM entities; `make dylint` passes.
- **Completion signals**: automated tests exist and pass for every owned acceptance item listed in §1.4; `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass. Every implemented CDSL instruction of this feature is wrapped in `@cpt-begin:{cpt-id}:p1:inst-{id}` / `@cpt-end:{cpt-id}:p1:inst-{id}` code markers (FULL traceability), and every flow, algorithm, state and definition-of-done ID of this feature appears in code. The checkboxes of this document and the featstatus are set to `[x]` only when their markers and tests exist.

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-retry-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-replace-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-remove-turn`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- Build: `make gear-ci GEAR=mini-chat`, `make dylint`, `cfs validate`

## 6. Acceptance Criteria

- [ ] Turn Mutations — "Retry / edit / delete act only on the latest, terminal turn": in a chat with completed turns A then B, a retry, edit or delete of A returns 409 `aborted` with `context.reason` `NOT_LATEST_TURN`. While B is `running` (mock holding its stream open), each of the three on B returns 400 `failed_precondition` with `violations[{subject: turn_state, type: STATE}]`, and each on A returns `NOT_LATEST_TURN`. Once B is `completed`, `failed` or `cancelled`, each of the three on B is accepted. A retry, edit or delete of a soft-deleted turn (replaced or deleted) returns `NOT_LATEST_TURN`, an unknown `request_id` 404 with `gts.cf.core.mini_chat.turn.v1~`, a non-UUID `request_id` 400 `invalid_path_params`, and a chat of another user or tenant 404 with the chat type. With two live turns seeded with equal `started_at`, only the one with the greater `id` is mutable. In every rejected case `chat_turns`, `messages`, `message_attachments`, `thread_summaries`, `chats.updated_at`, `quota_usage` and the outbox are unchanged.
- [ ] Turn Mutations — "A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id": a retry of the latest completed turn returns 200 `text/event-stream` with `stream_started` (`request_id` a new UUID v4 different from the path id, `is_new_turn` `true`), `delta` events and `done` with usage, models and the quota decision. The mock receives the original content as the current message, and the history without the replaced turn. An edit sends the edited content instead and stores it as the new user message. After `done` the old turn has `deleted_at` set and `replaced_by_request_id` equal to the new `request_id` with its `state` unchanged, and its user and assistant messages have `deleted_at` set. The new turn is `completed` with all six preflight columns set, `requester_user_id` the caller and `web_search_enabled` the old value. The new user and assistant messages carry the new `request_id`, `chats.updated_at` moved, and the quota buckets show one reserve and one settlement. Before the commit, with the old turn intact afterwards: a chat model removed from the catalog gives 400 `INVALID_MODEL`, exhausted quota 429 `quota_exceeded`, `disable_web_search` on a turn sent with web search 400 `FEATURE_DISABLED` `web_search`, `disable_images` on a turn with an image 400 `FEATURE_DISABLED` `images`, an effective model without vision 400 `VISION_NOT_SUPPORTED`, an edit above `max_input_tokens` 400 `INPUT_TOO_LONG`, and an edit with `""` or `"   "` 400 `EMPTY_CONTENT` (also on a foreign chat). After the commit: a catalog whose `max_output_tokens_applied` is at least `context_window` gives 400 `CONTEXT_BUDGET_EXCEEDED`, with the old turn replaced and the new turn `failed` with `error_code` `context_length_exceeded`; a reserve re-check failure (a concurrent reserve filling the bucket between preflight and reserve) gives 429 and `quota_exceeded`; a provider resolution failure gives 500 and `turn_setup_failed`. Each of these leaves no `quota_usage` change, no usage event and no finalization audit event, and the turn status reports `error` with that code. In every case no SSE stream is opened, and a following send to the chat is accepted.
- [ ] Turn Mutations — "Concurrent mutations resolve deterministically": two concurrent retries (and a concurrent retry and edit) of the latest completed turn, with the mock holding the winner's stream open, yield exactly one 200 stream. The other request gets 409 `aborted` with `context.reason` `GENERATION_IN_PROGRESS` when its replacement transaction met the winner's running turn, or `NOT_LATEST_TURN` when its preview already saw the target soft-deleted; it never gets 500 or a second stream. Afterwards the chat holds one replaced old turn, exactly one new live turn, one new user message with its links, one `turn_retry` / `turn_edit` audit event and one reserve. Two concurrent deletes of the latest turn yield one 204 and one 409 `NOT_LATEST_TURN` with one `turn_delete` event. A concurrent retry and delete yield one success and one 409, never both. A retry that races a `POST /messages:stream` of the same chat yields one running turn: the send gets 409 `turn_already_running`, or the retry gets `GENERATION_IN_PROGRESS` or `NOT_LATEST_TURN`. All of this holds on SQLite and PostgreSQL alike.
- [ ] Turn Mutations — "Mutated turns correctly carry forward attachment and tool-usage history": a turn sent with a ready document, an image and `web_search.enabled` `true`, when retried or edited, gets a new user message with one `message_attachments` row for each still non-deleted original attachment. The old rows remain on the soft-deleted old user message. An attachment soft-deleted since the original send is silently left out, without an error. The provider request re-sends the image as `input_image` with the same image guards, includes `web_search` (the new turn has `web_search_enabled` `true`) and `file_search` over the whole chat vector store, and the message list shows the attachments on the new user message. A turn sent without web search is retried without the `web_search` tool even when the model supports it.
- [ ] Turn delete (feature-local): `DELETE` of the latest completed turn returns 204 with an empty body. The turn has `deleted_at` set and `replaced_by_request_id` NULL, its user and assistant messages have `deleted_at` set, `GET /v1/chats/{id}/messages` no longer lists them, `message_count` drops by 2, and `GET /v1/chats/{id}/turns/{request_id}` returns 404. The previous turn becomes mutable, and `chats.updated_at`, `quota_usage` and `message_attachments` are unchanged. A failed turn (one message) deletes one message.
- [ ] Summary invalidation (feature-local, mutation side of Cleanup & Recovery "mutation-driven invalidation"): with a `thread_summaries` row whose frontier is the assistant message of the latest turn and messages marked `is_compressed` `true`, a delete, retry or edit of that turn deletes the summary row and leaves no message of the chat with `is_compressed` `true`. The next provider request carries no summary item and the uncompressed history within the budget. With a frontier before the mutated turn's user message, the summary and the flags are unchanged, and the next provider request of a retry starts with the kept summary followed by the replacement turn. A summary frontier exactly equal to the user message `(created_at, id)` counts as covering.
- [ ] Mutation audit events (feature-local): a committed retry enqueues exactly one `mini-chat.audit` message `{"event_type":"turn_retry", tenant_id, actor_user_id, chat_id, original_request_id, new_request_id, timestamp}` in the mutation transaction, and the replacement's finalization adds one `turn_completed` event. An edit enqueues `turn_edit` with the same fields, and a delete `turn_delete` with `request_id`. A rejected or rolled-back mutation enqueues nothing, a replacement that fails setup keeps its `turn_retry` event but gets no `turn_failed` event, and no payload contains message content.
- [ ] Replaced request ids (feature-local): after a retry, `POST /messages:stream` with the old `request_id` returns 409 `request_id_conflict` without a stream, `GET /turns/{old}` returns 404, and a retry of the old `request_id` returns `NOT_LATEST_TURN`. `POST /messages:stream` with the new `request_id` replays the completed replacement (`is_new_turn` `false`, no provider call, no quota change), and a retry of the new `request_id` creates a further replacement whose old turn is the first replacement.
- [ ] Disconnect after the replacement commit (feature-local): a test hook drops the handler right after the replacement transaction commits. The new turn ends `cancelled` (or `failed` with a setup code), never `running`, and a following send to the chat is accepted.
- [ ] Authorization (operations of this feature): retry, edit and delete evaluate `retry_turn`, `edit_turn` and `delete_turn` once each, with the chat id as `resource.id`. PDP deny returns 403 `AUTHZ_DENIED` and PDP failure 503 with `Retry-After: 5`, both without changing anything. A turn whose `requester_user_id` is not the caller returns 403 `AUTHZ_DENIED`, and another user's chat returns 404, also when the PDP returns only a tenant predicate.
- [ ] Mutation metrics (feature-local): a committed retry records `mini_chat_turn_mutation{op="retry",result="ok"}` and one `mini_chat_turn_mutation_latency_ms{op="retry"}` sample; a `NOT_LATEST_TURN` delete records `{op="delete",result="not_latest_turn"}`; a post-commit context failure records `result="setup_failed"`; no label carries a tenant, user, chat or request id.
- [ ] Quality gates: `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass, and every implemented CDSL ID of this feature has `@cpt-begin` / `@cpt-end` markers.
