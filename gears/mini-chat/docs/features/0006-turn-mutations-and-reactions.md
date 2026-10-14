---
description: "Specifies the Turn Mutations and Reactions feature of the mini-chat gear: retry, edit and delete of the latest terminal turn (latest-turn rule, read-only preview, preflight before the mutation, one atomic mutation transaction with a server-generated request id, attachment and tool-usage carry-forward, deterministic resolution of concurrent mutations, thread-summary invalidation, mutation audit events through the audit outbox, post-commit setup failures), the canonical error mapping of these endpoints, and like or dislike reactions on assistant messages."
---

# Feature: Turn Mutations and Reactions

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-turn-mutations-and-reactions-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat User Retries the Last Turn](#chat-user-retries-the-last-turn)
  - [Chat User Edits the Last Turn](#chat-user-edits-the-last-turn)
  - [Chat User Deletes the Last Turn](#chat-user-deletes-the-last-turn)
  - [Chat User Sets a Reaction](#chat-user-sets-a-reaction)
  - [Chat User Removes a Reaction](#chat-user-removes-a-reaction)
  - [Admin Observes Turn Mutation Health](#admin-observes-turn-mutation-health)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Edit Request Validation](#edit-request-validation)
  - [Latest Turn Selection](#latest-turn-selection)
  - [Mutation Preview](#mutation-preview)
  - [Attachment Carry-Forward](#attachment-carry-forward)
  - [Tool-Usage Carry-Forward](#tool-usage-carry-forward)
  - [Mutation Preflight](#mutation-preflight)
  - [Mutation Lock and Re-check](#mutation-lock-and-re-check)
  - [Mutation Commit Transaction](#mutation-commit-transaction)
  - [Concurrent Mutation Resolution](#concurrent-mutation-resolution)
  - [Summary Invalidation on Mutation](#summary-invalidation-on-mutation)
  - [Mutation Audit Event](#mutation-audit-event)
  - [Post-Commit Setup](#post-commit-setup)
  - [Unstarted Replacement Turn Failure](#unstarted-replacement-turn-failure)
  - [Message Order and Count After a Mutation](#message-order-and-count-after-a-mutation)
  - [Reaction Request Validation](#reaction-request-validation)
  - [Reaction Target Resolution](#reaction-target-resolution)
  - [Reaction Upsert](#reaction-upsert)
  - [Reaction Removal](#reaction-removal)
  - [Turn Mutation and Reaction Error Contract](#turn-mutation-and-reaction-error-contract)
  - [Turn Mutation Metrics](#turn-mutation-metrics)
- [4. States (CDSL)](#4-states-cdsl)
  - [Turn Visibility State Machine](#turn-visibility-state-machine)
  - [Replacement Turn Setup State Machine](#replacement-turn-setup-state-machine)
  - [Message Reaction State Machine](#message-reaction-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Latest Terminal Turn Rule](#latest-terminal-turn-rule)
  - [Retry Last Turn Endpoint](#retry-last-turn-endpoint)
  - [Edit Last Turn Endpoint](#edit-last-turn-endpoint)
  - [Delete Last Turn Endpoint](#delete-last-turn-endpoint)
  - [Atomic Mutation Transaction](#atomic-mutation-transaction)
  - [Deterministic Concurrent Mutations](#deterministic-concurrent-mutations)
  - [Attachment and Tool-Usage Carry-Forward](#attachment-and-tool-usage-carry-forward)
  - [Thread Summary Invalidation on Mutation](#thread-summary-invalidation-on-mutation)
  - [Mutation Audit Events](#mutation-audit-events)
  - [Post-Commit Setup and Unstarted Replacement Turn](#post-commit-setup-and-unstarted-replacement-turn)
  - [Message Reactions API](#message-reactions-api)
  - [Error Contract and Observability](#error-contract-and-observability)
  - [Deferred and Not-Implemented Scope Stays Unimplemented](#deferred-and-not-implemented-scope-stays-unimplemented)
  - [Test Layering, Traceability and Feature-Local Deviations](#test-layering-traceability-and-feature-local-deviations)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

<!--
=============================================================================
FEATURE SPECIFICATION: Turn Mutations and Reactions (mini-chat)
Source entry: DECOMPOSITION section 2.6 (gears/mini-chat/docs/DECOMPOSITION.md)
Precedence: gears/mini-chat/docs/ (PRD, DESIGN, ADR, acceptance-criteria) wins over
config/mini-chat.yaml, the extracted reference OpenAPI and platform documents under
docs/ wherever they disagree.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

The Turn Mutations and Reactions feature lets a chat user correct the tail of a conversation and give feedback on answers in the `mini-chat` gear. It covers retry, edit and delete of the latest terminal turn (`POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH /v1/chats/{id}/turns/{request_id}`, `DELETE /v1/chats/{id}/turns/{request_id}`): the latest-turn rule, the read-only mutation preview, the mutation preflight that runs before anything is replaced, the single mutation transaction (soft-delete of the previous turn and its messages, the replacement user message with carried-forward attachment links, the new `running` turn with a server-generated `request_id`, thread-summary invalidation, the chat activity bump and the mutation audit event), the deterministic resolution of concurrent mutations, and the post-commit setup that hands the replacement turn to the unchanged send pipeline. It also covers like and dislike reactions on assistant messages (`PUT` and `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`).

### 1.2 Purpose

The PRD lets users fix a typo, rephrase a question or retry a poor answer without branching the conversation, and collect binary feedback on assistant answers. The DESIGN restricts mutations to the latest terminal turn (3.9 `Turn Mutation Rules (P1)`), fixes the preflight-before-mutation order so a rejection never destroys the previous answer, requires one atomic transaction for the latest-turn check, the terminal check, the soft-delete and the insert of the replacement turn, and requires every retry and edit to reuse the send pipeline, its SSE contract and its settlement path unchanged. This feature composes those rules into the five endpoints and owns every mutation write of `chat_turns`, `messages`, `message_attachments`, `thread_summaries` and `message_reactions`.

Scope boundary: the send pipeline steps reused by retry and edit (image guards, tool assembly, context assembly and truncation, provider resolution, provider call, SSE relay, turn finalization, the `turn_completed` and `turn_failed` audit events, the thread-summary trigger, idempotent replay, the parallel-turn guard, the turn status API and the `my_reaction` projection of the messages list) are owned by `cpt-cf-mini-chat-feature-message-streaming`. The quota preflight, the reserve write with its re-check and the settlement are contracts of `cpt-cf-mini-chat-feature-quota-and-billing`. The attachment lifecycle, the ready-document and ready-code-interpreter facts and the attachment deletion lock are owned by `cpt-cf-mini-chat-feature-attachments-and-file-search`. The tables, request admission, PEP, the canonical error mapping, the outbox queues and the audit delivery handler are owned by `cpt-cf-mini-chat-feature-gear-foundation`. The chat activity contract and `message_count` are owned by `cpt-cf-mini-chat-feature-chat-management`. The summary worker's commit check against a concurrent mutation is owned by `cpt-cf-mini-chat-feature-background-maintenance`. Those elements are referenced here, not redefined.

**Requirements**: `cpt-cf-mini-chat-fr-turn-mutations`, `cpt-cf-mini-chat-fr-message-reactions`. The feature also produces the turn-mutation audit events of `cpt-cf-mini-chat-fr-audit` (requirement owned by `cpt-cf-mini-chat-feature-gear-foundation`).

**Principles**: `cpt-cf-mini-chat-principle-linear-conversation`

**Constraints**: none owned (DECOMPOSITION 2.6). Every retry and edit preserves `cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-model-locked-per-chat` and `cpt-cf-mini-chat-constraint-model-image-capability`, which are owned by other features and verified here by mutation tests.

**Design elements**: `cpt-cf-mini-chat-design-turn-mutations` (3.9: definition, allowed operations, rules 1 to 9, API contracts, summary interaction, audit events), `cpt-cf-mini-chat-interface-message-reaction`, the retry and edit variant of `cpt-cf-mini-chat-seq-send-message`, `cpt-cf-mini-chat-design-turn-lifecycle` (unstarted retry and edit turn, soft-deleted turns), `cpt-cf-mini-chat-design-authz-pep` (actions `retry_turn`, `edit_turn`, `delete_turn`, `set_reaction`, `delete_reaction`), `cpt-cf-mini-chat-contract-sse-streaming` (same SSE contract), `cpt-cf-mini-chat-interface-turn-status` (404 for a soft-deleted turn).

**Database tables used (created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their mutation and reaction behavior)**: written: `cpt-cf-mini-chat-dbtable-chat-turns` (`deleted_at`, `replaced_by_request_id`, the replacement `running` turn, the unstarted-turn failure), `cpt-cf-mini-chat-dbtable-messages` (`deleted_at`, the replacement user message, `is_compressed`), `cpt-cf-mini-chat-dbtable-message-attachments` (copied links), `cpt-cf-mini-chat-dbtable-thread-summaries` (deletion), `cpt-cf-mini-chat-dbtable-message-reactions` (upsert and delete), `cpt-cf-mini-chat-dbtable-chats` (`updated_at` on retry and edit). Read only: `cpt-cf-mini-chat-dbtable-attachments`. The `cpt-cf-mini-chat-dbtable-quota-usage` rows change only through the contracts of `cpt-cf-mini-chat-feature-quota-and-billing`.

**Domain entities**: `MessageReaction`, turn mutation (`retry`, `edit`, `delete`), and the SDK audit payloads `TurnMutationAuditEvent` (`turn_retry`, `turn_edit`) and `TurnDeleteAuditEvent` (`turn_delete`).

**ADRs traced**: `cpt-cf-mini-chat-adr-canonical-error-contract` (Problem shape and the mutation and reaction rows), `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (mutation audit transport and event types, empty content fields, soft-deleted rows retained without hard purge), `cpt-cf-mini-chat-adr-quota-policy-scope` (reserve re-check on retry and edit leaves the turn `failed`), `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (unstarted retry and edit turn finalization path, application clock, CHECKs kept by repositories), `cpt-cf-mini-chat-adr-group-chat-usage-attribution` (the mutating user is the requester and is charged), `cpt-cf-mini-chat-adr-document-retrieval-scope` (retrieval over the entire chat vector store; a deleted document stays searchable until its provider file is removed).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Retries or edits the latest terminal turn of an own chat and receives the new answer as an SSE stream, deletes the latest terminal turn, and sets, changes or removes a like or dislike on assistant messages. Is charged for the replacement turn through the quota contracts. |
| `cpt-cf-mini-chat-actor-admin` | Observes turn mutation volume, outcomes and latency through the metric series of this feature and audit delivery health through the foundation series; never sees chat content and never sees tenant or user identifiers in metric labels. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (`Message Actions (P1 Scope)`, `Message Reactions (Like/Dislike)`, `Audit Events`, `Emitted: turn mutations`, UC-007, UC-008, UC-009)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) section 2.6 (`Turn Mutations and Reactions`)
- **Acceptance criteria**: [acceptance-criteria.md](../acceptance-criteria.md) (`Turn Mutations`, `Reactions API`)
- **ADRs (all accepted)**: [ADR-0003](../ADR/0003-cpt-cf-mini-chat-adr-group-chat-usage-attribution.md), [ADR-0004](../ADR/0004-cpt-cf-mini-chat-adr-canonical-error-contract.md), [ADR-0007](../ADR/0007-cpt-cf-mini-chat-adr-document-retrieval-scope.md), [ADR-0008](../ADR/0008-cpt-cf-mini-chat-adr-quota-policy-scope.md), [ADR-0009](../ADR/0009-cpt-cf-mini-chat-adr-data-lifecycle-audit-scope.md), [ADR-0010](../ADR/0010-cpt-cf-mini-chat-adr-runtime-consistency-limitations.md)
- **Reference OpenAPI** (operations `mini_chat.retry_turn`, `mini_chat.edit_turn`, `mini_chat.delete_turn`, `mini_chat.put_reaction`, `mini_chat.delete_reaction`; schemas `EditTurnRequest`, `SetReactionReq`, `MiniChatReactionDto`, `ReactionKindDto`): `.cf-studio/.cache/mini-chat/openapi-mini-chat.json`. The DESIGN wins where they differ.
- **Cross-feature implementation notes**: `.cf-studio/.cache/mini-chat/implementation-notes.md` items 1 and 2 (SQLite timestamp ordering: latest-turn selection, the summary frontier comparison and the replacement message timestamp use the fixed-width sort key with `id` as tie-break), 3 (repository functions take a runner so the mutation writes and the audit enqueue share one transaction; the outbox wake fires after commit), 4 (PEP, 404 masking of foreign chats), 6 (domain layering), 7 (audit handler wired by the foundation; thread-summary handler placeholder), 8 and 10 (`started_at` is the preflight instant; retry and edit use the `Existing` reserve path, which overwrites `started_at`; the requester is passed as `identity.user_id`), 9 (overflow-safe quota checks inside the quota service), 11 (user message `created_at` monotonicity and the streaming guidance, which applies unchanged to the replacement turn)
- **Sibling features**: [0001-gear-foundation.md](./0001-gear-foundation.md) (request admission, PEP, authorization matrix, error mapping, scoped persistence, outbox queues and transactional enqueue, audit delivery, policy snapshot access, provider registry), [0002-chat-management.md](./0002-chat-management.md) (activity contract on `chats.updated_at`, `message_count`), [0003-quota-and-billing.md](./0003-quota-and-billing.md) (`QuotaService::preflight`, `write_reserve` with `ReserveTurn::Existing`, rejection mapping, settlement), [0004-message-streaming.md](./0004-message-streaming.md) (send pipeline, SSE contract, finalization, turn audit event, replay and admission checks, turn status, message projection, orphan watchdog), [0005-attachments-and-file-search.md](./0005-attachments-and-file-search.md) (ready facts, tool contributions, attachment deletion lock)
- **Dependencies**: `cpt-cf-mini-chat-feature-message-streaming`, `cpt-cf-mini-chat-feature-attachments-and-file-search` (and through them `cpt-cf-mini-chat-feature-gear-foundation`, `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-quota-and-billing`). Feature that builds on this one: `cpt-cf-mini-chat-feature-background-maintenance` (the summary worker skips a commit whose target frontier message was soft-deleted by a mutation).

**Normative value sources** (the DESIGN holds the exact values; this document states the values it must preserve and names the DESIGN heading that carries the rest):

| Topic | DESIGN location (section and heading) |
|-------|---------------------------------------|
| Linear conversation | 2.1 `Linear Conversation Model` |
| Latest-turn definition, allowed operations, rules 1 to 9, API contracts | 3.9 `Definition`, `Allowed Operations`, `Rules`, `Turn Mutation API Contracts` |
| Summary interaction and mutation audit events | 3.9 `Summary Interaction on Turn Mutation`, `Audit Events for Turn Mutations` |
| Retry and edit order of steps | 3.6 `Send Message with Streaming Response` (`Retry / edit variant`) |
| Reaction endpoints | 3.3 `Message Reaction API` (`Set Reaction`, `Remove Reaction`) |
| Error rows (`NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`, `turn_state`, `reaction_target`, `INVALID_REACTION`, `EMPTY_CONTENT`, audit enqueue failure) | 3.3 `Error Codes`, `Streaming error codes`; ADR-0004 |
| Authorization actions and the single PDP evaluation of retry and edit | 3.8 `Per-Operation Authorization Matrix` |
| `chat_turns` soft-delete columns, preflight columns on retry and edit, indexes | 3.7 `Table: chat_turns` |
| `messages`, `message_attachments`, `thread_summaries`, `message_reactions` | 3.7 `Table: messages`, `Table: message_attachments`, `Table: thread_summaries`, `Table: message_reactions` |
| Unstarted retry and edit turn, billing classification of the setup codes | 5.7 `FinalizeTurn Invariant`; 5.8 `Internal Error Code Taxonomy (Normative)`, `Normative Billing Outcome Derivation (P1 Mandatory)`, `Pre-Provider Failure Handling` |
| Reserve re-check on retry and edit | 5.4.2 `TOCTOU`, 5.4.3 |
| Turn status of soft-deleted turns and the setup error codes | 3.3 `Turn Status API` |
| Turn mutation metrics | 4 `Metrics (OTLP) and Alerts (P1)` (`Emitted`) |
| Attachment deletion lock | 4 `Attachment Mutability and Deletion` |

## 2. Actor Flows (CDSL)

User-facing interactions of the chat user and the admin. Every flow runs behind the request admission flow `cpt-cf-mini-chat-flow-gear-foundation-request-admission` (authentication, license gate, PEP with the action of the authorization matrix, owner-scoped chat load and 404 masking of missing, soft-deleted and foreign chats).

**Use cases**: `cpt-cf-mini-chat-usecase-retry-turn`, `cpt-cf-mini-chat-usecase-edit-turn`, `cpt-cf-mini-chat-usecase-delete-turn`.

### Chat User Retries the Last Turn

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with `Content-Type: text/event-stream`; the stream follows the send contract exactly: `stream_started` with the new server-generated `request_id` (UUID v4), the pre-allocated assistant `message_id`, `is_new_turn` `true` and `thread_summary_applied` when a summary is in the context, then `ping`, `delta`, `tool` and `citations` events as on the send path, and one terminal `done` or `error`.
- After the commit the previous turn has `deleted_at` set and `replaced_by_request_id` equal to the new `request_id`, its messages are soft-deleted, the new user message carries the original content and the original non-deleted attachment links, one `turn_retry` audit event is enqueued and `chats.updated_at` moved forward.

**Error Scenarios**:
- Rejected before the commit, with the previous turn, its answer and the chat unchanged: 400 `invalid_path_params`; 401, 403 (`LICENSE_FEATURE_REQUIRED`, `AUTHZ_DENIED`) and 503 with `Retry-After: 5` from admission; 404 for the chat or the turn; 403 `AUTHZ_DENIED` when the turn's requester is not the caller; 409 `NOT_LATEST_TURN`; 400 `turn_state`; 400 `INVALID_MODEL`; 400 `TOO_MANY_IMAGES`; 400 `FEATURE_DISABLED` for `web_search` or `images`; 429 with scope `tokens`, `web_search` or `code_interpreter`; 400 `INPUT_TOO_LONG`; 400 `VISION_NOT_SUPPORTED`; 500 for a policy failure.
- Rejected at the commit, with everything rolled back: 409 `NOT_LATEST_TURN` (a concurrent mutation or send committed first), 409 `GENERATION_IN_PROGRESS` (the running-turn index), 500 `internal` (audit payload too large or a database error).
- Failed after the commit and before the reserve: a JSON Problem and the replacement turn `failed`: 400 `CONTEXT_BUDGET_EXCEEDED` (`context_length_exceeded`), 429 scope `tokens` (`quota_exceeded`), 500 `internal` (`turn_setup_failed`); the previous turn stays replaced.
- Failed after the stream opened: one terminal SSE `error` with a streaming error code, as on the send path.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats/{id}/turns/{request_id}/retry` with no body - `inst-retry-request`
2. [x] - `p1` - **IF** the chat id or the `request_id` in the path is not a UUID - `inst-retry-bad-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` `invalid_path_params` - `inst-retry-return-bad-path`
3. [x] - `p1` - Run request admission with the action `retry_turn` and the chat id as `resource.id`; the PDP is evaluated once, and the mutation, the quota preflight and the stream reuse that scope without another PDP call - `inst-retry-admission`
4. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-retry-refused`
   1. [x] - `p1` - **RETURN** 401, 403, 503 with `Retry-After: 5`, or 404 for the chat type, without opening a stream - `inst-retry-return-refusal`
5. [x] - `p1` - Create the turn's cancellation token bound to the response and run the remaining setup in a spawned task whose result is awaited, as the send flow does (`inst-send-cancel-token`, `inst-send-spawn-setup` of `cpt-cf-mini-chat-flow-message-streaming-send-message`) - `inst-retry-spawn-setup`
6. [x] - `p1` - Run the mutation preview algorithm with the operation `retry` and the path `request_id` - `inst-retry-preview`
7. [x] - `p1` - **IF** the preview rejected the mutation - `inst-retry-preview-rejected`
   1. [x] - `p1` - **RETURN** its JSON Problem (404, 403, 409 `NOT_LATEST_TURN`, 400 `turn_state` or 500); nothing was written - `inst-retry-return-preview`
8. [x] - `p1` - Run the mutation preflight algorithm with the content of the original user message as the new content - `inst-retry-preflight`
9. [x] - `p1` - **IF** the preflight rejected the mutation - `inst-retry-preflight-rejected`
   1. [x] - `p1` - **RETURN** its JSON Problem; the previous turn, its answer and the chat stay unchanged and no reserve exists - `inst-retry-return-preflight`
10. [x] - `p1` - Generate the new `request_id` on the server as a UUID v4; the client never supplies it - `inst-retry-new-request-id`
11. [x] - `p1` - Run the mutation commit transaction algorithm with the operation `retry`, the original content and the new `request_id` - `inst-retry-commit`
12. [x] - `p1` - **IF** the commit was rejected - `inst-retry-commit-rejected`
    1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` or `GENERATION_IN_PROGRESS`, or 500 `internal`; the transaction rolled back, so nothing changed - `inst-retry-return-commit`
13. [x] - `p1` - Run the post-commit setup algorithm for the replacement turn (tool assembly, context assembly, provider resolution and the reserve through the `Existing` path) - `inst-retry-setup`
14. [x] - `p1` - **IF** the setup failed - `inst-retry-setup-failed`
    1. [x] - `p1` - **RETURN** its JSON Problem (400 `CONTEXT_BUDGET_EXCEEDED`, 429 scope `tokens` or 500 `internal`); the unstarted replacement turn failure algorithm has already marked the new turn `failed`, and no stream is opened - `inst-retry-return-setup`
15. [x] - `p1` - **IF** the cancellation token is already cancelled after the reserve commit - `inst-retry-dropped`
    1. [x] - `p1` - Send no provider request and finalize the replacement turn as `cancelled` exactly as `inst-send-dropped-before-provider` of `cpt-cf-mini-chat-flow-message-streaming-send-message` (estimated settlement, one `aborted` usage event, one `turn_failed` audit event) and **RETURN** without writing any SSE event - `inst-retry-return-dropped`
16. [x] - `p1` - Continue with the send flow from `inst-send-stream-started` to `inst-send-return-stream`: `stream_started` with the new `request_id`, the pre-allocated assistant message id and `is_new_turn` `true`, the provider task, the relay, the finalization with `Finalizer::Stream` and the terminal event; no separate settlement or outbox path exists for retry - `inst-retry-stream`
17. [x] - `p1` - **RETURN** the closed stream - `inst-retry-return`

### Chat User Edits the Last Turn

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with the same SSE stream as on retry; `stream_started` carries the new server-generated `request_id`.
- After the commit the previous turn is replaced as on retry, the new user message carries the edited `content` exactly as sent and the original non-deleted attachment links, one `turn_edit` audit event is enqueued and `chats.updated_at` moved forward.

**Error Scenarios**:
- Body errors before admission: 415 `missing_json_content_type`, 400 `json_syntax_error`, 422 `invalid_json_body`, 400 `EMPTY_CONTENT` (field `content`), 400 `invalid_path_params`.
- Every error scenario of the retry flow, with the same codes and the same before-commit, at-commit and after-commit guarantees.

**Steps**:
1. [x] - `p1` - Chat User sends `PATCH /v1/chats/{id}/turns/{request_id}` with a JSON body carrying only `content` - `inst-edit-request`
2. [x] - `p1` - Validate the request through the edit request validation algorithm - `inst-edit-validate`
3. [x] - `p1` - **IF** validation failed - `inst-edit-invalid`
   1. [x] - `p1` - **RETURN** the Problem of the validation algorithm (400, 415 or 422) without opening a stream - `inst-edit-return-invalid`
4. [x] - `p1` - Run request admission with the action `edit_turn` and the chat id as `resource.id`, evaluated once and reused by the mutation, the preflight and the stream - `inst-edit-admission`
5. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-edit-refused`
   1. [x] - `p1` - **RETURN** 401, 403, 503 with `Retry-After: 5`, or 404 for the chat type, without opening a stream - `inst-edit-return-refusal`
6. [x] - `p1` - Create the cancellation token and run the remaining setup in an awaited spawned task, as on retry - `inst-edit-spawn-setup`
7. [x] - `p1` - Run the mutation preview algorithm with the operation `edit` and **RETURN** its JSON Problem when it rejects - `inst-edit-preview`
8. [x] - `p1` - Run the mutation preflight algorithm with the validated `content` as the new content and **RETURN** its JSON Problem when it rejects; the previous turn stays unchanged - `inst-edit-preflight`
9. [x] - `p1` - Generate the new `request_id` on the server as a UUID v4 - `inst-edit-new-request-id`
10. [x] - `p1` - Run the mutation commit transaction algorithm with the operation `edit`, the validated `content` and the new `request_id`, and **RETURN** 409 `NOT_LATEST_TURN`, 409 `GENERATION_IN_PROGRESS` or 500 when it rejects - `inst-edit-commit`
11. [x] - `p1` - Run the post-commit setup algorithm and **RETURN** its JSON Problem when it fails; the new turn is already `failed` - `inst-edit-setup`
12. [x] - `p1` - **IF** the cancellation token is already cancelled after the reserve commit, finalize the replacement turn as `cancelled` as on retry and **RETURN** without writing any SSE event - `inst-edit-dropped`
13. [x] - `p1` - Continue with the send flow from `inst-send-stream-started` to `inst-send-return-stream`, with identical ordering rules, terminal semantics, error mapping, downgrade metadata and billing finalization; no separate settlement or outbox path exists for edit - `inst-edit-stream`
14. [x] - `p1` - **RETURN** the closed stream - `inst-edit-return`

### Chat User Deletes the Last Turn

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 204 with no body; the turn has `deleted_at` set (no `replaced_by_request_id`), its user and assistant messages are soft-deleted and disappear from the messages list and from `message_count`, its turn status returns 404, the previous non-deleted turn becomes the latest turn, and one `turn_delete` audit event is enqueued.

**Error Scenarios**:
- 400 `invalid_path_params`; 401, 403, 503 with `Retry-After: 5` from admission; 404 for the chat or the turn; 403 `AUTHZ_DENIED` when the turn's requester is not the caller; 409 `NOT_LATEST_TURN` (not the latest, already replaced or already deleted, or lost to a concurrent mutation); 400 `turn_state` for a `running` turn; 500 `internal` for an oversized audit payload or a database error. No preflight runs and no stream is opened.

**Steps**:
1. [x] - `p1` - Chat User sends `DELETE /v1/chats/{id}/turns/{request_id}` with no body - `inst-del-request`
2. [x] - `p1` - **IF** the chat id or the `request_id` in the path is not a UUID - `inst-del-bad-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` `invalid_path_params` - `inst-del-return-bad-path`
3. [x] - `p1` - Run request admission with the action `delete_turn` and the chat id as `resource.id` - `inst-del-admission`
4. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-del-refused`
   1. [x] - `p1` - **RETURN** 401, 403, 503 with `Retry-After: 5`, or 404 for the chat type - `inst-del-return-refusal`
5. [x] - `p1` - Run the mutation preview algorithm with the operation `delete` and **RETURN** its JSON Problem when it rejects - `inst-del-preview`
6. [x] - `p1` - Run the mutation commit transaction algorithm with the operation `delete`; no quota preflight, no reserve and no provider call take place - `inst-del-commit`
7. [x] - `p1` - **IF** the commit was rejected - `inst-del-commit-rejected`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` or 500 `internal`; nothing changed - `inst-del-return-commit`
8. [x] - `p1` - **RETURN** 204 with no body - `inst-del-return-204`

### Chat User Sets a Reaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with `message_id`, `reaction` (`like` or `dislike`) and `created_at`; the next messages list shows the value in `my_reaction` of that assistant message.
- Sending the same value again, or switching between `like` and `dislike`, leaves exactly one reaction row for the user and the message (upsert).

**Error Scenarios**:
- Before authorization: 400 `invalid_path_params`, 415 `missing_json_content_type`, 400 `json_syntax_error`, 422 `invalid_json_body` (for example no `reaction` field), 400 `INVALID_REACTION` (field `reaction`).
- 401, 403, 503 with `Retry-After: 5` from admission; 404 `not_found` with the chat type for a missing, soft-deleted or foreign chat; 404 with the message type for a message that is not a non-deleted message of the chat; 400 `failed_precondition` `reaction_target` / `STATE` for a user or system message; 500 for a database error.

**Steps**:
1. [x] - `p1` - Chat User sends `PUT /v1/chats/{id}/messages/{msg_id}/reaction` with a JSON body `{"reaction": "like"}` or `{"reaction": "dislike"}` - `inst-setr-request`
2. [x] - `p1` - Validate the request through the reaction request validation algorithm, before authorization - `inst-setr-validate`
3. [x] - `p1` - **IF** validation failed - `inst-setr-invalid`
   1. [x] - `p1` - **RETURN** the Problem of the validation algorithm (400, 415 or 422) - `inst-setr-return-invalid`
4. [x] - `p1` - Run request admission with the action `set_reaction` and the chat id as `resource.id` - `inst-setr-admission`
5. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-setr-refused`
   1. [x] - `p1` - **RETURN** 401, 403, 503 with `Retry-After: 5`, or 404 for the chat type - `inst-setr-return-refusal`
6. [x] - `p1` - Resolve the target through the reaction target resolution algorithm and **RETURN** its 404 or 400 Problem when it rejects - `inst-setr-target`
7. [x] - `p1` - Store the reaction through the reaction upsert algorithm - `inst-setr-upsert`
8. [x] - `p1` - **RETURN** 200 with `message_id`, `reaction` and `created_at` of the stored row - `inst-setr-return-200`

### Chat User Removes a Reaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 204 with no body whether or not a reaction existed; the next messages list shows `my_reaction` `null` for that assistant message.

**Error Scenarios**:
- 400 `invalid_path_params`; 401, 403, 503 with `Retry-After: 5`; 404 for the chat or the message; 400 `failed_precondition` `reaction_target` / `STATE` for a user or system message, exactly as on `PUT`; 500 for a database error.

**Steps**:
1. [x] - `p1` - Chat User sends `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` with no body - `inst-delr-request`
2. [x] - `p1` - Validate the path parameters through the reaction request validation algorithm and **RETURN** 400 `invalid_path_params` when one is not a UUID - `inst-delr-validate`
3. [x] - `p1` - Run request admission with the action `delete_reaction` and the chat id as `resource.id` - `inst-delr-admission`
4. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-delr-refused`
   1. [x] - `p1` - **RETURN** 401, 403, 503 with `Retry-After: 5`, or 404 for the chat type - `inst-delr-return-refusal`
5. [x] - `p1` - Resolve the target through the reaction target resolution algorithm and **RETURN** its 404 or 400 Problem when it rejects - `inst-delr-target`
6. [x] - `p1` - Remove the caller's reaction through the reaction removal algorithm - `inst-delr-remove`
7. [x] - `p1` - **RETURN** 204 with no body - `inst-delr-return-204`

### Admin Observes Turn Mutation Health

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability`

**Actor**: `cpt-cf-mini-chat-actor-admin`

**Success Scenarios**:
- The admin sees mutation volume per operation and outcome, mutation latency per operation, and audit delivery health, with low-cardinality labels and no identifiers or content.

**Error Scenarios**:
- No plugin is registered for audit: mutation audit events are counted as `dropped` by the foundation's audit delivery, and chat traffic is not blocked.

**Steps**:
1. [x] - `p1` - Admin reads `mini_chat_turn_mutation` by `op` (`retry`, `edit`, `delete`) and `result` from the turn mutation metrics algorithm - `inst-obs-mutation-counter`
2. [x] - `p1` - Admin reads `mini_chat_turn_mutation_latency_ms` by `op` - `inst-obs-mutation-latency`
3. [x] - `p1` - Admin checks the delivery of `turn_retry`, `turn_edit` and `turn_delete` events through `mini_chat_audit_emit` of the audit delivery algorithm of `cpt-cf-mini-chat-feature-gear-foundation` - `inst-obs-audit-health`
4. [x] - `p1` - Admin correlates a failed replacement turn through the logs (chat id, original and new `request_id`, error code), never through message content - `inst-obs-logs`
5. [x] - `p1` - **RETURN** the observed signals - `inst-obs-return`

## 3. Processes / Business Logic (CDSL)

Internal functions and procedures that the flows above call. Every database step runs through the scoped persistence algorithm of `cpt-cf-mini-chat-feature-gear-foundation` (tenant predicate, child tables reached through the chat id of the owner-scoped chat load), and repository functions take the caller's transaction runner so the writes and the outbox enqueue share one transaction.

### Edit Request Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation`

**Input**: the raw request of `PATCH /v1/chats/{id}/turns/{request_id}` (path, content type, body)

**Output**: the validated `content`, or a Problem. The body schema is `EditTurnRequest`; the reasons are DESIGN 3.3 `Error Codes`.

**Steps**:
1. [x] - `p1` - **IF** the chat id or the `request_id` in the path is not a UUID - `inst-ev-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` `invalid_path_params` - `inst-ev-return-path`
2. [x] - `p1` - **IF** the body has no JSON `Content-Type` - `inst-ev-content-type`
   1. [x] - `p1` - **RETURN** 415 `invalid_argument` with `field_violations[body].reason` `missing_json_content_type` - `inst-ev-return-415`
3. [x] - `p1` - **IF** the body is not valid JSON - `inst-ev-json-syntax`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[body].reason` `json_syntax_error` - `inst-ev-return-syntax`
4. [x] - `p1` - **IF** the body does not match the schema (no `content`, or `content` not a string) - `inst-ev-schema`
   1. [x] - `p1` - **RETURN** 422 `invalid_argument` with `field_violations[body].reason` `invalid_json_body` - `inst-ev-return-422`
5. [x] - `p1` - **IF** `content` is empty or whitespace-only - `inst-ev-empty`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[content].reason` `EMPTY_CONTENT` - `inst-ev-return-empty`
6. [x] - `p1` - Ignore unknown body fields: the edit body accepts only `content`, so the client can choose neither the new `request_id` nor the attachments of the replacement message - `inst-ev-content-only`
7. [x] - `p1` - Keep the content as sent: it is never trimmed, rewritten or truncated (only the emptiness check trims) - `inst-ev-content-kept`
8. [x] - `p1` - **RETURN** the validated content; these checks run before the PEP call and before the chat is loaded, as on the send path - `inst-ev-return`

### Latest Turn Selection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn`

**Input**: the scoped chat (and, inside the mutation transaction, the open transaction)

**Output**: the latest turn of the chat, or none. The definition is DESIGN 3.9 `Definition` and 3.7 `Table: chat_turns` (soft-delete rules).

**Steps**:
1. [x] - `p1` - DB: SELECT the `chat_turns` rows of the scoped chat with `deleted_at IS NULL`, ordered by `started_at` descending and then `id` descending, and take the first row; the partial index `(chat_id, started_at DESC) WHERE deleted_at IS NULL` serves the query - `inst-lt-select`
2. [x] - `p1` - Compare `started_at` on SQLite through the fixed-width sort key of the chat page module (never raw text comparison of variable-length RFC 3339 values) and with the native type on PostgreSQL, always with `id` as the deterministic tie-break - `inst-lt-sort-key`
3. [x] - `p1` - Count every state: a newer `running` turn is the latest turn, and so is a newer `completed`, `failed` or `cancelled` turn - `inst-lt-every-state`
4. [x] - `p1` - Never count a soft-deleted turn: a replaced or deleted turn is never the latest, and after a delete the previous non-deleted turn becomes the latest - `inst-lt-skip-deleted`
5. [x] - `p1` - **RETURN** the latest turn, or none for a chat without non-deleted turns - `inst-lt-return`

### Mutation Preview

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`

**Input**: the scoped chat, the caller's user id, the path `request_id` and the operation (`retry`, `edit` or `delete`)

**Output**: the target turn and its user message, or a rejection. The rules are DESIGN 3.9 rules 1, 2, 3 and 9; the step is the read-only "mutation preview" of DESIGN 3.6 (`Retry / edit variant`, step 1). It writes nothing and takes no lock.

**Steps**:
1. [x] - `p1` - DB: SELECT the `chat_turns` row with the scoped `chat_id` and the path `request_id`, including a soft-deleted row - `inst-mp-select-target`
2. [x] - `p1` - **IF** no row exists - `inst-mp-missing`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.turn.v1~` - `inst-mp-return-404`
3. [x] - `p1` - **IF** `requester_user_id` of the row is not the caller (a NULL requester included) - `inst-mp-not-owner`
   1. [x] - `p1` - **RETURN** 403 `permission_denied` with `context.reason` `AUTHZ_DENIED`; a turn in another user's chat never reaches this step because the chat load already returned 404 - `inst-mp-return-403`
4. [x] - `p1` - **IF** the row is soft-deleted (`deleted_at` set: already replaced by retry or edit, or deleted) - `inst-mp-deleted`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-mp-return-deleted`
5. [x] - `p1` - **IF** the row's `state` is `running` (checked before the latest-turn check) - `inst-mp-running`
   1. [x] - `p1` - **RETURN** 400 `failed_precondition` with `violations[{subject: turn_state, type: STATE}]`; the client waits for completion or cancels by disconnecting the stream - `inst-mp-return-running`
6. [x] - `p1` - Select the latest turn through the latest turn selection algorithm - `inst-mp-latest`
7. [x] - `p1` - **IF** the latest turn is not the target (a newer turn exists, also when that newer turn is still `running`) - `inst-mp-not-latest`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-mp-return-not-latest`
8. [x] - `p1` - DB: SELECT the target turn's user message: the `messages` row with the scoped `chat_id`, the target `request_id`, `role` `user` and `deleted_at IS NULL` - `inst-mp-select-user-message`
9. [x] - `p1` - **IF** the user message is missing - `inst-mp-user-message-missing`
   1. [x] - `p1` - Log the inconsistency and **RETURN** 500 `internal` with a generic detail - `inst-mp-return-500`
10. [x] - `p1` - Use a generic `detail` on every 409 and log the internal cause (turn ids) only - `inst-mp-generic-detail`
11. [x] - `p1` - **RETURN** the target turn (state `completed`, `failed` or `cancelled`) and its user message - `inst-mp-return`

### Attachment Carry-Forward

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward`

**Input**: the target turn's user message and the scoped chat

**Output**: the ordered list of carried attachments and its image subset. The rules are DESIGN 3.9 `Allowed Operations` (Retry, Edit: copy, deleted attachment handling, image re-send) and 3.7 `Table: message_attachments` (Semantics).

**Steps**:
1. [x] - `p1` - DB: SELECT the `message_attachments` rows of the original user message joined to `attachments` on `(attachment_id, chat_id)`, within the tenant and the scoped chat - `inst-acf-select`
2. [x] - `p1` - Keep only attachments with `deleted_at IS NULL`; silently exclude every attachment deleted since the original turn (no error, no warning to the client) - `inst-acf-exclude-deleted`
3. [x] - `p1` - Do not run the `attachment_ids` validation of the send pipeline again: the links were validated when the original message was sent, and retry and edit accept no `attachment_ids` from the client - `inst-acf-no-revalidation`
4. [x] - `p1` - Order the carried attachments by the original link's `created_at`, then by `attachment_id`, so the image input order is deterministic - `inst-acf-order`
5. [x] - `p1` - Classify each carried attachment by `attachment_kind`: images become model input of the replacement turn; documents and code-interpreter files stay associations only, because retrieval covers the entire chat vector store and the tool inclusion comes from the chat's ready facts - `inst-acf-classify`
6. [x] - `p1` - Never carry links of any other message and never add an attachment uploaded after the original message - `inst-acf-only-original`
7. [x] - `p1` - **RETURN** the carried list and its images - `inst-acf-return`

### Tool-Usage Carry-Forward

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`

**Input**: the target turn and the scoped chat

**Output**: the tool request flags of the replacement turn and the rules that keep the tool-usage history of the replaced turn intact. The rules are DESIGN 3.7 `Table: chat_turns` (`web_search_enabled`: retry and edit reuse it; the completed counters), 5.7 `FinalizeTurn Invariant` and 3.9 rule 5.

**Steps**:
1. [x] - `p1` - Set `web_search.enabled` of the replacement turn to the target turn's `web_search_enabled`; the client cannot change it on retry or edit - `inst-tcf-web-search-flag`
2. [x] - `p1` - Leave `web_search_completed_count`, `code_interpreter_completed_count` and `file_search_completed_count` of the replaced turn unchanged, kept for audit on the soft-deleted row; the replacement turn starts with all three counters at 0 - `inst-tcf-counters`
3. [x] - `p1` - Never refund, re-settle or re-publish the replaced turn's quota, daily tool counters, usage event or audit event: that turn was finalized exactly once, and a mutation adds no settlement for it - `inst-tcf-no-refund`
4. [x] - `p1` - Decide the `file_search`, `code_interpreter` and `search_knowledge` tools of the replacement turn afresh through the tool assembly of `cpt-cf-mini-chat-algo-message-streaming-tool-assembly`, using the chat's current facts from `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact` and `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact` and the effective model of the new decision; `web_search` is added when the carried flag is set and the effective model supports it - `inst-tcf-tools-afresh`
5. [x] - `p1` - Apply the daily web search and code interpreter quota checks and the per-message tool limits to the replacement turn exactly as to a new send - `inst-tcf-limits`
6. [x] - `p1` - **RETURN** the carried web search flag - `inst-tcf-return`

### Mutation Preflight

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight`

**Input**: the security context, the scoped chat, the target turn and its user message, and the content of the replacement message (the original content on retry, the validated content on edit)

**Output**: the prepared mutation (preflight decision with its preflight instant, carried attachments and images, carried web search flag, snapshot boundary), or a JSON Problem. This is DESIGN 3.9 rule 8 and 3.6 (`Retry / edit variant`, step 2). It writes nothing, takes no reserve and calls no provider, so a rejection leaves the previous turn and the chat unchanged.

**Steps**:
1. [x] - `p1` - Resolve the chat's model (`chats.model`) in the current catalog without the enabled filter through the policy snapshot access of `cpt-cf-mini-chat-feature-gear-foundation` - `inst-mpf-resolve-model`
2. [x] - `p1` - **IF** the model is no longer in the catalog - `inst-mpf-model-missing`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[model].reason` `INVALID_MODEL` - `inst-mpf-return-invalid-model`
3. [x] - `p1` - **IF** the policy plugin cannot be resolved or fails - `inst-mpf-policy-failed`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail - `inst-mpf-return-policy-500`
4. [x] - `p1` - Run the attachment carry-forward algorithm and the tool-usage carry-forward algorithm - `inst-mpf-carry-forward`
5. [x] - `p1` - DB: compute the snapshot boundary `(max_included_created_at, max_included_id)` once as the latest non-deleted message of the chat, in the order `(created_at, id)`, whose `request_id` differs from the target turn's, so the context holds the history before the mutated turn; compare through the fixed-width sort key on SQLite; the boundary is immutable for the request - `inst-mpf-snapshot-boundary`
6. [x] - `p1` - **IF** the number of carried images exceeds `rag.max_images_per_message` (default 4) - `inst-mpf-too-many-images`
   1. [x] - `p1` - **RETURN** 400 `out_of_range` with `field_violations[image_count].reason` `TOO_MANY_IMAGES` - `inst-mpf-return-too-many-images`
7. [x] - `p1` - DB: read the chat facts the quota estimate needs: the ready-document fact and the ready-code-interpreter fact of `cpt-cf-mini-chat-feature-attachments-and-file-search` - `inst-mpf-read-facts`
8. [x] - `p1` - Call `QuotaService::preflight` of `cpt-cf-mini-chat-feature-quota-and-billing` with the chat id, the selected model and the request facts (UTF-8 byte length of the replacement content, carried image count, ready documents, ready code-interpreter attachments, carried `web_search.enabled`), passing the caller as `identity.user_id` (the requester who is charged) - `inst-mpf-quota-preflight`
9. [x] - `p1` - **IF** the preflight rejected - `inst-mpf-quota-rejected`
   1. [x] - `p1` - **RETURN** the Problem of `cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping` unchanged: 400 `failed_precondition` `FEATURE_DISABLED` for `web_search`, 429 `resource_exhausted` with scope `tokens`, `web_search` or `code_interpreter`, or 500 `internal` - `inst-mpf-return-quota`
10. [x] - `p1` - **IF** the decision reports the replacement content as too long (a non-zero `max_input_tokens` of the effective model below `estimated_text_tokens`) - `inst-mpf-input-too-long`
    1. [x] - `p1` - **RETURN** 400 `out_of_range` with `field_violations[content].reason` `INPUT_TOO_LONG` - `inst-mpf-return-input-too-long`
11. [x] - `p1` - Run `cpt-cf-mini-chat-algo-message-streaming-image-guards` with the carried images and the effective model of the decision, and **RETURN** its Problem when it rejects (400 `FEATURE_DISABLED` for `images`, 400 `VISION_NOT_SUPPORTED`); images are never dropped to make the request pass - `inst-mpf-image-guards`
12. [x] - `p1` - Keep the decision, including its preflight instant and the period starts derived from it, for the mutation commit and the reserve write; the preflight is not run again after the commit - `inst-mpf-keep-decision`
13. [x] - `p1` - **RETURN** the prepared mutation; no row was written, no reserve was taken and no provider request was sent - `inst-mpf-return`

### Mutation Lock and Re-check

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck`

**Input**: the open mutation transaction, the scoped chat and the target turn of the preview

**Output**: the confirmed target, or a rejection. This is DESIGN 3.9 rule 7 (atomicity invariant): the latest-turn check and the terminal check run again inside the transaction that soft-deletes and inserts.

**Steps**:
1. [x] - `p1` - On PostgreSQL, DB: SELECT the target `chat_turns` row FOR UPDATE, so concurrent mutations of the same turn queue on the row lock; on SQLite, run the mutation transaction as a write transaction that holds the database write lock before its first read, so concurrent mutations are serialized - `inst-lr-lock`
2. [x] - `p1` - DB: re-read the target row inside the transaction - `inst-lr-reread`
3. [x] - `p1` - **IF** the row is now soft-deleted (a concurrent retry, edit or delete committed first) - `inst-lr-deleted`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-lr-return-deleted`
4. [x] - `p1` - **IF** the row's `state` is not `completed`, `failed` or `cancelled` - `inst-lr-not-terminal`
   1. [x] - `p1` - **RETURN** 400 `failed_precondition` with `violations[{subject: turn_state, type: STATE}]` - `inst-lr-return-not-terminal`
5. [x] - `p1` - Select the latest turn through the latest turn selection algorithm inside the transaction - `inst-lr-latest`
6. [x] - `p1` - **IF** the latest turn is not the target (a newer turn committed after the preview, for example by a concurrent send) - `inst-lr-not-latest`
   1. [x] - `p1` - **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-lr-return-not-latest`
7. [x] - `p1` - **RETURN** the confirmed target - `inst-lr-return`

### Mutation Commit Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit`

**Input**: the operation, the scoped chat, the caller, the confirmed target turn and its user message, and for retry and edit the prepared mutation, the replacement content and the new server-generated `request_id`

**Output**: the committed mutation (for retry and edit the new turn id, the new `request_id` and the new user message), or a rejection with everything rolled back. This realizes DESIGN 3.9 `Allowed Operations`, rules 4 to 7 and 3.6 (`Retry / edit variant`, step 3).

**Steps**:
1. [x] - `p1` - Open one database transaction - `inst-mc-begin`
2. [x] - `p1` - Run the mutation lock and re-check algorithm and, when it rejects, roll back and **RETURN** its rejection - `inst-mc-recheck`
3. [x] - `p1` - DB: UPDATE the target `chat_turns` row setting `deleted_at` and `updated_at` to the current time and, for retry and edit, `replaced_by_request_id` to the new `request_id` (delete leaves it NULL), guarded by `deleted_at IS NULL` - `inst-mc-soft-delete-turn`
4. [x] - `p1` - **IF** the guarded update affected no row - `inst-mc-soft-delete-lost`
   1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-mc-return-soft-delete-lost`
5. [x] - `p1` - DB: UPDATE every `messages` row of the scoped chat with the target `request_id` and `deleted_at IS NULL` (the user message and the assistant or partial assistant message) setting `deleted_at` to the current time; leave their `message_attachments` and `message_reactions` rows untouched, kept for audit and invisible through the deleted messages - `inst-mc-soft-delete-messages`
6. [x] - `p1` - **IF** the operation is `retry` or `edit` - `inst-mc-replace`
   1. [x] - `p1` - DB: INSERT the replacement user `messages` row: a new UUID v4 `id`, `role` `user`, the new `request_id`, `content` equal to the original content on retry or the validated content on edit, `content_type` `text`, `token_estimate` 0, `request_kind` `chat`, `features_used` `[]`, token columns 0, `model` NULL, `is_compressed` false, and `created_at` from the message order algorithm - `inst-mc-insert-user-message`
   2. [x] - `p1` - DB: INSERT one `message_attachments` row per carried attachment with `tenant_id`, `chat_id` taken from the replacement message row (never from input), the replacement `message_id`, the `attachment_id` and `created_at`; deleted attachments were excluded by the carry-forward, and the original rows stay on the soft-deleted message - `inst-mc-copy-links`
   3. [x] - `p1` - DB: INSERT the replacement `chat_turns` row: `state` `running`, `requester_type` `user`, `requester_user_id` the caller, the new `request_id`, `started_at` equal to the preflight instant of the mutation preflight decision, `last_progress_at` equal to the insert instant, `web_search_enabled` equal to the carried flag, the six preflight columns NULL, `provider_name` NULL, counters 0, `completed_at` NULL and `deleted_at` NULL - `inst-mc-insert-turn`
   4. [x] - `p1` - **IF** the turn insert violated `UNIQUE(chat_id) WHERE state = 'running' AND deleted_at IS NULL` - `inst-mc-running-race`
      1. [x] - `p1` - Roll back and **RETURN** 409 `aborted` with `context.reason` `GENERATION_IN_PROGRESS`, never 500 - `inst-mc-return-generation-in-progress`
   5. [x] - `p1` - **IF** an insert violated `UNIQUE(chat_id, request_id)` on `chat_turns` or the `(chat_id, request_id, role)` index on `messages` (a server UUID collision) - `inst-mc-key-collision`
      1. [x] - `p1` - Roll back and **RETURN** 500 `internal` with a generic detail - `inst-mc-return-key-collision`
   6. [x] - `p1` - DB: UPDATE `chats.updated_at` to the current time in the same transaction (the message-activity contract of `cpt-cf-mini-chat-algo-chat-management-activity-ordering`) - `inst-mc-activity`
7. [x] - `p1` - Leave `chats.updated_at` unchanged for `delete`; the activity contract covers sent messages, retry and edit only - `inst-mc-delete-no-activity`
8. [x] - `p1` - Run the summary invalidation algorithm in the same transaction, after the soft-deletes - `inst-mc-summary`
9. [x] - `p1` - Build the event through the mutation audit event algorithm and enqueue it to the audit queue in the same transaction - `inst-mc-audit`
10. [x] - `p1` - **IF** the audit payload exceeds the outbox payload size limit - `inst-mc-payload-too-large`
    1. [x] - `p1` - Roll back and **RETURN** 500 `internal` (not the 400 of chat deletion) - `inst-mc-return-payload-500`
11. [x] - `p1` - **IF** any other database error occurred - `inst-mc-db-error`
    1. [x] - `p1` - Roll back and **RETURN** 500 `internal` with a generic detail - `inst-mc-return-500`
12. [x] - `p1` - Commit the transaction, then fire the outbox wake and record the turn mutation metrics - `inst-mc-commit`
13. [x] - `p1` - Never split the mutation: the soft-deletes, the replacement message and links, the replacement turn, the summary deletion, the activity bump and the audit enqueue commit together or not at all, so a rejection leaves the previous turn, its messages, the summary and the chat unchanged - `inst-mc-atomic`
14. [x] - `p1` - **RETURN** the committed mutation - `inst-mc-return`

### Concurrent Mutation Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution`

**Input**: two or more retry, edit or delete requests for the same chat, possibly racing with a send of `cpt-cf-mini-chat-feature-message-streaming`

**Output**: exactly one winner and a deterministic 409 for every loser. The rules are DESIGN 3.9 rules 1 and 7 and 3.3 `Error Codes`.

**Steps**:
1. [x] - `p1` - Let any number of requests pass the preview and the preflight concurrently; neither holds a lock or writes a row, so a loser has nothing to undo - `inst-cr-no-early-lock`
2. [x] - `p1` - Serialize the mutation transactions on the target row lock (PostgreSQL) or on the database write lock (SQLite); the first transaction to commit wins, and only its result is visible - `inst-cr-first-commit-wins`
3. [x] - `p1` - **MATCH** what the loser observes in its own transaction - `inst-cr-match`
   - [x] - `p1` - **CASE** the target is already soft-deleted by the winner (retry, edit or delete): **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-cr-case-deleted`
   - [x] - `p1` - **CASE** a newer turn committed by the winner or by a concurrent send makes the target non-latest: **RETURN** 409 `aborted` with `context.reason` `NOT_LATEST_TURN` - `inst-cr-case-newer`
   - [x] - `p1` - **CASE** the loser's replacement turn insert hits the running-turn unique index (the winner's replacement turn or a concurrent send's turn is `running`): **RETURN** 409 `aborted` with `context.reason` `GENERATION_IN_PROGRESS` - `inst-cr-case-running-index`
4. [x] - `p1` - Roll back the loser's whole transaction: no soft-delete, no message, no link, no turn, no summary change, no activity bump and no audit event remain, and no reserve exists because its preflight wrote nothing - `inst-cr-loser-clean`
5. [x] - `p1` - Let a send that races with a committed mutation be rejected by the send pipeline's own guards (409 `turn_already_running` through the parallel-turn guard or the running index, 409 `request_id_conflict` when it reuses the replaced turn's `request_id`) - `inst-cr-send-race`
6. [x] - `p1` - Never map a lost race to 500 and never retry it on the server side; the client reads the latest state (messages list or turn status) and decides - `inst-cr-no-server-retry`
7. [x] - `p1` - **RETURN** the winner's committed mutation or the loser's 409 - `inst-cr-return`

### Summary Invalidation on Mutation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation`

**Input**: the open mutation transaction, the scoped chat and the `(created_at, id)` of the target turn's user message

**Output**: `invalidated` or `kept`. The rule is DESIGN 3.9 `Summary Interaction on Turn Mutation` and 3.7 `Table: thread_summaries` (frontier semantics). A summary never covers the turn that triggered it, but after a delete the previous turn becomes the latest and may already be covered.

**Steps**:
1. [x] - `p1` - Run after the target turn and its messages were soft-deleted, inside the mutation transaction - `inst-si-after-soft-delete`
2. [x] - `p1` - DB: SELECT the chat's `thread_summaries` row (FOR UPDATE on PostgreSQL, so a concurrent summary worker commit and the mutation are ordered by the row lock) - `inst-si-select`
3. [x] - `p1` - **IF** no row exists - `inst-si-no-summary`
   1. [x] - `p1` - **RETURN** `kept` - `inst-si-return-none`
4. [x] - `p1` - **IF** the frontier `(summarized_up_to_created_at, summarized_up_to_message_id)` is at or after the target user message's `(created_at, id)` in the order `(created_at, id)`, compared through the fixed-width sort key on SQLite and the native type on PostgreSQL - `inst-si-covers`
   1. [x] - `p1` - DB: DELETE the chat's `thread_summaries` row - `inst-si-delete`
   2. [x] - `p1` - DB: UPDATE `is_compressed` to false on all `messages` rows of the chat - `inst-si-clear-compressed`
   3. [x] - `p1` - **RETURN** `invalidated`; until the next summary, context assembly reads the uncompressed history bounded by the token budget, and the next summary trigger builds a summary with no base frontier - `inst-si-return-invalidated`
5. [x] - `p1` - **ELSE** keep the summary; the next context is the existing summary followed by the replacement turn - `inst-si-keep`
6. [x] - `p1` - Never generate, enqueue or rewrite a summary here; the trigger belongs to `cpt-cf-mini-chat-algo-message-streaming-summary-trigger`, and the worker's check that its target frontier message was not soft-deleted belongs to `cpt-cf-mini-chat-feature-background-maintenance` - `inst-si-no-generation`
7. [x] - `p1` - **RETURN** `kept` - `inst-si-return-kept`

### Mutation Audit Event

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event`

**Input**: the operation, the caller, the chat id, the target `request_id`, the new `request_id` (retry and edit) and the commit time

**Output**: one audit payload for the audit queue, delivered by `cpt-cf-mini-chat-algo-gear-foundation-audit-delivery`. The fields are DESIGN 3.9 `Audit Events for Turn Mutations` and ADR-0009.

**Steps**:
1. [x] - `p1` - **MATCH** the operation - `inst-ae-match`
   - [x] - `p1` - **CASE** `retry`: `event_type` `turn_retry` with the `TurnMutationAuditEvent` fields `actor_user_id` (the caller), `chat_id`, `original_request_id` (the target), `new_request_id` and `timestamp` - `inst-ae-retry`
   - [x] - `p1` - **CASE** `edit`: `event_type` `turn_edit` with the same fields as retry - `inst-ae-edit`
   - [x] - `p1` - **CASE** `delete`: `event_type` `turn_delete` with the `TurnDeleteAuditEvent` fields `actor_user_id`, `chat_id`, `request_id` (the target) and `timestamp` - `inst-ae-delete`
2. [x] - `p1` - Enqueue the payload to `outbox.audit_queue_name` (default `mini-chat.audit`), partitioned by the chat's `tenant_id`, in the mutation transaction - `inst-ae-enqueue`
3. [x] - `p1` - Carry no content: no old or new message text, no attachment, no model and no provider identifier - `inst-ae-no-content`
4. [x] - `p1` - Enqueue exactly one mutation event per committed mutation and none for a rejected or rolled-back mutation - `inst-ae-exactly-one`
5. [x] - `p1` - Leave the replacement turn's own `turn_completed` or `turn_failed` event to `cpt-cf-mini-chat-algo-message-streaming-turn-audit-event` at its finalization; an unstarted replacement turn emits no turn audit event - `inst-ae-replacement-turn`
6. [x] - `p1` - **RETURN** the payload - `inst-ae-return`

### Post-Commit Setup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup`

**Input**: the committed mutation, the prepared mutation, the scoped chat and the caller

**Output**: the reserved replacement turn with its context plan, tool set, resolved provider and pre-allocated assistant message id, in the shape the send pipeline streams; or a JSON Problem after the replacement turn was marked `failed`. This is DESIGN 3.6 (`Retry / edit variant`, steps 4 and 5) and 3.9 rule 8.

**Steps**:
1. [x] - `p1` - Run without another PDP call: every query is tenant-scoped on the chat authorized by the mutation's admission - `inst-pcs-no-pdp`
2. [x] - `p1` - Run `cpt-cf-mini-chat-algo-message-streaming-tool-assembly` for the effective model of the decision with the carried web search flag - `inst-pcs-tools`
3. [x] - `p1` - Run `cpt-cf-mini-chat-algo-message-streaming-context-assembly` with the mutation's snapshot boundary, the decision, the tool set, the replacement content and the carried images as the current user item - `inst-pcs-context`
4. [x] - `p1` - **IF** context assembly returned a budget rejection - `inst-pcs-context-rejected`
   1. [x] - `p1` - Run the unstarted replacement turn failure algorithm with `context_length_exceeded` and **RETURN** 400 `out_of_range` with `field_violations[context].reason` `CONTEXT_BUDGET_EXCEEDED` - `inst-pcs-return-context`
5. [x] - `p1` - Resolve the provider of the effective model through the provider registry resolver of `cpt-cf-mini-chat-feature-gear-foundation` - `inst-pcs-provider`
6. [x] - `p1` - **IF** the provider cannot be resolved - `inst-pcs-provider-failed`
   1. [x] - `p1` - Run the unstarted replacement turn failure algorithm with `turn_setup_failed` and **RETURN** 500 `internal` with a generic detail - `inst-pcs-return-provider`
7. [x] - `p1` - Pre-allocate the assistant message id as a new UUID v4; no assistant row exists until finalization - `inst-pcs-message-id`
8. [x] - `p1` - Open the reserve transaction and call `QuotaService::write_reserve` with the decision, the turn identity (`tenant_id`, the caller as `user_id`, `chat_id`, the new `request_id`) and `ReserveTurn::Existing` with the replacement turn id; it fills the NULL preflight columns once, writes `started_at` as the decision's preflight instant (the value already written at the commit), increments the buckets and re-checks the limits - `inst-pcs-reserve`
9. [x] - `p1` - **IF** the re-check reported a bucket over its limit - `inst-pcs-over-limit`
   1. [x] - `p1` - Roll back the reserve transaction, run the unstarted replacement turn failure algorithm with `quota_exceeded`, and **RETURN** 429 `resource_exhausted` with scope `tokens`; no downgrade is attempted - `inst-pcs-return-429`
10. [x] - `p1` - **IF** the reserve transaction failed otherwise (the turn is no longer `running` or already holds a reserve, or a database error) - `inst-pcs-reserve-failed`
    1. [x] - `p1` - Roll back the reserve transaction, run the unstarted replacement turn failure algorithm with `turn_setup_failed`, and **RETURN** 500 `internal` - `inst-pcs-return-reserve-500`
11. [x] - `p1` - Commit the reserve transaction, then record the reserve metrics of the receipt - `inst-pcs-commit`
12. [x] - `p1` - Allow the provider request only after this commit, so the replacement turn holds its reserve before the first outbound byte; a crash after this commit is settled by the orphan watchdog on the estimated path - `inst-pcs-before-provider`
13. [x] - `p1` - Hand the reserved turn to the send pipeline unchanged (SSE relay, provider call, event translation, tool accounting, `cpt-cf-mini-chat-algo-message-streaming-finalization` with `Finalizer::Stream`, terminal emission, thread-summary trigger); never add a separate settlement or outbox path for retry or edit - `inst-pcs-handoff`
14. [x] - `p1` - **RETURN** the reserved replacement turn - `inst-pcs-return`

### Unstarted Replacement Turn Failure

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure`

**Input**: the replacement turn id and the error code (`context_length_exceeded`, `turn_setup_failed` or `quota_exceeded`)

**Output**: the replacement turn in state `failed`, or unchanged when another finalizer won. The rules are DESIGN 5.7 `FinalizeTurn Invariant` (unstarted retry/edit turn), 5.8 `Internal Error Code Taxonomy (Normative)` and ADR-0010.

**Steps**:
1. [x] - `p1` - DB: UPDATE the `chat_turns` row setting `state` `failed`, `error_code` to the given code, a sanitized non-sensitive `error_detail`, `completed_at` and `updated_at` to the current time, guarded by `state = 'running'` (a plain CAS) - `inst-uf-cas`
2. [x] - `p1` - **IF** the update affected no row - `inst-uf-cas-lost`
   1. [x] - `p1` - Log the outcome and change nothing; another finalizer already made the turn terminal - `inst-uf-return-cas-lost`
3. [x] - `p1` - Run no settlement and enqueue no usage event and no audit event, because no reserve exists; the billing outcome derivation is not used - `inst-uf-no-settlement`
4. [x] - `p1` - Leave the replaced turn replaced and the replacement user message in place, so the messages list shows the new user message with the new `request_id` and the client can retry, edit or delete the failed replacement turn - `inst-uf-keep-replacement`
5. [x] - `p1` - **IF** the update itself fails (database unavailable) - `inst-uf-update-failed`
   1. [x] - `p1` - Log the cause; the turn stays `running` with NULL reserve columns until `cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog` finalizes it as `orphan_timeout` without settlement (placeholder usage event and `turn_failed` audit event) - `inst-uf-orphan-fallback`
6. [x] - `p1` - Never send these codes over SSE: the client receives the JSON Problem of the caller, and the turn status reports `error` with the stored `error_code` - `inst-uf-not-sse`
7. [x] - `p1` - **RETURN** - `inst-uf-return`

### Message Order and Count After a Mutation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order`

**Input**: every message write and soft-delete of this feature

**Output**: a chronological order that stays stable across mutations and a `message_count` consistent with the list. Rules: DESIGN 3.7 `Table: messages` (`Ordering note (normative)`), implementation notes 1 and 11.

**Steps**:
1. [x] - `p1` - Set the replacement user message's `created_at` to the later of the application UTC clock and the latest `created_at` among all messages of the chat (the just soft-deleted ones included) plus one microsecond, comparing through the fixed-width sort key on SQLite - `inst-mo-user-after-all`
2. [x] - `p1` - Leave the assistant message timestamp rule (strictly after its user message) to `cpt-cf-mini-chat-algo-message-streaming-message-order` - `inst-mo-assistant`
3. [x] - `p1` - Exclude soft-deleted messages from the messages list, `message_count`, context assembly, `prior_context_tokens` and summary ranges, and keep them in storage - `inst-mo-exclude-deleted`
4. [x] - `p1` - Keep `message_count` consistent: delete removes the turn's non-deleted messages (two for a completed turn or a cancelled turn with a partial message, one for a failed turn or a cancelled turn without text); retry and edit remove the same and add the replacement user message, and the replacement turn adds its assistant message at finalization under the rules of `cpt-cf-mini-chat-algo-message-streaming-message-order`; the count itself is `cpt-cf-mini-chat-algo-chat-management-message-count` - `inst-mo-count`
5. [x] - `p1` - Keep the replaced or deleted turn's `request_id` out of every live path: replay of it returns 409 `request_id_conflict` through `cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks` and its turn status returns 404 through `cpt-cf-mini-chat-flow-message-streaming-turn-status` - `inst-mo-old-request-id`
6. [x] - `p1` - **RETURN** the ordering contract - `inst-mo-return`

### Reaction Request Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation`

**Input**: the raw request of `PUT` or `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`

**Output**: the validated reaction value (`PUT`) or nothing (`DELETE`), or a Problem. The body schema is `SetReactionReq`; the reasons are DESIGN 3.3 `Error Codes`.

**Steps**:
1. [x] - `p1` - **IF** the chat id or `msg_id` in the path is not a UUID - `inst-rv-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` `invalid_path_params` - `inst-rv-return-path`
2. [x] - `p1` - **IF** the method is `DELETE` - `inst-rv-delete`
   1. [x] - `p1` - **RETURN** success; the remove endpoint has no body - `inst-rv-return-delete`
3. [x] - `p1` - **IF** the body has no JSON `Content-Type` - `inst-rv-content-type`
   1. [x] - `p1` - **RETURN** 415 `invalid_argument` with `field_violations[body].reason` `missing_json_content_type` - `inst-rv-return-415`
4. [x] - `p1` - **IF** the body is not valid JSON - `inst-rv-json-syntax`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[body].reason` `json_syntax_error` - `inst-rv-return-syntax`
5. [x] - `p1` - **IF** the body does not match the schema (no `reaction`, or `reaction` not a string) - `inst-rv-schema`
   1. [x] - `p1` - **RETURN** 422 `invalid_argument` with `field_violations[body].reason` `invalid_json_body` - `inst-rv-return-422`
6. [x] - `p1` - **IF** `reaction` is not exactly `like` or `dislike` - `inst-rv-value`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[reaction].reason` `INVALID_REACTION`, before authorization - `inst-rv-return-value`
7. [x] - `p1` - **RETURN** the reaction value - `inst-rv-return`

### Reaction Target Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target`

**Input**: the scoped chat from admission (action `set_reaction` or `delete_reaction`) and `msg_id`

**Output**: the target assistant message, or a Problem. The rules are DESIGN 3.3 `Message Reaction API` and 3.7 `Table: message_reactions` (the parent chat is loaded through a scoped query first).

**Steps**:
1. [x] - `p1` - DB: SELECT the `messages` row with `id` equal to `msg_id`, the scoped `chat_id`, the tenant predicate and `deleted_at IS NULL` - `inst-rt-select`
2. [x] - `p1` - **IF** no row exists (unknown id, a message of another chat, or a soft-deleted message of a replaced or deleted turn) - `inst-rt-missing`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.message.v1~` - `inst-rt-return-404`
3. [x] - `p1` - **IF** the message's `role` is not `assistant` (a `user` or `system` message) - `inst-rt-not-assistant`
   1. [x] - `p1` - **RETURN** 400 `failed_precondition` with `violations[{subject: reaction_target, type: STATE}]`, for `PUT` and `DELETE` alike - `inst-rt-return-400`
4. [x] - `p1` - Accept every non-deleted assistant message, including a partial message of a cancelled turn and an empty completed answer - `inst-rt-any-assistant`
5. [x] - `p1` - **RETURN** the message - `inst-rt-return`

### Reaction Upsert

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert`

**Input**: the target assistant message, the caller's tenant and user ids from the security context, and the reaction value

**Output**: the stored reaction row. The rules are DESIGN 3.3 `Set Reaction` (upsert semantics) and 3.7 `Table: message_reactions`.

**Steps**:
1. [x] - `p1` - DB: INSERT a `message_reactions` row with a new UUID v4 `id`, `message_id`, `user_id` equal to the caller, `tenant_id` equal to the chat's tenant from the security context (never from input), `reaction` and `created_at` equal to the current time; on a conflict on `(message_id, user_id)` update `reaction` and `created_at` of the existing row in the same statement - `inst-ru-upsert`
2. [x] - `p1` - Keep at most one row per user and message through the unique `(message_id, user_id)` constraint; two concurrent `PUT` requests both succeed and the last committed value stays, with no 409 and no 500 for the unique conflict - `inst-ru-one-row`
3. [x] - `p1` - Make a repeated `PUT` with the same value leave the same single row with the same `reaction` (idempotent state) - `inst-ru-idempotent`
4. [x] - `p1` - Write no audit event and no outbox message, do not bump `chats.updated_at` and do not change the message - `inst-ru-no-side-effects`
5. [x] - `p1` - **IF** a database error occurred - `inst-ru-db-error`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail - `inst-ru-return-500`
6. [x] - `p1` - **RETURN** `message_id`, `reaction` and `created_at` of the stored row - `inst-ru-return`

### Reaction Removal

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal`

**Input**: the target assistant message and the caller's tenant and user ids from the security context

**Output**: success whether or not a reaction existed. The rule is DESIGN 3.3 `Remove Reaction`.

**Steps**:
1. [x] - `p1` - DB: DELETE the `message_reactions` row with the target `message_id`, `user_id` equal to the caller and the caller's tenant - `inst-rr-delete`
2. [x] - `p1` - Treat zero and one deleted rows alike, so a repeated `DELETE` keeps returning 204 - `inst-rr-idempotent`
3. [x] - `p1` - Never remove another user's reaction: the owner column `user_id` of `message_reactions` scopes the delete - `inst-rr-owner-only`
4. [x] - `p1` - Write no audit event and no outbox message and do not bump `chats.updated_at` - `inst-rr-no-side-effects`
5. [x] - `p1` - **IF** a database error occurred - `inst-rr-db-error`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail - `inst-rr-return-500`
6. [x] - `p1` - **RETURN** success - `inst-rr-return`

### Turn Mutation and Reaction Error Contract

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract`

**Input**: any rejection raised by the five endpoints of this feature before a stream opens

**Output**: the canonical Problem, built by `cpt-cf-mini-chat-algo-gear-foundation-error-mapping`. The rows are DESIGN 3.3 `Error Codes` and ADR-0004.

**Steps**:
1. [x] - `p1` - **MATCH** the rejection - `inst-ec-match`
   - [x] - `p1` - **CASE** chat, turn or message not visible: 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.chat.v1~`, `gts.cf.core.mini_chat.turn.v1~` or `gts.cf.core.mini_chat.message.v1~` - `inst-ec-404`
   - [x] - `p1` - **CASE** body or path errors: 415 `missing_json_content_type`, 400 `json_syntax_error`, 422 `invalid_json_body`, 400 `invalid_path_params` - `inst-ec-body`
   - [x] - `p1` - **CASE** value errors: 400 `invalid_argument` `EMPTY_CONTENT` (field `content`, edit) and `INVALID_REACTION` (field `reaction`) - `inst-ec-values`
   - [x] - `p1` - **CASE** state errors: 400 `failed_precondition` with `violations[{subject: turn_state, type: STATE}]` or `violations[{subject: reaction_target, type: STATE}]` - `inst-ec-state`
   - [x] - `p1` - **CASE** mutation conflicts: 409 `aborted` with `context.reason` `NOT_LATEST_TURN` or `GENERATION_IN_PROGRESS` and a generic `detail` - `inst-ec-409`
   - [x] - `p1` - **CASE** requester mismatch: 403 `permission_denied` with `context.reason` `AUTHZ_DENIED` - `inst-ec-requester`
   - [x] - `p1` - **CASE** retry and edit preflight: 400 `INVALID_MODEL` (`model`), 400 `out_of_range` `TOO_MANY_IMAGES` (`image_count`) and `INPUT_TOO_LONG` (`content`), 400 `failed_precondition` `FEATURE_DISABLED` for `web_search` or `images`, 400 `VISION_NOT_SUPPORTED` (`content_type`), 429 `resource_exhausted` with `violations[{subject: tokens|web_search|code_interpreter, description: "quota_exceeded"}]` - `inst-ec-preflight`
   - [x] - `p1` - **CASE** retry and edit setup after the commit: 400 `out_of_range` `CONTEXT_BUDGET_EXCEEDED` (`context`), 429 scope `tokens`, 500 `internal` - `inst-ec-setup`
   - [x] - `p1` - **CASE** authorization: 401, 403 `AUTHZ_DENIED` or `LICENSE_FEATURE_REQUIRED`, 503 with `Retry-After: 5` - `inst-ec-authz`
   - [x] - `p1` - **CASE** audit payload above the outbox size limit, provider or policy resolution failure before streaming, internal or database error: 500 `internal` with a generic detail - `inst-ec-500`
2. [x] - `p1` - Return every one of these as a JSON Problem with no SSE stream opened; after the stream opens, the streaming error codes of `cpt-cf-mini-chat-feature-message-streaming` apply unchanged - `inst-ec-json-only`
3. [x] - `p1` - Keep `detail` human-readable and outside the contract; clients branch on the status, the category and the machine-readable fields only - `inst-ec-detail`
4. [x] - `p1` - Declare in the OpenAPI document: retry 200 (`text/event-stream`), 400, 401, 403, 404, 409, 429, 500, 503; edit the same plus 422; delete 204, 400, 401, 403, 404, 409, 500, 503; set reaction 200, 400, 401, 403, 404, 422, 500, 503; remove reaction 204, 400, 401, 403, 404, 500, 503; 415 is not declared - `inst-ec-openapi`
5. [x] - `p1` - **RETURN** the Problem - `inst-ec-return`

### Turn Mutation Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics`

**Input**: the outcome and the duration of each retry, edit and delete request

**Output**: the series of DESIGN 4 `Metrics (OTLP) and Alerts (P1)` (`Turn mutations`, emitted) and PRD `Emitted: turn mutations`.

**Steps**:
1. [x] - `p1` - Record `mini_chat_turn_mutation` once per request with `op` (`retry`, `edit` or `delete`) and `result`: `ok` when the mutation committed (a later setup failure of the replacement turn is reported through its turn state, not here), otherwise the rejection class `not_found`, `forbidden`, `not_latest_turn`, `turn_running`, `generation_in_progress`, `preflight_rejected` or `error` - `inst-met-counter`
2. [x] - `p1` - Record `mini_chat_turn_mutation_latency_ms` with `op` for each committed mutation, from the start of the preview to the commit - `inst-met-latency`
3. [x] - `p1` - Use no tenant, user, chat or request identifier as a label and never log message content; log causes with the chat id and the request ids only - `inst-met-cardinality`
4. [x] - `p1` - Record no reaction metric; the DESIGN defines none - `inst-met-no-reaction-metric`
5. [x] - `p1` - **RETURN** - `inst-met-return`

## 4. States (CDSL)

### Turn Visibility State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility`

**States**: Active, Replaced, Deleted

**Initial State**: Active (a `chat_turns` row with `deleted_at` NULL; its execution state follows `cpt-cf-mini-chat-state-message-streaming-turn-lifecycle`)

**Transitions**:
1. [x] - `p1` - **FROM** Active **TO** Replaced **WHEN** a retry or edit mutation transaction commits for the latest terminal turn, setting `deleted_at` and `replaced_by_request_id` and soft-deleting its messages - `inst-tv-to-replaced`
2. [x] - `p1` - **FROM** Active **TO** Deleted **WHEN** a delete mutation transaction commits for the latest terminal turn, setting `deleted_at` only and soft-deleting its messages - `inst-tv-to-deleted`
3. [x] - `p1` - **FROM** Replaced **TO** Replaced **WHEN** any later retry, edit or delete targets the turn: 409 `NOT_LATEST_TURN` and nothing changes (the same holds for Deleted); a soft-deleted turn is excluded from history, context, replay and turn status, and is never restored or purged in P1 - `inst-tv-final`

### Replacement Turn Setup State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup`

**States**: Unreserved, Reserved, Failed, Finalized

**Initial State**: Unreserved (the replacement turn committed by the mutation transaction: `running`, `started_at` equal to the preflight instant, the six preflight columns NULL)

**Transitions**:
1. [x] - `p1` - **FROM** Unreserved **TO** Reserved **WHEN** the reserve transaction through `ReserveTurn::Existing` commits, filling the preflight columns once and booking the reserve - `inst-rts-to-reserved`
2. [x] - `p1` - **FROM** Unreserved **TO** Failed **WHEN** context assembly, provider resolution, the reserve re-check or another setup step fails, and the plain CAS sets `failed` with `context_length_exceeded`, `turn_setup_failed` or `quota_exceeded`, with no settlement and no outbox event - `inst-rts-to-failed-setup`
3. [x] - `p1` - **FROM** Unreserved **TO** Failed **WHEN** the pod stopped before the reserve and the orphan watchdog wins its CAS with `orphan_timeout`, skipping settlement for the NULL reserve columns - `inst-rts-to-failed-orphan`
4. [x] - `p1` - **FROM** Reserved **TO** Finalized **WHEN** the send pipeline's finalization, cancellation or orphan watchdog commits the first terminal transition (`completed`, `failed` or `cancelled`) with its settlement and outbox events - `inst-rts-to-finalized`
5. [x] - `p1` - **FROM** Failed **TO** Failed **WHEN** any later terminal signal arrives: the CAS affects no row and the signal is discarded (the same holds for Finalized) - `inst-rts-immutable`

### Message Reaction State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction`

**States**: None, Like, Dislike

**Initial State**: None (no `message_reactions` row for the user and the assistant message)

**Transitions**:
1. [x] - `p1` - **FROM** None **TO** Like **WHEN** the user sends `PUT` with `like` - `inst-rx-none-like`
2. [x] - `p1` - **FROM** None **TO** Dislike **WHEN** the user sends `PUT` with `dislike` - `inst-rx-none-dislike`
3. [x] - `p1` - **FROM** Like **TO** Dislike **WHEN** the user sends `PUT` with `dislike` (the row is replaced) - `inst-rx-like-dislike`
4. [x] - `p1` - **FROM** Dislike **TO** Like **WHEN** the user sends `PUT` with `like` (the row is replaced) - `inst-rx-dislike-like`
5. [x] - `p1` - **FROM** Like **TO** Like **WHEN** the user sends `PUT` with `like` again (idempotent; the same holds for Dislike) - `inst-rx-same`
6. [x] - `p1` - **FROM** Like **TO** None **WHEN** the user sends `DELETE` (the same holds for Dislike) - `inst-rx-remove`
7. [x] - `p1` - **FROM** None **TO** None **WHEN** the user sends `DELETE` and no reaction exists (204) - `inst-rx-remove-none`

## 5. Definitions of Done

### Latest Terminal Turn Rule

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-latest-terminal-turn`

The system **MUST** accept retry, edit and delete only for the non-deleted turn with the greatest `(started_at, id)` of the chat in state `completed`, `failed` or `cancelled`, **MUST** return 404 with the turn type for an unknown `request_id`, 403 `AUTHZ_DENIED` when the requester is not the caller, 409 `NOT_LATEST_TURN` for a soft-deleted or non-latest target (including a target behind a newer `running` turn) and 400 `turn_state` for a `running` target (checked before the latest-turn check), and **MUST** compare timestamps through the fixed-width sort key on SQLite.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck`
- `cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility`

**Constraints**: `cpt-cf-mini-chat-principle-linear-conversation` (principle; no design constraint is owned)

**Touches**:
- API: `POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH /v1/chats/{id}/turns/{request_id}`, `DELETE /v1/chats/{id}/turns/{request_id}`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Retry Last Turn Endpoint

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-retry`

The system **MUST** serve `POST /v1/chats/{id}/turns/{request_id}/retry` with no body under the action `retry_turn` evaluated once, **MUST** run the preview and the mutation preflight before anything is written, **MUST** re-submit the original user message content and its non-deleted attachments (images re-sent under the send-time image guards) in a replacement turn with a new server-generated UUID v4 `request_id`, and **MUST** stream the new answer through the send pipeline with `stream_started` carrying the new `request_id` and `is_new_turn` `true`, with no separate settlement or outbox path.

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-constraint-model-locked-per-chat`, `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- API: `POST /v1/chats/{id}/turns/{request_id}/retry`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`, `Message`

### Edit Last Turn Endpoint

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-edit`

The system **MUST** serve `PATCH /v1/chats/{id}/turns/{request_id}` with a body of only `content` under the action `edit_turn` evaluated once, **MUST** validate the body before admission (415, 400 `json_syntax_error`, 422, 400 `EMPTY_CONTENT`), **MUST** keep the content as sent, **MUST** re-link the original non-deleted attachments to the replacement message, and **MUST** reuse the exact streaming pipeline, SSE contract, ordering rules, terminal semantics, error mapping, downgrade metadata and billing finalization of `POST /messages:stream`.

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-constraint-model-locked-per-chat`, `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- API: `PATCH /v1/chats/{id}/turns/{request_id}`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-messages`
- Entities: `Turn`, `Message`

### Delete Last Turn Endpoint

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-delete`

The system **MUST** serve `DELETE /v1/chats/{id}/turns/{request_id}` under the action `delete_turn`, **MUST** soft-delete the latest terminal turn (`deleted_at` only, no `replaced_by_request_id`) and its messages in one transaction with the summary invalidation and the `turn_delete` audit enqueue, **MUST** return 204 with no body, **MUST NOT** run a quota preflight, take a reserve or call a provider, and **MUST NOT** bump `chats.updated_at`.

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit`

**Constraints**: `cpt-cf-mini-chat-principle-linear-conversation` (principle; no design constraint is owned)

**Touches**:
- API: `DELETE /v1/chats/{id}/turns/{request_id}`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-thread-summaries`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Atomic Mutation Transaction

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-mutation-transaction`

The system **MUST** execute the latest-turn re-check, the terminal re-check, the guarded soft-delete of the target turn and its messages, the replacement user message and copied links, the replacement `running` turn (with `started_at` equal to the mutation preflight instant and NULL preflight columns), the summary invalidation, the activity bump (retry and edit only) and the audit enqueue in one database transaction under the target row lock (PostgreSQL) or the write lock (SQLite), **MUST** roll everything back on any rejection, and **MUST** fire the outbox wake only after the commit.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: retry, edit and delete endpoints
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-chats`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`, `Message`

### Deterministic Concurrent Mutations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-concurrency`

The system **MUST** let exactly one of several concurrent mutations of the same chat commit (first commit wins), **MUST** answer every loser with 409 `aborted` and `context.reason` `NOT_LATEST_TURN` (the target is soft-deleted or no longer the latest) or `GENERATION_IN_PROGRESS` (the replacement turn insert lost on the running-turn unique index), never 500, and **MUST** leave no row, reserve or audit event of a loser.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: retry, edit and delete endpoints
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Attachment and Tool-Usage Carry-Forward

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-carry-forward`

The system **MUST** copy the original user message's attachment links to the replacement message as new `message_attachments` rows (keeping the original rows on the soft-deleted message), **MUST** silently exclude attachments with `deleted_at` set, **MUST** re-send the carried images under the image count, `disable_images` and vision guards of the effective model, **MUST** reuse the replaced turn's `web_search_enabled`, **MUST** start the replacement turn's tool counters at 0 while keeping the replaced turn's counters, settlement and usage event untouched, and **MUST** decide the tools of the replacement turn afresh from the chat's current ready facts.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`

**Constraints**: `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- API: retry and edit endpoints
- DB: `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-turns`
- DB Table: `cpt-cf-mini-chat-dbtable-message-attachments`
- Entities: `Message`, `Attachment`

### Thread Summary Invalidation on Mutation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-summary-invalidation`

The system **MUST**, inside the retry, edit and delete transaction and after the soft-deletes, delete the chat's `thread_summaries` row and clear `is_compressed` on all messages of the chat when the summary frontier is at or after the target user message's `(created_at, id)`, **MUST** keep the summary otherwise, and **MUST NOT** generate or enqueue a summary in the mutation.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation`

**Constraints**: `cpt-cf-mini-chat-constraint-context-budget`

**Touches**:
- API: retry, edit and delete endpoints
- DB: `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-thread-summaries`
- Entities: `ThreadSummary`

### Mutation Audit Events

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-audit-events`

The system **MUST** enqueue exactly one `turn_retry` or `turn_edit` (`actor_user_id`, `chat_id`, `original_request_id`, `new_request_id`, `timestamp`) or `turn_delete` (`actor_user_id`, `chat_id`, `request_id`, `timestamp`) event to the `mini-chat.audit` queue in each committed mutation transaction, with no content, **MUST** enqueue none for a rejected mutation, and **MUST** map an oversized payload to 500 `internal` with a rollback.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound` (no audit event is ever a provider call; delivery runs through the foundation's audit handler)

**Touches**:
- API: retry, edit and delete endpoints
- DB: shared outbox tables (queue `mini-chat.audit`)
- Entities: `TurnMutationAuditEvent`, `TurnDeleteAuditEvent`

### Post-Commit Setup and Unstarted Replacement Turn

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-post-commit-setup`

The system **MUST**, after the mutation commit, run tool assembly, context assembly and provider resolution and then the reserve write with `ReserveTurn::Existing` (filling the preflight columns and `started_at` once, passing the caller as `identity.user_id`), **MUST** mark the replacement turn `failed` with a plain CAS and `context_length_exceeded`, `turn_setup_failed` or `quota_exceeded` on a setup failure, returning 400 `CONTEXT_BUDGET_EXCEEDED`, 500 or 429 scope `tokens`, with no settlement, no usage event and no audit event, and **MUST** allow the provider call only after the reserve commit.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure`
- `cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-context-budget`

**Touches**:
- API: retry and edit endpoints
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-quota-usage`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn`

### Message Reactions API

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-reactions`

The system **MUST** serve `PUT /v1/chats/{id}/messages/{msg_id}/reaction` (actions `set_reaction`; 200 with `message_id`, `reaction`, `created_at`) and `DELETE` on the same path (action `delete_reaction`; 204 whether or not a reaction existed), **MUST** validate the reaction value before authorization (400 `INVALID_REACTION`, 422 for a schema mismatch), **MUST** accept only non-deleted assistant messages of the scoped chat (404 with the message type otherwise, 400 `reaction_target` / `STATE` for user and system messages on both methods), **MUST** keep at most one reaction per user and message through an upsert, and **MUST** populate `tenant_id` and `user_id` from the security context.

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction`
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal`
- `cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction`

**Constraints**: none; no design constraint binds reactions, and owner scoping comes from `cpt-cf-mini-chat-design-authz-pep`

**Touches**:
- API: `PUT /v1/chats/{id}/messages/{msg_id}/reaction`, `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`
- DB: `cpt-cf-mini-chat-dbtable-message-reactions`, `cpt-cf-mini-chat-dbtable-messages`
- DB Table: `cpt-cf-mini-chat-dbtable-message-reactions`
- Entities: `MessageReaction`

### Error Contract and Observability

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-errors-observability`

The system **MUST** map every rejection of the five endpoints to the canonical Problem with the exact categories and reasons of DESIGN 3.3 (`NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`, `turn_state`, `reaction_target`, `INVALID_REACTION`, `EMPTY_CONTENT`, the preflight and setup rows), **MUST** declare the responses listed by the error contract algorithm in the OpenAPI document, and **MUST** record `mini_chat_turn_mutation{op,result}` and `mini_chat_turn_mutation_latency_ms{op}` with no identifiers in labels and no content in logs.

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics`
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: every route of this feature
- DB: none
- Entities: none

### Deferred and Not-Implemented Scope Stays Unimplemented

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-deferred-scope`

The system **MUST NOT** implement mutation of any turn other than the latest, thread branching or multi-version conversations, restoring or hard-purging soft-deleted turns, messages or reactions, a client-chosen `request_id` or attachment change on retry or edit, a quota refund for a replaced or deleted turn, audit content or redaction, a chat-deletion audit event, reactions on user or system messages, or immediate exclusion of a deleted document from `file_search`, and **MUST** keep them recorded as deferred or not implemented (PRD `Message Actions (P1 Scope)`, ADR-0007, ADR-0009).

**Implements**:
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`
- `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`

**Constraints**: `cpt-cf-mini-chat-principle-linear-conversation` (principle; no design constraint is owned)

**Touches**:
- API: none
- DB: none
- Entities: none

### Test Layering, Traceability and Feature-Local Deviations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-tests-and-deviations`

The system **MUST** cover every acceptance-criteria item that this feature owns with tests in the layers below, and **MUST** record each feature-local deviation with its rationale and review owner.

**Test layering**:
- Unit tests live in the gear crate (`gears/mini-chat/mini-chat/`) and cover pure logic: edit request validation (every Problem, content kept as sent, unknown fields ignored), the preview check order (unknown, foreign requester, soft-deleted, running, non-latest behind a newer completed and a newer running turn), the latest-turn comparison with equal `started_at` and the `id` tie-break, the attachment carry-forward filter and order (deleted attachment excluded, image subset), the tool-usage carry-forward (flag reused, counters 0), the summary frontier comparison (frontier before, equal to and after the target user message), the mutation audit payloads (`turn_retry`, `turn_edit`, `turn_delete` field sets, no content), reaction validation (`like`, `dislike`, any other value, missing field) and the reaction state machine, the error contract rows and the metric `result` classes.
- Repository tests with SQLite cover the latest-turn query with fractional seconds of different lengths (`.12` and `.123`), the mutation transaction (soft-delete of the turn and both messages, `replaced_by_request_id` on retry and edit only, the replacement message with `created_at` after every existing message, copied links with `chat_id` from the message row, the replacement turn with NULL preflight columns and `started_at` equal to the preflight instant, the activity bump on retry and edit and none on delete, one audit outbox row), the rollback on each rejection (forced audit payload overflow, forced database error, running-index violation) with no row left, the summary invalidation (row deleted and `is_compressed` cleared when the frontier covers the target, kept otherwise, after a delete that exposes a covered turn), the reserve write through `ReserveTurn::Existing` on the replacement turn, the unstarted-turn CAS, and the reaction upsert and removal (one row per user and message, concurrent upserts, removal of the caller's row only).
- Concurrency tests run two mutation transactions against the same chat with a controlled interleaving: delete then retry, retry then edit, retry then delete, and a send that inserts a `running` turn between the preview and the commit; each yields one committed mutation and one 409 with the expected reason (`NOT_LATEST_TURN` after a committed soft-delete or a newer turn, `GENERATION_IN_PROGRESS` on the running-turn index) and no residue of the loser.
- In-process REST and SSE tests drive the router with stubbed platform clients (PDP, types-registry, authn resolver, model-policy and audit plugins) over SQLite and the mock OAGW `ServiceGatewayClientV1` with scripted provider streams, as in `cpt-cf-mini-chat-feature-message-streaming`; they cover the retry and edit streams end to end (new `request_id` in `stream_started`, `is_new_turn` `true`, `done`, the persisted replacement turn and messages, one usage event, one `turn_completed` and one `turn_retry` or `turn_edit` audit event), every pre-commit Problem with the previous turn intact and no proxied request, the post-commit setup failures (context budget, provider resolution, reserve re-check) with the turn `failed` and no usage or turn audit event, a disconnect after the reserve commit, the delete endpoint, the replay of a replaced `request_id` (409 `request_id_conflict`) and its turn status (404), the messages list and `message_count` after each operation, and the reaction endpoints including the `my_reaction` projection.
- No tests are added under `testing/e2e/suites/mini_chat/`.
- Every implemented CDSL step carries `@cpt-begin` and `@cpt-end` markers with its ID, phase token and `inst-*` ID (traceability mode FULL).

**Acceptance-criteria test mapping**:

| Acceptance-criteria item | Flows, algorithms and DoDs | Test layer |
|--------------------------|----------------------------|------------|
| Retry / edit / delete act only on the latest, terminal turn | `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck`, `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-latest-terminal-turn` | Unit (check order), repository (latest-turn query on SQLite), in-process REST (404, 403, 409, 400 per endpoint) |
| A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id | `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn`, `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup`, `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-retry`, `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-edit`, `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-post-commit-setup` | In-process REST and SSE with mock OAGW (quota 429 before commit, context budget after commit, image guards, new `request_id`), repository (`Existing` reserve) |
| Concurrent mutations resolve deterministically | `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit`, `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-concurrency` | Concurrency tests over SQLite with controlled interleaving, in-process REST (two concurrent retries yield one stream and one 409) |
| Mutated turns correctly carry forward attachment and tool-usage history | `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`, `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-carry-forward` | Unit (filter, order, flag), repository (copied links, counters), in-process SSE (images in the proxied request, `web_search` tool present) |
| Set/remove reaction on assistant messages only, idempotently | `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction`, `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert`, `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal`, `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-reactions` | Unit (validation, state machine), repository (upsert, removal), in-process REST (200, 204, 400, 404, `my_reaction`) |

**Reliability and data integrity**: the mutation transaction and the reserve transaction are each atomic; the first committed mutation wins and losers leave nothing; the replacement turn never stays `running` after a setup failure except when the database itself is unavailable, in which case the orphan watchdog finalizes it; a replaced turn's settlement, usage event and audit event are never repeated. **Security**: admission, tenant and owner scoping and 404 masking come from the foundation; the target turn's requester must be the caller; reaction rows take `tenant_id` and `user_id` from the security context; no content and no provider identifier reaches an audit event, a metric label or a log. **Observability**: the turn mutation series, the mutation audit events and the replacement turn's own usage and audit events. **Rollback**: a failed transaction leaves no partial state; soft-deleted rows are never restored in P1; schema rollback is owned by the foundation migrations. **Compile-time gates**: those of the foundation apply (workspace lints, the domain layering lint that keeps `domain/` free of `crate::infra`, `sea_orm`, `axum` and `http`, the domain-model lint on new domain types, the example server build with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`).

**Feature-local deviations** (review owner for every item: the mini-chat gear owners who maintain `gears/mini-chat/docs/`):

| Deviation | Rationale | Source |
|-----------|-----------|--------|
| The preview checks run in the order: turn missing (404), requester mismatch (403), soft-deleted (409 `NOT_LATEST_TURN`), `running` (400 `turn_state`), non-latest (409 `NOT_LATEST_TURN`) | The DESIGN fixes only that the running check precedes the latest-turn check; a deterministic order makes each case testable | DESIGN 3.9 rules 1 to 3 and 9 |
| Edit body validation (415, 400, 422, `EMPTY_CONTENT`) runs before the PEP call and the chat load | Same order as the send path; validation stays independent of chat and turn existence | DESIGN 3.3, FEATURE 0004 |
| `INPUT_TOO_LONG` is evaluated in the mutation preflight, before the commit | It depends only on the preflight decision and the content, so a rejection keeps the previous answer | DESIGN 3.9 rule 8 |
| The replacement turn is inserted at the commit with `started_at` already equal to the mutation preflight instant, and the `Existing` reserve path writes the same value again | Keeps the latest-turn order and the orphan watchdog's period derivation correct from the commit on, and satisfies the reserve contract | Implementation notes 8 and 10, FEATURE 0003 `inst-rw-started-at` |
| The mutation preflight's `prior_context_tokens` may still count the assistant message of the turn being replaced | The preflight runs before the soft-delete; the overestimate is bounded and conservative, and the estimate rules belong to the quota service | DESIGN 3.9 rule 8, FEATURE 0003 `inst-est-prior-context` |
| Carried images are ordered by the original link's `created_at`, then `attachment_id` | `message_attachments` stores no position; a deterministic order keeps the provider request reproducible | DESIGN 3.7 `Table: message_attachments` |
| A replaced or deleted turn's `message_attachments` rows stay, so its attachments remain locked against `DELETE` (409 `attachment_locked`) | The DESIGN keeps the old rows for audit and locks any attachment referenced by a submitted message | DESIGN 3.9, 4 `Attachment Mutability and Deletion` |
| A mutation never refunds or re-settles the quota of the replaced or deleted turn | Settlement is exactly once at finalization; the DESIGN defines no refund path | DESIGN 5.7 |
| A reaction replace updates `created_at` to the time of the write | The DESIGN specifies upsert semantics without saying which timestamp a replaced reaction keeps; the response reflects the current reaction | DESIGN 3.3 `Set Reaction` |
| The `result` label of `mini_chat_turn_mutation` takes the values `ok`, `not_found`, `forbidden`, `not_latest_turn`, `turn_running`, `generation_in_progress`, `preflight_rejected` and `error` | The DESIGN names the label but not its values | DESIGN 4 `Metrics (OTLP) and Alerts (P1)` |
| On SQLite the mutation transaction takes the database write lock before its first read instead of `SELECT ... FOR UPDATE` | SQLite has no row locks; the write lock serializes mutations with the same outcome | DESIGN 3.9 rule 7 (`or equivalent serializable isolation`) |
| Where `config/mini-chat.yaml` or the extracted OpenAPI and the DESIGN differ, the DESIGN governs | `gears/mini-chat/docs/` is the source of truth | DECOMPOSITION 1 |

**Implements**:
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn`
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction`
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction`
- `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability`

**Constraints**: `cpt-cf-mini-chat-constraint-context-budget`, `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-model-locked-per-chat`, `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- API: every route of this feature
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-message-reactions`
- Entities: `Turn`, `Message`, `MessageReaction`

## 6. Acceptance Criteria

The first five items are the DECOMPOSITION 2.6 "Acceptance Criteria Covered" and the matching items of `acceptance-criteria.md`; each is both implemented and tested. The remaining items pin the details behind them. The test layer of each item is given in parentheses.

- [ ] Retry, edit and delete act only on the latest, terminal turn: for a chat with turns T1 and T2 (both completed), a mutation of T1 returns 409 `aborted` `NOT_LATEST_TURN`; a mutation of T2 while it is `running` returns 400 `failed_precondition` `turn_state`; a mutation of T1 while a newer T2 is `running` returns 409 `NOT_LATEST_TURN`; a mutation of an already replaced or deleted turn returns 409 `NOT_LATEST_TURN`; an unknown `request_id` returns 404 with the turn type; after deleting T2, T1 becomes the latest turn and can be retried; latest-turn selection is correct on SQLite with equal and differently precise `started_at` values (unit, repository, in-process REST).
- [ ] A mutation goes through the full send pipeline and gets a new request id: retry and edit stream `stream_started` with a server-generated UUID v4 `request_id` different from the target's and `is_new_turn` `true`, then the send events and `done`; quota exhaustion before the commit returns 429 with the previous turn and its answer intact and no proxied request; `disable_images`, a non-vision effective model and too many carried images return 400 before the commit; a context that no longer fits after the commit returns 400 `CONTEXT_BUDGET_EXCEEDED` and leaves the replacement turn `failed` with `context_length_exceeded`; the replacement turn's reserve fills its preflight columns through `ReserveTurn::Existing` and it is settled once with one usage event (in-process REST and SSE with mock OAGW, repository).
- [ ] Concurrent mutations resolve deterministically: two concurrent retries of the latest turn yield exactly one stream and one 409 `aborted` (`NOT_LATEST_TURN` or `GENERATION_IN_PROGRESS` by the interleaving); retry against delete, edit against retry and delete against delete each leave exactly one committed mutation, one audit event and no row of the loser; a send that commits a `running` turn between a mutation's preview and its commit makes the mutation return 409 `NOT_LATEST_TURN` or `GENERATION_IN_PROGRESS`, never 500 (concurrency tests over SQLite, in-process REST).
- [ ] Mutated turns carry forward attachment and tool-usage history: the replacement user message lists the original non-deleted attachments in `attachments` (new `message_attachments` rows, original rows kept on the soft-deleted message), an attachment soft-deleted since the original turn is silently absent, the original images appear as image inputs of the proxied request, the replacement turn has `web_search_enabled` equal to the original and includes the `web_search` tool when the effective model supports it, its counters start at 0, and the replaced turn keeps its counters, its settled `quota_usage` and its single usage event (unit, repository, in-process SSE).
- [ ] Set and remove reaction work on assistant messages only, idempotently: `PUT` with `like` returns 200 with `message_id`, `reaction` and `created_at`, a second `PUT` with `like` or `dislike` leaves one row with the latest value, `DELETE` returns 204 with and without an existing reaction, `PUT` and `DELETE` on a user or system message return 400 `failed_precondition` `reaction_target` / `STATE`, an invalid value returns 400 `INVALID_REACTION` before authorization, a missing field returns 422, a message of a replaced turn or another chat returns 404 with the message type, and the messages list shows the caller's value in `my_reaction` (unit, repository, in-process REST).
- [ ] The mutation transaction is atomic: after a retry or edit, the target turn has `deleted_at` and `replaced_by_request_id`, its user and assistant messages have `deleted_at`, a replacement user message and a `running` replacement turn exist with `started_at` equal to the preflight instant, and one audit row is in `mini-chat.audit`; a forced audit payload overflow returns 500 and leaves the previous turn, its messages, the summary and `chats.updated_at` unchanged (repository, in-process REST).
- [ ] Delete removes the latest turn from the active conversation: 204 with no body, the turn has `deleted_at` and no `replaced_by_request_id`, its messages disappear from the list and from `message_count`, its turn status returns 404, a resend of its `request_id` to `messages:stream` returns 409 `request_id_conflict`, `chats.updated_at` is unchanged, and no quota row changes (in-process REST).
- [ ] The summary is invalidated only when it covers the mutated turn: with a frontier at or after the target user message, the mutation deletes the `thread_summaries` row and clears `is_compressed` on all messages of the chat, and the next stream has no `thread_summary_applied`; with a frontier before it, the summary stays and the next context is the summary followed by the replacement turn; deleting the latest turn after a summary covered the previous turn and then mutating that previous turn invalidates the summary (repository, in-process SSE).
- [ ] Mutation audit events are exact: one `turn_retry` or `turn_edit` with `actor_user_id`, `chat_id`, `original_request_id`, `new_request_id` and `timestamp`, or one `turn_delete` with `actor_user_id`, `chat_id`, `request_id` and `timestamp`, per committed mutation and none for a rejected one; no event carries message text or attachments; the replacement turn adds its own `turn_completed` or `turn_failed` at finalization, and an unstarted replacement turn adds none (unit, repository).
- [ ] Post-commit setup failures leave a terminal replacement turn: a forced provider resolution failure returns 500 and the turn is `failed` with `turn_setup_failed`; a concurrent reserve that fills the quota returns 429 scope `tokens` and the turn is `failed` with `quota_exceeded`; in each case no usage event, no settlement and no turn audit event exist, the turn status reports `error` with the code, and the chat accepts a new turn at once (in-process REST, repository).
- [ ] Retry and edit are authorized once: the stubbed PDP records exactly one evaluation (`retry_turn` or `edit_turn`) per request; a foreign or deleted chat returns 404 with the chat type, a PDP deny 403 `AUTHZ_DENIED` and a PDP failure 503 with `Retry-After: 5`, each before any write (in-process REST).
- [ ] The edit body is validated and kept: a missing JSON content type returns 415, malformed JSON 400 `json_syntax_error`, a missing `content` 422, whitespace-only content 400 `EMPTY_CONTENT`; a valid content with leading spaces is stored and sent unchanged (unit, in-process REST).
- [ ] The turn mutation metrics are recorded: `mini_chat_turn_mutation` with `op` and `result` for every request and `mini_chat_turn_mutation_latency_ms` with `op` for every committed mutation, with no identifiers in labels (unit with a metrics recorder).
- [ ] The deferred scope stays unimplemented: no mutation of older turns, no branching, no restore or purge of soft-deleted rows, no client-chosen `request_id` or attachment change on retry or edit, no quota refund, no audit content, no reactions on user or system messages (review and in-process REST negative cases).
- [ ] Unit tests live in the gear crate, repository, concurrency and REST/SSE tests run in-process over SQLite with a mock OAGW `ServiceGatewayClientV1` and no network, no test is added under `testing/e2e/suites/mini_chat/`, and every implemented CDSL step has `@cpt-begin` and `@cpt-end` markers.
