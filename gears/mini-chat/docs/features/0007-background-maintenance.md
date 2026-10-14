---
description: "Specifies the Background Maintenance feature of the mini-chat gear: the outbox-driven thread summary worker (frozen range, prompt and summary model call, CAS commit with the concurrent-mutation frontier check, failure, retry and dead-letter behavior, system task isolation) and the chat-cleanup outbox handler that deletes the provider files and the vector store of a soft-deleted chat with bounded retries, plus the P1 retention behavior."
---

# Feature: Background Maintenance

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-background-maintenance-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-background-maintenance`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat User Continues a Long Conversation](#chat-user-continues-a-long-conversation)
  - [Chat User Deletes a Chat and Provider Resources Are Released](#chat-user-deletes-a-chat-and-provider-resources-are-released)
  - [Admin Observes Background Maintenance and Recovers Dead Letters](#admin-observes-background-maintenance-and-recovers-dead-letters)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Background Handler Wiring](#background-handler-wiring)
  - [Thread Summary Payload Parsing](#thread-summary-payload-parsing)
  - [Thread Summary Handler](#thread-summary-handler)
  - [Summary Retry Budget](#summary-retry-budget)
  - [Summary Frontier Pre-check](#summary-frontier-pre-check)
  - [Summary Range Selection](#summary-range-selection)
  - [Summary Model Resolution](#summary-model-resolution)
  - [Summary Prompt Construction](#summary-prompt-construction)
  - [Summary Provider Call](#summary-provider-call)
  - [Summary Adapter Call](#summary-adapter-call)
  - [Summary Response Parsing](#summary-response-parsing)
  - [Summary CAS Commit](#summary-cas-commit)
  - [System Task Usage and Isolation](#system-task-usage-and-isolation)
  - [Chat Cleanup Payload Parsing](#chat-cleanup-payload-parsing)
  - [Chat Cleanup Handler](#chat-cleanup-handler)
  - [Chat Attachment File Cleanup](#chat-attachment-file-cleanup)
  - [Chat Secondary File Cleanup](#chat-secondary-file-cleanup)
  - [Vector Store Cleanup](#vector-store-cleanup)
  - [Cleanup Responsibility Boundaries](#cleanup-responsibility-boundaries)
  - [Retention Behavior](#retention-behavior)
  - [Background Maintenance Metrics](#background-maintenance-metrics)
  - [Background Maintenance Configuration](#background-maintenance-configuration)
- [4. States (CDSL)](#4-states-cdsl)
  - [Thread Summary Frontier State Machine](#thread-summary-frontier-state-machine)
  - [Chat Provider Purge State Machine](#chat-provider-purge-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Outbox Handlers Replace the Placeholders](#outbox-handlers-replace-the-placeholders)
  - [Thread Summary Execution](#thread-summary-execution)
  - [Stable Range and CAS Commit](#stable-range-and-cas-commit)
  - [Summary Failure, Retry and Dead Letter](#summary-failure-retry-and-dead-letter)
  - [System Task Isolation](#system-task-isolation)
  - [Chat Deletion Cleanup Handler](#chat-deletion-cleanup-handler)
  - [Vector Store Cleanup Ordering](#vector-store-cleanup-ordering)
  - [Cleanup Ownership Between the Two Paths](#cleanup-ownership-between-the-two-paths)
  - [P1 Retention](#p1-retention)
  - [Observability and Configuration](#observability-and-configuration)
  - [Deferred and Not-Implemented Scope Stays Unimplemented](#deferred-and-not-implemented-scope-stays-unimplemented)
  - [Test Layering, Traceability and Feature-Local Deviations](#test-layering-traceability-and-feature-local-deviations)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

<!--
=============================================================================
FEATURE SPECIFICATION: Background Maintenance (mini-chat)
Source entry: DECOMPOSITION section 2.7 (gears/mini-chat/docs/DECOMPOSITION.md)
Precedence: gears/mini-chat/docs/ (PRD, DESIGN, ADR, acceptance-criteria) wins over
config/mini-chat.yaml and platform documents under docs/ wherever they disagree.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

The Background Maintenance feature keeps long conversations affordable and removes the provider-side data of deleted chats without blocking any user request in the `mini-chat` gear. It provides the two remaining outbox handlers of the gear: the thread-summary handler on the queue `mini-chat.thread_summary`, which turns the frozen message range enqueued by a completed turn into one committed summary per range (summary model call through OAGW, compare-and-set commit, `is_compressed` marking, system-task usage event), and the chat-cleanup handler on the queue `mini-chat.chat_cleanup`, which deletes the provider files and then the provider vector store of a soft-deleted chat with idempotent, bounded retries. Both replace the placeholder `UnwiredHandler` that the foundation registered on those queues.

### 1.2 Purpose

The PRD requires older conversation history to be compressed into a summary when a conversation approaches the context budget, with the previous summary kept when the provider call fails, and requires chat deletion to return at once while a cleanup worker deletes the chat's provider files and vector store with idempotent retries, best effort within one hour. The DESIGN fixes the frozen-range work item, the stable range and commit invariant (at most one successful commit per frozen range, compare-and-set on the base frontier, a target-frontier check against concurrent turn mutations), the summary request and response format, system task isolation (no `chat_turns` row, no per-user quota, a zero-credit `system_task` usage event), the cleanup responsibility boundaries, the per-attachment cleanup state transitions, the vector store cleanup ordering and the P1 retention limits. This feature realizes those contracts on the shared transactional outbox, without a gear-local polling, claim or leader-elected worker.

Scope boundary: the trigger evaluation and the in-transaction enqueue of the thread-summary work item are owned by `cpt-cf-mini-chat-feature-message-streaming` (`cpt-cf-mini-chat-algo-message-streaming-summary-trigger`), as are context assembly that reads the committed summary, the provider adapter kinds, the streaming request construction and wire translation, the provider error sanitizer and the provider registry. This feature owns the non-streaming summary completion call itself (`cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`): the request body per adapter kind, the OAGW call, the extraction of the response text and usage, and the mapping of its failures to the summary outcomes; the clause of `cpt-cf-mini-chat-algo-message-streaming-adapter-kinds` that each adapter provides this call names the contract and specifies no step. The deletion of a summary by retry, edit or delete is owned by `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation`); this feature owns only the worker's commit check against such a mutation. The chat DELETE transaction and the chat-cleanup payload are owned by `cpt-cf-mini-chat-feature-chat-management` (`cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue`). The `attachments.cleanup_status` state machine, the `attachment_cleanup` handler, the storage dispatcher, the Files and Vector Stores client and the Anthropic Files client are owned by `cpt-cf-mini-chat-feature-attachments-and-file-search`; this feature applies that state machine's transitions to the attachments of a deleted chat and realizes the transition `inst-vsm-chat-cleanup` of `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`. The usage payload contract of system tasks and the usage outbox handler are owned by `cpt-cf-mini-chat-feature-quota-and-billing`. The queues, their partitioning and leases, the shared outbox tuning, the configuration sections, the schema, the policy snapshot access, the provider registry and OAGW provisioning are owned by `cpt-cf-mini-chat-feature-gear-foundation`. The orphan watchdog (`cpt-cf-mini-chat-feature-message-streaming`) and the upload reaper (`cpt-cf-mini-chat-feature-attachments-and-file-search`) are not part of this feature, although the foundation's gear start names this feature for them (DECOMPOSITION 2.4 and 2.5 assign them elsewhere). Those elements are referenced here, not redefined.

**Requirements**: `cpt-cf-mini-chat-fr-thread-summary`, `cpt-cf-mini-chat-fr-chat-deletion-cleanup`, `cpt-cf-mini-chat-nfr-data-retention`

**Principles**: `cpt-cf-mini-chat-principle-summary-over-history`

**Constraints**: none owned (DECOMPOSITION 2.7). The provider calls of both handlers preserve `cpt-cf-mini-chat-constraint-no-credentials`, which is owned by `cpt-cf-mini-chat-feature-gear-foundation`; system tasks are outside per-user quota enforcement (DESIGN 3.2 `System Task Isolation Invariant (P1)`).

**Design elements**: `cpt-cf-mini-chat-seq-thread-summary` (the asynchronous part from the delivery of the work item on), `cpt-cf-mini-chat-seq-chat-deletion-cleanup` (the asynchronous part from the delivery of the chat-cleanup message on). Consumed: `cpt-cf-mini-chat-component-llm-provider` (non-streaming summary call, file and vector store dispatch by `storage_kind`, Anthropic Files client), `cpt-cf-mini-chat-design-turn-mutations` (summary interaction on turn mutation), `cpt-cf-mini-chat-design-vector-store-scope` (one store per chat).

**Database tables used (created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their summary and cleanup behavior)**: written: `thread_summaries` (insert and compare-and-set update), `messages` (`is_compressed` only), `attachments` (cleanup columns of a deleted chat's rows), `chat_vector_stores` (row deletion after the provider store is gone). Read only: `chats`. The table IDs are listed in the definitions of done.

**Domain entities**: `ThreadSummary`, the thread-summary work item (outbox message), the chat-cleanup outbox message, the system-task `UsageEvent`.

**Use case touchpoint**: step 4 (asynchronous cleanup) of `cpt-cf-mini-chat-usecase-delete-chat`, which is owned by `cpt-cf-mini-chat-feature-chat-management`.

**ADRs traced**: `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (cleanup and retention parts: no hard purge, no chat-deletion audit event, a running turn is not cancelled), `cpt-cf-mini-chat-adr-quota-policy-scope` (system tasks: no tenant operational bucket, no audit event, no kill-switch check), `cpt-cf-mini-chat-adr-document-retrieval-scope` (no document summary task), `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (no CHECK on `cleanup_status`, deprecated `cleanup_worker` and `thread_summary_worker.reconcile_interval_secs` keys, application clock), `cpt-cf-mini-chat-adr-multi-provider-adapters` (summary call through every adapter kind, Anthropic secondary file).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Triggers summary work by completing turns of a long conversation and receives later answers whose context carries the committed summary; deletes a chat and gets 204 at once while its provider resources are released in the background. Never waits for either handler. |
| `cpt-cf-mini-chat-actor-cleanup-scheduler` | Runs as the chat-cleanup outbox handler (provider file and vector store deletion for soft-deleted chats) and, as the gear's background system process, as the thread-summary outbox handler; no human interaction and no user security context. |
| `cpt-cf-mini-chat-actor-admin` | Observes summary execution, CAS conflicts, fallbacks and cleanup outcomes through low-cardinality metrics, inspects and replays dead-lettered summary and chat-cleanup messages through the shared outbox tooling; never sees chat content, summary text or provider identifiers. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (`Thread Summary Compression`, `Chat Deletion with Resource Cleanup`, `Data Retention Compliance`, UC-004 `Delete Chat`, UC-005 `Temporary Chat Auto-Deletion (P2)`, `Cleanup Scheduler` actor)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) section 2.7 (`Background Maintenance`)
- **Acceptance criteria**: [acceptance-criteria.md](../acceptance-criteria.md) section `Cleanup & Recovery`
- **ADRs (all accepted)**: [ADR-0005](../ADR/0005-cpt-cf-mini-chat-adr-multi-provider-adapters.md), [ADR-0007](../ADR/0007-cpt-cf-mini-chat-adr-document-retrieval-scope.md), [ADR-0008](../ADR/0008-cpt-cf-mini-chat-adr-quota-policy-scope.md), [ADR-0009](../ADR/0009-cpt-cf-mini-chat-adr-data-lifecycle-audit-scope.md), [ADR-0010](../ADR/0010-cpt-cf-mini-chat-adr-runtime-consistency-limitations.md)
- **Cross-feature implementation notes**: `.cf-studio/.cache/mini-chat/implementation-notes.md` items 1 and 2 (range selection, the frontier comparison and the `is_compressed` range use the fixed-width chronological sort key on SQLite with `id` as tie-break, never raw text comparison of `created_at`), 3 (repository functions take a runner so the summary upsert, the `is_compressed` update and the usage enqueue share one transaction; the outbox wake fires only after commit), 4 (no PEP call: both handlers run outside a user scope, which is why every query filters on `tenant_id` and `chat_id` from the payload), 5 (every provider call goes through OAGW under the gear's S2S security context with the URI `/{upstream_alias}{api_path}`), 6 (domain layering: the handler logic lives in `domain/` behind ports implemented in `infra/`), 7 (the chat-cleanup and thread-summary queues still carry the placeholder `UnwiredHandler`, which this feature replaces; the shared outbox tuning is global with batch 1 and a 30 s retry cap), 11 (provider HTTP guidance: JSON content type, `api_path` used verbatim with only `{model}` substituted, no credential header, gateway versus upstream failures distinguished, error bodies read bounded and sanitized)
- **Sibling features**: [0001-gear-foundation.md](./0001-gear-foundation.md) (outbox queues with the `UnwiredHandler` slots, partitioning by `chat_id`, leases, configuration validation including `thread_summary_worker` and `cleanup_worker`, gear start with the summary model check, policy snapshot access, provider registry, observability instruments), [0002-chat-management.md](./0002-chat-management.md) (chat DELETE transaction that marks attachments `pending` and enqueues the chat-cleanup message), [0003-quota-and-billing.md](./0003-quota-and-billing.md) (system-task usage event fields and the usage outbox handler), [0004-message-streaming.md](./0004-message-streaming.md) (summary trigger and work item payload, context assembly reading `thread_summaries`, provider adapter kinds, request construction and wire translation of the streaming call; the non-streaming summary call is specified by this feature), [0005-attachments-and-file-search.md](./0005-attachments-and-file-search.md) (cleanup status state machine, attachment cleanup handler that acknowledges messages of deleted chats, storage dispatcher, Files, Vector Stores and Anthropic Files clients, chat vector store state machine), [0006-turn-mutations-and-reactions.md](./0006-turn-mutations-and-reactions.md) (summary invalidation on retry, edit and delete)
- **Dependencies**: `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-message-streaming`, `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`, `cpt-cf-mini-chat-feature-attachments-and-file-search` (and through them `cpt-cf-mini-chat-feature-gear-foundation` and `cpt-cf-mini-chat-feature-quota-and-billing`). No feature builds on this one.

**Upstream payload contracts consumed** (as enqueued by the current code in `gears/mini-chat/mini-chat/src/infra/outbox.rs`):

| Queue (default name) | Payload type | Partition key | Fields |
|----------------------|--------------|---------------|--------|
| `outbox.thread_summary_queue_name` (`mini-chat.thread_summary`) | `mini_chat.thread_summary` | `chat_id` | `tenant_id`, `chat_id`, `system_request_id`, `system_task_type` (`thread_summary_update`), `base_frontier_created_at` and `base_frontier_message_id` (both null when the frontier was empty), `frozen_target_created_at`, `frozen_target_message_id` (timestamps in RFC 3339), `dedupe_key` (`{tenant_id}/thread_summary_update/{system_request_id}`, simple UUID form) |
| `outbox.chat_cleanup_queue_name` (`mini-chat.chat_cleanup`) | `mini_chat.chat_cleanup` | `chat_id` | `tenant_id`, `chat_id`, `system_request_id`, `reason` (`chat_soft_delete`), `chat_deleted_at` (RFC 3339), `dedupe_key` (`{tenant_id}/chat_cleanup/{system_request_id}`, simple UUID form) |

**Normative value sources** (the DESIGN holds the exact values; this document states the values it must preserve and names the DESIGN heading that carries the rest):

| Topic | DESIGN location (section and heading) |
|-------|---------------------------------------|
| Summary principle | 2.1 `Summary Over History` |
| Summary sequence, execution stages, request and response format, observability | 3.6 `Thread Summary Update` (`Trigger timing`, `Execution stages and invariants`) |
| Frontier, range selection, frozen-range commit invariant, recovery paths | 3.6 `Thread Summary - Stable Range and Commit Invariant` |
| Commit check against a concurrent mutation | 3.9 `Summary Interaction on Turn Mutation` |
| System task identity, isolation and usage payload | 3.2 `System Task Isolation Invariant (P1)`, `System Task Attribution Rules (Normative)` |
| Summary request metadata and system identity | 4 `Provider Request Metadata` |
| Cleanup sequence, boundaries, state machine, payload semantics, vector store ordering, invariants, observability | 3.6 `Cleanup on Chat Deletion` (`Cleanup responsibility boundaries`, `Attachment cleanup state machine`, `Outbox payload and execution semantics`, `Vector store cleanup ordering`, `Additional invariants`); 4 `Cleanup on Chat Deletion` |
| Tables | 3.7 `Table: thread_summaries`, `Table: messages`, `Table: attachments`, `Table: chat_vector_stores`, `Table: chats` |
| Retention | 4 `Data Classification and Retention (P1)`, `P1 Scope Boundaries` |
| Metrics and alerts | 4 `Metrics (OTLP) and Alerts (P1)` (areas Thread summary and Cleanup) |
| Configuration and prompts | Appendix B `B.5.5`, `B.9.2`, `B.9.3`, `B.9.4`, `B.11` |

## 2. Actor Flows (CDSL)

End-to-end interactions in which the chat user's actions cause background work and the admin observes and recovers it. Neither flow adds an endpoint: the summary work is enqueued by the send pipeline and the cleanup message by the chat DELETE transaction; the shared outbox delivers both to the handlers of section 3.

**Use cases**: `cpt-cf-mini-chat-usecase-delete-chat` (asynchronous cleanup step 4 only). `cpt-cf-mini-chat-usecase-temporary-chat-cleanup` is deferred to P2 and not implemented.

### Chat User Continues a Long Conversation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-background-maintenance-long-conversation`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A completed turn whose context reached the compression threshold, or was truncated, enqueues one work item; the summary handler commits a summary over exactly the frozen range, marks that range `is_compressed`, and enqueues one `system_task` usage event; the next turn's `stream_started` carries `thread_summary_applied` and its context holds the summary followed by the uncompressed messages after the frontier.
- The user's current turn never waits for the summary and its answer is unchanged by it; a summary serves only later turns.

**Error Scenarios**:
- The summary provider call fails or times out: the previous summary (or none) stays in effect, no message is marked compressed, the work item is retried with backoff and dead-lettered on its `thread_summary_worker.max_attempts`-th delivery (default 3); later turns keep working with truncation until a later trigger supersedes the range.
- The summary model is missing or disabled: the work item is rejected at once (dead letter) with `result` `model_unavailable`; turns keep enqueuing work.
- A retry, edit or delete removed the target frontier message, or deleted the summary the work item was based on, while the handler ran: the handler commits nothing and acknowledges the message.

**Steps**:
1. [x] - `p1` - Chat User completes a turn through the send flow `cpt-cf-mini-chat-flow-message-streaming-send-message`; its finalization transaction evaluates the trigger and enqueues the work item through `inst-sum-enqueue` of `cpt-cf-mini-chat-algo-message-streaming-summary-trigger`, which this feature does not change - `inst-lc-turn-completes`
2. [x] - `p1` - The shared outbox delivers the work item from `outbox.thread_summary_queue_name` to the thread-summary handler, one chat's items in order on the partition of its `chat_id`, under the lease `thread_summary_worker.claim_timeout_secs` (default 300 s) - `inst-lc-deliver`
3. [x] - `p1` - Run the thread summary handler algorithm on the delivery - `inst-lc-handle`
4. [x] - `p1` - **IF** the handler returned `Retry` - `inst-lc-retry`
   1. [x] - `p1` - The shared outbox redelivers the same message with the same `system_request_id` after its backoff (capped at 30 s by the global tuning); until a commit succeeds the previous summary stays in effect - `inst-lc-redeliver`
5. [x] - `p1` - **IF** the handler returned `Reject` - `inst-lc-rejected`
   1. [x] - `p1` - The message moves to the shared dead-letter store and the frontier stays where it was; recovery is a later trigger with the same base and a wider target, or an operator replay (admin flow) - `inst-lc-dead-letter`
6. [x] - `p1` - Chat User sends the next message; context assembly of `cpt-cf-mini-chat-algo-message-streaming-context-assembly` loads the committed summary (`inst-ctx-load-summary`), skips messages with `is_compressed` true, and `stream_started` reports `thread_summary_applied` with the stored `token_estimate` - `inst-lc-next-turn`
7. [x] - `p1` - **RETURN** the conversation with bounded context cost - `inst-lc-return`

### Chat User Deletes a Chat and Provider Resources Are Released

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- `DELETE /v1/chats/{id}` returns 204 at once; within the following deliveries every provider file of the chat's `pending` attachment rows (documents and images, plus any Anthropic secondary copy) is deleted or found already gone (404), each row becomes `done`, and then the chat's provider vector store is deleted (or found already gone) and its `chat_vector_stores` row removed; the best-effort target is one hour under normal conditions.
- Local rows are kept: the chat stays soft-deleted, its child rows keep `deleted_at` empty and stay unreachable, nothing is hard-purged and no audit event is emitted.

**Error Scenarios**:
- A provider file delete keeps failing: the row's `cleanup_attempts` grows by one per delivery; at `cleanup_worker.max_attempts` (default 5) the row becomes terminal `failed` (provider-file debt) and the handler continues with the other rows; the vector store is still deleted once no row is `pending`, and `mini_chat_cleanup_vector_store_with_failed_attachments` is recorded.
- The vector store delete keeps failing: the `chat_vector_stores` row is kept and the message is retried until the delivery that reaches `cleanup_worker.max_attempts`, which rejects it (dead letter); a replay retries the delete.
- A database error at any step: the delivery returns `Retry` without consuming any attempt budget.
- A provider call (file delete, secondary file delete or vector store delete) hits its fixed 5 s deadline: it counts as a failed call of that resource under the rules above (a failed file attempt, a not-retried secondary delete, a failed store delete). A delivery that has used its fixed 20 s time budget starts no further provider call and returns `Retry` while a row is still `pending` or the store is still to be deleted; the next delivery continues from the persisted state.

**Steps**:
1. [x] - `p1` - Chat User sends `DELETE /v1/chats/{id}`; the delete flow of `cpt-cf-mini-chat-feature-chat-management` soft-deletes the chat, marks its attachments `cleanup_status` `pending` and enqueues the chat-cleanup message in one transaction through `cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue`, and answers 204 without any provider call - `inst-cd-delete`
2. [x] - `p1` - The shared outbox delivers the message from `outbox.chat_cleanup_queue_name` to the chat-cleanup handler, partitioned by `chat_id`, under the shared default lease of 30 s; the handler bounds every provider call by a fixed 5 s deadline and the whole delivery by a fixed 20 s time budget, so a delivery ends inside its lease - `inst-cd-deliver`
3. [x] - `p1` - The Cleanup Scheduler runs the chat cleanup handler algorithm on the delivery - `inst-cd-handle`
4. [x] - `p1` - **IF** the handler returned `Retry` - `inst-cd-retry`
   1. [x] - `p1` - The shared outbox redelivers the same message after its backoff; the next delivery re-reads the attachment rows and the `chat_vector_stores` row and continues from the persisted state - `inst-cd-redeliver`
5. [x] - `p1` - **IF** the handler returned `Reject` - `inst-cd-rejected`
   1. [x] - `p1` - The message moves to the shared dead-letter store; a kept `chat_vector_stores` row marks the outstanding store delete for an operator replay - `inst-cd-dead-letter`
6. [x] - `p1` - Chat User keeps observing only the deletion: the chat and its sub-resources return 404, and no endpoint exposes cleanup progress - `inst-cd-user-view`
7. [x] - `p1` - **RETURN** the chat provider-purged, or purged with recorded provider-file debt - `inst-cd-return`

### Admin Observes Background Maintenance and Recovers Dead Letters

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-background-maintenance-admin-observability`

**Actor**: `cpt-cf-mini-chat-actor-admin`

**Success Scenarios**:
- The admin sees summary execution outcomes, CAS conflicts and fallbacks, cleanup completions, retries and terminal failures per resource type, vector store deletions that left file debt and skipped secondary deletions, all without tenant, user, chat, attachment, file or vector store identifiers in labels.
- A dead-lettered work item or cleanup message is replayed through the shared outbox tooling and processed against the current persisted state.

**Error Scenarios**:
- No outbox dead-letter metric series exists in P1 (DESIGN lists it as not defined), so dead-letter visibility comes from the shared outbox store and tooling.
- A `failed` attachment row of a deleted chat cannot be cleared by any replay in P1: the chat-cleanup handler processes only `pending` rows and the attachment cleanup handler acknowledges messages of deleted chats without action; clearing that debt needs operator intervention outside the gear or a P2 recovery policy.

**Steps**:
1. [x] - `p1` - Admin reads the series of the background maintenance metrics algorithm from the OTLP pipeline - `inst-adm-read-metrics`
2. [x] - `p1` - **IF** `mini_chat_thread_summary_execution{result="provider_error"}` or `mini_chat_summary_fallback` rises, or `mini_chat_thread_summary_cas_conflicts` rises above its background rate - `inst-adm-summary-alert`
   1. [x] - `p1` - Admin inspects the dead-lettered thread-summary messages in the shared outbox store and replays one after the cause is fixed; the replay re-runs the handler with the same `system_request_id` and commits only if the stored frontier still equals the message's base frontier - `inst-adm-summary-replay`
   2. [x] - `p1` - Admin may re-drive a stuck frontier by enqueueing a replacement work item for the same or a wider frozen range through the shared outbox tooling when no new turn triggers a supersession; no admin route is added - `inst-adm-summary-redrive`
3. [x] - `p1` - **IF** `mini_chat_cleanup_failed{resource_type="vector_store"}` grows - `inst-adm-store-alert`
   1. [x] - `p1` - Admin replays the dead-lettered chat-cleanup message; the handler finds no `pending` row and retries the vector store delete once per delivery - `inst-adm-store-replay`
4. [x] - `p1` - **IF** `mini_chat_cleanup_failed{resource_type="file"}` or `mini_chat_cleanup_vector_store_with_failed_attachments` grows - `inst-adm-file-debt`
   1. [x] - `p1` - Admin queries the `attachments` rows of deleted chats with `cleanup_status` `failed` and their `last_cleanup_error`, which holds no file content and no provider text, and records the provider-file debt for manual or P2 recovery - `inst-adm-file-debt-query`
5. [x] - `p1` - Admin reads queue lag, retry backlog, lease churn and dead-letter depth from the shared outbox surface, not from Mini Chat series - `inst-adm-outbox-surface`
6. [x] - `p1` - **RETURN** the observed state - `inst-adm-return`

## 3. Processes / Business Logic (CDSL)

The handler algorithms and their building blocks. Both handlers run outside any user security context, call no PDP and filter every query on `tenant_id` and `chat_id` taken from the payload (secure ORM tenant scoping of `cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence`). Repository functions take the caller's transaction runner. On SQLite every timestamp order, range and equality uses the fixed-width chronological sort key with `id` as tie-break, and UUID values are bound as their 16-byte form (DESIGN 3.7). Every provider call goes through the in-process OAGW client `ServiceGatewayClientV1` with the URI `/{upstream_alias}{api_path}` under the gear's S2S context; OAGW injects credentials.

### Background Handler Wiring

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-handler-wiring`

**Input**: the queue registrations of `cpt-cf-mini-chat-algo-gear-foundation-outbox-queues`, the services built at gear init and the validated configuration

**Output**: the thread-summary handler and the chat-cleanup handler serving their queues. Rules: DESIGN 3.6 `Thread Summary Update` (stage 3), `Cleanup on Chat Deletion`, B.9.3; implementation note 7.

**Steps**:
1. [x] - `p1` - Build the thread-summary handler with its collaborators: the summary repository, the policy snapshot access of `cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access`, the provider registry of `cpt-cf-mini-chat-algo-gear-foundation-provider-registry`, the provider adapters of `cpt-cf-mini-chat-algo-message-streaming-adapter-kinds`, the usage enqueuer, the clock and the metrics - `inst-hw-build-summary`
2. [x] - `p1` - Build the chat-cleanup handler with its collaborators: the cleanup repository, the storage provider client and, when an `anthropic_messages` provider exists, the Anthropic Files client of `cpt-cf-mini-chat-feature-attachments-and-file-search`, the provider registry, the policy snapshot access of `cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access` (for the secondary file target), an injectable monotonic delivery timer, the clock and the metrics - `inst-hw-build-cleanup`
3. [x] - `p1` - Register the thread-summary handler on the thread-summary queue and the chat-cleanup handler on the chat-cleanup queue in place of `UnwiredHandler`, through the handler slot of `inst-outbox-register`; keep the partitioning by `chat_id`, the leases (`thread_summary_worker.claim_timeout_secs` for summaries, the shared default 30 s for chat cleanup) and the global processor tuning (batch 1, retry backoff capped at 30 s) unchanged - `inst-hw-register`
4. [x] - `p1` - Map each domain outcome to the shared outbox result `Ok`, `Retry` or `Reject(reason)` (`inst-outbox-outcomes`); a reason never carries message content, summary text or a provider identifier - `inst-hw-outcomes`
5. [x] - `p1` - Read the delivery count from the message's `attempts`, which is 0 on the first delivery, so the N-th delivery carries `attempts` N minus 1 - `inst-hw-attempts`
6. [x] - `p1` - Define no gear-local polling loop, claim or reclaim table, leader-elected worker or task state machine for either path; delivery, partitioned ordering, lease and reclaim, retries with backoff, dead-letter handling and reconciliation are the shared outbox's - `inst-hw-no-local-worker`
7. [x] - `p1` - Keep the thread-summary handler wired when `thread_summary_worker.enabled` is false (the flag gates only the trigger of `cpt-cf-mini-chat-feature-message-streaming`), so already enqueued work still runs; the deprecated `cleanup_worker` keys and `thread_summary_worker.reconcile_interval_secs` change nothing (ADR-0010) - `inst-hw-enabled-flag`
8. [x] - `p1` - On gear stop, let the foundation's gear stop stop the outbox pipeline; an interrupted delivery is redelivered after its lease and re-runs idempotently - `inst-hw-stop`
9. [x] - `p1` - **RETURN** the wired handlers - `inst-hw-return`

### Thread Summary Payload Parsing

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-payload`

**Input**: the payload bytes of one thread-summary message

**Output**: the work item (`tenant_id`, `chat_id`, `system_request_id`, optional base frontier, frozen target frontier) or a malformed reason. The field list is DESIGN 3.6 `Execution stages and invariants` stage 2 and the enqueue of `inst-sum-enqueue`.

**Steps**:
1. [x] - `p1` - Parse the bytes as a JSON object - `inst-sp-parse`
2. [x] - `p1` - Read `tenant_id`, `chat_id` and `system_request_id` as UUIDs - `inst-sp-ids`
3. [x] - `p1` - Require `system_task_type` equal to `thread_summary_update` - `inst-sp-task-type`
4. [x] - `p1` - Read the frozen target from `frozen_target_created_at` (RFC 3339) and `frozen_target_message_id` (UUID) - `inst-sp-target`
5. [x] - `p1` - Read the base frontier from `base_frontier_created_at` and `base_frontier_message_id`: both null means an empty base, both set means a base frontier, one of them set alone is malformed - `inst-sp-base`
6. [x] - `p1` - **IF** a `dedupe_key` is present, require that it starts with the 32-character lowercase hex form of `tenant_id` followed by `/` - `inst-sp-dedupe`
7. [x] - `p1` - **IF** any check above failed - `inst-sp-malformed`
   1. [x] - `p1` - **RETURN** the malformed reason naming the field, without any payload value - `inst-sp-return-malformed`
8. [x] - `p1` - **RETURN** the work item; the target is taken from the payload as frozen at enqueue and is never recomputed - `inst-sp-return`

### Thread Summary Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-handler`

**Input**: one leased thread-summary message and its `attempts`

**Output**: `Ok`, `Retry` or `Reject(reason)`, at most one committed summary for the frozen range, and the execution metrics. Rules: DESIGN 3.6 `Execution stages and invariants` stages 3 and 4, `Thread Summary - Stable Range and Commit Invariant`, 3.9 `Summary Interaction on Turn Mutation`, B.9.4.

**Steps**:
1. [x] - `p1` - Parse the payload through the thread summary payload parsing algorithm - `inst-sh-parse`
2. [x] - `p1` - **IF** the payload is malformed - `inst-sh-malformed`
   1. [x] - `p1` - Log an error with the reason and **RETURN** `Reject` at once (dead letter); no execution result is recorded because the allowlist has none for it - `inst-sh-return-malformed`
3. [x] - `p1` - Run the summary frontier pre-check algorithm - `inst-sh-precheck`
4. [x] - `p1` - **MATCH** the pre-check result - `inst-sh-precheck-match`
   - [x] - `p1` - **CASE** chat gone (missing or soft-deleted): **RETURN** `Ok` without a provider call and without an execution result - `inst-sh-case-chat-gone`
   - [x] - `p1` - **CASE** base missing: record `mini_chat_thread_summary_execution{result="base_missing"}` and **RETURN** `Ok`; the next trigger builds a summary with no base - `inst-sh-case-base-missing`
   - [x] - `p1` - **CASE** frontier moved: increment `mini_chat_thread_summary_cas_conflicts` and **RETURN** `Ok`; another commit already advanced the frontier - `inst-sh-case-conflict`
   - [x] - `p1` - **CASE** target deleted: record `mini_chat_thread_summary_execution{result="frontier_deleted"}` and **RETURN** `Ok` - `inst-sh-case-target-deleted`
   - [x] - `p1` - **CASE** database error: record `result="retry"` and **RETURN** the outcome of the summary retry budget algorithm - `inst-sh-case-precheck-db`
   - [x] - `p1` - **CASE** ready: continue with the stored summary text of the base (absent for an empty base) - `inst-sh-case-ready`
5. [x] - `p1` - Load the frozen range through the summary range selection algorithm - `inst-sh-range`
6. [x] - `p1` - **IF** the range load failed with a database error - `inst-sh-range-db`
   1. [x] - `p1` - Record `result="retry"` and **RETURN** the outcome of the summary retry budget algorithm - `inst-sh-return-range-db`
7. [x] - `p1` - **IF** the range holds no message - `inst-sh-range-empty`
   1. [x] - `p1` - **RETURN** `Ok` without a provider call; there is nothing to summarize - `inst-sh-return-range-empty`
8. [x] - `p1` - Resolve the summary model through the summary model resolution algorithm - `inst-sh-model`
9. [x] - `p1` - **IF** the summary model is missing from the catalog or disabled - `inst-sh-model-unavailable`
   1. [x] - `p1` - Log an error naming the configured model id, record `result="model_unavailable"` and **RETURN** `Reject` at once (dead letter, no retry) - `inst-sh-return-model-unavailable`
10. [x] - `p1` - **IF** another resolution error occurred (policy snapshot or provider for the tenant unavailable) - `inst-sh-resolution-error`
    1. [x] - `p1` - Record `result="retry"` and **RETURN** the outcome of the summary retry budget algorithm - `inst-sh-return-resolution-error`
11. [x] - `p1` - Build and fit the prompt through the summary prompt algorithm - `inst-sh-prompt`
12. [x] - `p1` - Call the summary model through the summary provider call algorithm - `inst-sh-call`
13. [x] - `p1` - **IF** the call failed (provider error, gateway error, timeout, or a context-length rejection after the bounded prompt-too-long retries) - `inst-sh-call-failed`
    1. [x] - `p1` - Keep the previous summary unchanged, advance no frontier and mark no message compressed; record `result="provider_error"`, increment `mini_chat_summary_fallback` and **RETURN** the outcome of the summary retry budget algorithm - `inst-sh-return-call-failed`
14. [x] - `p1` - Parse the response through the summary response parsing algorithm - `inst-sh-parse-response`
15. [x] - `p1` - **IF** the parsed summary is empty - `inst-sh-empty`
    1. [x] - `p1` - Record `result="empty_summary"` (no fallback increment) and **RETURN** the outcome of the summary retry budget algorithm - `inst-sh-return-empty`
16. [x] - `p1` - Run the summary CAS commit algorithm with the parsed text, its `token_estimate` and the usage of the call - `inst-sh-commit`
17. [x] - `p1` - **MATCH** the commit result - `inst-sh-commit-match`
    - [x] - `p1` - **CASE** committed: fire the usage outbox wake after the commit, record `result="success"` and **RETURN** `Ok` - `inst-sh-case-committed`
    - [x] - `p1` - **CASE** target deleted: record `result="frontier_deleted"` and **RETURN** `Ok`; no summary, no `is_compressed` change and no usage event were written - `inst-sh-case-commit-target-deleted`
    - [x] - `p1` - **CASE** base missing: record `result="base_missing"` and **RETURN** `Ok` - `inst-sh-case-commit-base-missing`
    - [x] - `p1` - **CASE** frontier moved: increment `mini_chat_thread_summary_cas_conflicts` and **RETURN** `Ok` without another commit attempt for this frozen range - `inst-sh-case-commit-conflict`
    - [x] - `p1` - **CASE** database or enqueue error: record `result="retry"` and **RETURN** the outcome of the summary retry budget algorithm - `inst-sh-case-commit-db`
18. [x] - `p1` - Accept duplicate provider calls for the same frozen range caused by retry, replay or lease expiry as a P1 operational side effect; correctness rests on the CAS commit, never on partition ordering alone - `inst-sh-duplicates-accepted`
19. [x] - `p1` - Never log message content, the prompt or the summary text; log the chat id, the `system_request_id` and the outcome only - `inst-sh-no-content-logs`
20. [x] - `p1` - **RETURN** the outcome - `inst-sh-return`

### Summary Retry Budget

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget`

**Input**: the delivery's `attempts` and `thread_summary_worker.max_attempts` (default 3, greater than 0)

**Output**: `Retry`, or `Reject` on the last allowed delivery. Rule: DESIGN 3.6 stage 3 (`A handler attempt that would return Retry on its max_attempts-th delivery returns Reject instead`), B.9.4.

**Steps**:
1. [x] - `p1` - **IF** this delivery is the `max_attempts`-th or a later one (`attempts` at least `max_attempts` minus 1) - `inst-rb-last`
   1. [x] - `p1` - Log a warning with the chat id, the `system_request_id` and the last result and **RETURN** `Reject` with the reason `thread summary: max attempts (N) reached`; the message is dead-lettered so a persistent failure does not block other chats of the partition, and the frontier stays unchanged - `inst-rb-return-reject`
2. [x] - `p1` - **ELSE** - `inst-rb-else`
   1. [x] - `p1` - **RETURN** `Retry`; the shared outbox applies its lease-aware backoff capped at 30 s - `inst-rb-return-retry`

### Summary Frontier Pre-check

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck`

**Input**: the work item

**Output**: `ready` with the base summary text, or `chat gone`, `base missing`, `frontier moved`, `target deleted`, `database error`. Rules: DESIGN 3.6 stage 4 and `Range stability and concurrency`; 3.7 `Table: thread_summaries` (frontier semantics, a missing row is an empty frontier). The pre-check saves provider calls that cannot commit; the commit algorithm repeats the decisive checks atomically.

**Steps**:
1. [x] - `p1` - DB: SELECT the `chats` row by `tenant_id` and `chat_id` without the deleted filter - `inst-pc-load-chat`
2. [x] - `p1` - **IF** the row is missing or its `deleted_at` is set - `inst-pc-chat-gone`
   1. [x] - `p1` - **RETURN** `chat gone`; the content of a deleted chat is not sent to the provider again - `inst-pc-return-chat-gone`
3. [x] - `p1` - DB: SELECT the chat's `thread_summaries` row by `tenant_id` and `chat_id` (summary text and frontier `(summarized_up_to_created_at, summarized_up_to_message_id)`) - `inst-pc-load-summary`
4. [x] - `p1` - **IF** the work item has a base frontier and no row exists - `inst-pc-base-missing`
   1. [x] - `p1` - **RETURN** `base missing`; a retry, edit or delete deleted the summary after the enqueue - `inst-pc-return-base-missing`
5. [x] - `p1` - **IF** the work item has an empty base and a row exists, or the stored frontier differs from the base frontier in `created_at` or `id` (compared through the sort key on SQLite and the native type on PostgreSQL) - `inst-pc-moved`
   1. [x] - `p1` - **RETURN** `frontier moved` - `inst-pc-return-moved`
6. [x] - `p1` - DB: SELECT the target message by `tenant_id`, `chat_id` and `frozen_target_message_id` - `inst-pc-load-target`
7. [x] - `p1` - **IF** the target message is missing or its `deleted_at` is set - `inst-pc-target-deleted`
   1. [x] - `p1` - **RETURN** `target deleted` - `inst-pc-return-target-deleted`
8. [x] - `p1` - **IF** any query above failed - `inst-pc-db-error`
   1. [x] - `p1` - **RETURN** `database error` - `inst-pc-return-db-error`
9. [x] - `p1` - **RETURN** `ready` with the stored summary text when the base is not empty - `inst-pc-return-ready`

### Summary Range Selection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection`

**Input**: the work item

**Output**: the messages of the frozen range in the strict order `(created_at ASC, id ASC)`. Rule: DESIGN 3.6 `Range selection`, 3.7 `Table: messages` (ordering note).

**Steps**:
1. [x] - `p1` - DB: SELECT `id`, `role`, `content` and `created_at` from `messages` of the chat (filter `tenant_id` and `chat_id`) with `deleted_at IS NULL` and `is_compressed` false, ordered after the base frontier when the base is not empty and at or before the frozen target, in the order `created_at` then `id` - `inst-rs-select`
2. [x] - `p1` - Compare and order through the fixed-width sort key on SQLite with `id` as tie-break, so messages with equal `created_at` and fractional seconds of different lengths keep their total order; use the native type on PostgreSQL - `inst-rs-order`
3. [x] - `p1` - Exclude every message created after the frozen target; such messages neither cancel, widen nor invalidate this run and wait for a later cycle - `inst-rs-exclude-later`
4. [x] - `p1` - **RETURN** the ordered messages - `inst-rs-return`

### Summary Model Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution`

**Input**: `thread_summary_worker.summary_model_id` and the chat's `tenant_id`

**Output**: the summary model's catalog entry and its resolved provider, `model unavailable`, or a resolution error. Rules: DESIGN 3.6 stage 3 (summary model), B.9.4, 4 `Model Catalog Configuration` (`thread_summary_prompt`).

**Steps**:
1. [x] - `p1` - Take `thread_summary_worker.summary_model_id`; an empty value means `gpt-4.1-mini` - `inst-mr-model-id`
2. [x] - `p1` - Read the current policy snapshot through `cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access` - `inst-mr-snapshot`
3. [x] - `p1` - **IF** the snapshot cannot be read - `inst-mr-snapshot-failed`
   1. [x] - `p1` - **RETURN** a resolution error - `inst-mr-return-snapshot-failed`
4. [x] - `p1` - Look the id up in the catalog with the enabled filter - `inst-mr-lookup`
5. [x] - `p1` - **IF** no enabled entry has the id (missing or disabled) - `inst-mr-unavailable`
   1. [x] - `p1` - **RETURN** `model unavailable` - `inst-mr-return-unavailable`
6. [x] - `p1` - Resolve the entry's `provider_id` for the chat's tenant through `cpt-cf-mini-chat-algo-gear-foundation-provider-registry` (tenant overrides applied) to the adapter `kind`, the `upstream_alias` and the `api_path` - `inst-mr-provider`
7. [x] - `p1` - **IF** the provider cannot be resolved for the tenant - `inst-mr-provider-failed`
   1. [x] - `p1` - **RETURN** a resolution error - `inst-mr-return-provider-failed`
8. [x] - `p1` - Run no quota preflight, no downgrade cascade, no tier selection and no kill-switch check for the summary model (system task, ADR-0008) - `inst-mr-no-quota`
9. [x] - `p1` - **RETURN** the entry (`provider_model_id`, `context_window`, `max_output_tokens`, `max_input_tokens`, `estimation_budgets.bytes_per_token_conservative`, `thread_summary_prompt`) and the resolved provider - `inst-mr-return`

### Summary Prompt Construction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-prompt`

**Input**: the range messages, the base summary text (when the base is not empty), the summary model entry and `thread_summary_worker`

**Output**: the system prompt, the user prompt and the kept messages. Rules: DESIGN 3.6 stage 3 (`Request format`, prompt fitting), B.5.5 (prompt texts), B.9.4.

**Steps**:
1. [x] - `p1` - Set the system prompt to the catalog entry's `thread_summary_prompt` when it is not empty, otherwise to `thread_summary_worker.summary_system_prompt` when it is not empty, otherwise to the built-in default of B.5.5 - `inst-pr-system`
2. [x] - `p1` - **IF** the base is empty - `inst-pr-no-summary`
   1. [x] - `p1` - Open the user prompt with `Summarize the following conversation:` - `inst-pr-open-new`
3. [x] - `p1` - **ELSE** - `inst-pr-has-summary`
   1. [x] - `p1` - Open the user prompt with the B.5.5 merge instruction, then the stored summary text inside an `<existing_summary>` block, then `New messages to incorporate:` - `inst-pr-open-merge`
4. [x] - `p1` - **FOR EACH** range message whose `role` is not `system`, in range order - `inst-pr-each`
   1. [x] - `p1` - Append one entry `User: <content>` for a user message or `Assistant: <content>` for an assistant message, entries separated by a blank line - `inst-pr-entry`
   2. [x] - `p1` - **IF** `thread_summary_worker.message_content_limit` (default 4000) is greater than 0 and the content is longer than that many characters, cut it to that many characters on a character boundary and end it with `...` - `inst-pr-truncate`
5. [x] - `p1` - Close the user prompt with the B.5.5 analysis instruction that asks for an `<analysis>` block followed by a `<summary>` block with its five fixed sections - `inst-pr-analysis`
6. [x] - `p1` - **IF** the catalog `context_window` is 0 - `inst-pr-no-fit`
   1. [x] - `p1` - **RETURN** the prompts and all messages without fitting - `inst-pr-return-no-fit`
7. [x] - `p1` - Set the input budget to `context_window` minus `max_output_tokens`, capped by `max_input_tokens` when that is greater than 0 - `inst-pr-budget`
8. [x] - `p1` - Estimate the prompt size as the UTF-8 bytes of the system prompt plus the user prompt (existing summary included) divided by `bytes_per_token_conservative` (at least 1), rounded up - `inst-pr-estimate`
9. [x] - `p1` - **WHILE** the estimate exceeds the budget and more than two messages are kept - `inst-pr-fit-loop`
   1. [x] - `p1` - Drop the oldest ceiling of one fifth of the kept messages, never keeping fewer than two, rebuild the user prompt and estimate again - `inst-pr-fit-drop`
10. [x] - `p1` - Keep the whole frozen range as the range to mark compressed even when fitting dropped its oldest messages; the DESIGN marks exactly the frozen range - `inst-pr-range-unchanged`
11. [x] - `p1` - **RETURN** the prompts and the kept messages - `inst-pr-return`

### Summary Provider Call

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call`

**Input**: the prompts, the kept messages, the summary model entry, the resolved provider, the work item and `thread_summary_worker.claim_timeout_secs`

**Output**: the response text and the provider usage (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens`), or a failure. Rules: DESIGN 3.6 stage 3, 3.2 `llm_provider` (non-streaming thread-summary call), 4 `Provider Request Metadata`, ADR-0005; implementation notes 5 and 11. The per-kind request, the OAGW call and the response extraction are the summary adapter call algorithm below.

**Steps**:
1. [x] - `p1` - Build the non-streaming completion request for the provider `kind` through the summary adapter call algorithm (`cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`): the system prompt as the system instructions, the user prompt as one user message, the output cap equal to the summary model's catalog `max_output_tokens` (not capped by `streaming.max_output_tokens`), no tools, no previous response reference, and `store` false on the Responses adapters - `inst-call-build`
2. [x] - `p1` - Set the composite `user` value to the chat's tenant id and the platform default subject id `11111111-6a88-4768-9dfc-6bcd5187d9ed` in 32-character hex form, tenant first (as `metadata.user_id` on Anthropic); on the OpenAI Responses adapter set `metadata` to the tenant id, that subject id as `user_id`, the chat id, `request_type` `summary` and `feature` `none`; the summary adapter call algorithm places these values per adapter kind - `inst-call-identity`
3. [x] - `p1` - Bound the provider phase of the delivery, prompt-too-long retries included, to `thread_summary_worker.claim_timeout_secs` minus 10 seconds, so a hung call ends as a failure before the lease expires; every attempt gets the time left of this single bound, and an attempt that has no time left fails without a call - `inst-call-deadline`
4. [x] - `p1` - **TRY** - `inst-call-try`
   1. [x] - `p1` - API: send the request and read the complete JSON response through the summary adapter call algorithm (`POST /{upstream_alias}{api_path}` through OAGW under the S2S context, within the time left of the deadline above) - `inst-call-post`
5. [x] - `p1` - **CATCH** a provider rejection that the summary adapter call algorithm classified as a prompt that exceeds the model's context length - `inst-call-catch-ptl`
   1. [x] - `p1` - **IF** fewer than 2 prompt-too-long retries were made in this delivery and more than two messages are kept, drop the oldest ceiling of one fifth of the kept messages (keeping at least two), rebuild the user prompt and **GO TO** step 4 - `inst-call-ptl-retry`
   2. [x] - `p1` - **ELSE** **RETURN** a failure - `inst-call-ptl-exhausted`
6. [x] - `p1` - **CATCH** any other failure reported by the summary adapter call algorithm: a non-2xx upstream answer, a gateway error, the deadline, a transport error or an unparseable or malformed body - `inst-call-catch-other`
   1. [x] - `p1` - Keep only the sanitized cause that the adapter call algorithm returned for the log and **RETURN** a failure - `inst-call-return-failure`
7. [x] - `p1` - **RETURN** the response text and the usage that the summary adapter call algorithm extracted from the successful call; earlier rejected prompt-too-long attempts contribute no usage - `inst-call-return`

### Summary Adapter Call

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`

**Input**: the system prompt, the user prompt, the summary model entry (`provider_model_id`, `max_output_tokens`), the resolved provider (`kind`, `upstream_alias`, `api_path`), the chat's tenant id and chat id, and the time left of the provider phase of the delivery

**Output**: the response text and the usage (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens`), or a failure of the class `prompt too long` or `other` with a sanitized cause. Rules: DESIGN 3.2 `llm_provider` (the four adapter kinds and the non-streaming thread-summary call), 3.5 `LLM Provider` (route, `{alias}{api_path}`, Azure `api-version` in `api_path`), 4 `Provider Request Metadata`, `Detailed token breakdown` of the usage rules, B.5.5, ADR-0005. The field names of the request follow the streaming adapters of `cpt-cf-mini-chat-algo-message-streaming-adapter-kinds` and `cpt-cf-mini-chat-algo-message-streaming-responses-request`, the usage names follow `cpt-cf-mini-chat-algo-message-streaming-event-translation`; this algorithm specifies the non-streaming call, which those algorithms do not.

**Steps**:
1. [x] - `p1` - **MATCH** the provider `kind` to build the JSON request body - `inst-ad-match`
   - [x] - `p1` - **CASE** `openai_responses` (OpenAI and the Azure OpenAI variant; Azure differs only by the `api-version` query that `api_path` already carries): `model` equal to `provider_model_id`, `instructions` equal to the system prompt, `input` an array of one item with `role` `user` and `content` the user prompt as a plain string, `max_output_tokens` equal to the catalog value, `stream` `false`, `store` `false`, `user` equal to the composite value, `metadata` with `tenant_id`, `user_id` (the system subject id, hyphenated), `chat_id`, `request_type` `summary` and `feature` `none`; no `tools`, no `include`, no `max_tool_calls` and no `previous_response_id` - `inst-ad-openai-responses`
   - [x] - `p1` - **CASE** `openai_chat_completions`: `model`, `messages` holding one `system` message with the system prompt and then one `user` message with the user prompt as a plain string, `max_completion_tokens` equal to the catalog `max_output_tokens`, `stream` `false` and no `stream_options`, `user` equal to the composite value; no `metadata` and no `tools` - `inst-ad-chat-completions`
   - [x] - `p1` - **CASE** `vllm_responses`: the body of `openai_responses` without `metadata` (vLLM omits it), still with `user`, `store` `false` and no tools of any kind - `inst-ad-vllm-responses`
   - [x] - `p1` - **CASE** `anthropic_messages`: `model`, `system` equal to the system prompt, `messages` holding one `user` message with the user prompt as a plain string, `max_tokens` equal to the catalog `max_output_tokens`, `stream` `false`, `metadata.user_id` equal to the composite value; no `tools` - `inst-ad-anthropic`
   - [x] - `p1` - **DEFAULT**: unreachable, because configuration validation of `cpt-cf-mini-chat-feature-gear-foundation` rejects any other `kind` at startup - `inst-ad-default`
2. [x] - `p1` - Add none of the catalog `general_config.api_params` (no sampling parameter, no `reasoning` effort) and no `extra_body` key to the summary request, and never place a credential or any provider identifier in the body - `inst-ad-no-extras`
3. [x] - `p1` - Build the request URI as `/{upstream_alias}{api_path}` with only `{model}` replaced by `provider_model_id`; `upstream_alias` is the alias of the provider registry for the chat's tenant (it may be `host:port`) and the query of `api_path` (the Azure `api-version`) is kept verbatim - `inst-ad-uri`
4. [x] - `p1` - **IF** no time is left of the provider phase - `inst-ad-no-time`
   1. [x] - `p1` - **RETURN** a failure of the class `other` with the cause `the provider phase deadline was reached`, without a call - `inst-ad-return-no-time`
5. [x] - `p1` - API: `POST` the body through the in-process OAGW client `ServiceGatewayClientV1` `proxy_request` under the gear's S2S security context, with `Content-Type: application/json`, `Accept: application/json` and no credential header, waiting for the complete JSON response and ending the call when the time left is used up; the call is made without retry by this algorithm - `inst-ad-post`
6. [x] - `p1` - **IF** the transport failed, the gateway answered with its own error (a 502, 503 or 504 Problem included) or the deadline was reached - `inst-ad-transport-failed`
   1. [x] - `p1` - **RETURN** a failure of the class `other` with a fixed cause naming only the failure kind - `inst-ad-return-transport`
7. [x] - `p1` - **IF** the status is not 2xx - `inst-ad-non-2xx`
   1. [x] - `p1` - Read at most 16 KiB of the error body and keep it only for classification and the sanitized log cause - `inst-ad-read-error`
   2. [x] - `p1` - **IF** the status is 400 or 413 and the error body has `error.code` equal to `context_length_exceeded`, or has a message (read from `error.message`, or from a top-level `message` when `error` is absent or has no message) that contains `maximum context length`, `prompt is too long` or `context_length_exceeded` (compared case-insensitively); the `error.type` value is not consulted, because `vllm_responses` returns the type `BadRequestError` and older releases put the message at the top level - `inst-ad-classify-ptl`
      1. [x] - `p1` - **RETURN** a failure of the class `prompt too long` - `inst-ad-return-ptl`
   3. [x] - `p1` - **RETURN** a failure of the class `other` with the status code and a cause sanitized by `cpt-cf-mini-chat-algo-message-streaming-error-sanitization`, holding no provider identifier, URL, credential or body text; a 429 is a failure like any other status here and is retried only through the summary retry budget - `inst-ad-return-other`
8. [x] - `p1` - Read at most 1 MiB of the success body and parse it as a JSON object - `inst-ad-parse`
9. [x] - `p1` - **IF** the body is larger than that, is not valid JSON or is not an object - `inst-ad-malformed`
   1. [x] - `p1` - **RETURN** a failure of the class `other` with the cause `the provider response is malformed` - `inst-ad-return-malformed`
10. [x] - `p1` - **MATCH** the provider `kind` to extract the response text and the usage - `inst-ad-extract-match`
    - [x] - `p1` - **CASE** `openai_responses` or `vllm_responses`: fail as malformed when `output` is not an array; fail with the class `other` when `status` is `failed` or `error` is present and not null; otherwise join, in order, the `text` of every `output_text` part in the `content` of every `output` item of `type` `message`, ignoring `reasoning` and every other item; a `status` of `incomplete` is a truncated but valid completion and its text is used as it is; for `vllm_responses` remove every `<think>...</think>` block from the joined text, as the streaming adapter treats that text as reasoning - `inst-ad-text-responses`
    - [x] - `p1` - **CASE** `openai_chat_completions`: fail as malformed when `choices` is not a non-empty array; take `choices[0].message.content` as the text when it is a string and an empty text when it is null or absent; a `finish_reason` of `length` is a truncated but valid completion and its text is used as it is - `inst-ad-text-chat-completions`
    - [x] - `p1` - **CASE** `anthropic_messages`: fail as malformed when the body `type` is `error` or `content` is not an array; join, in order, the `text` of every content block of `type` `text`, ignoring `thinking`, `tool_use` and every other block; a `stop_reason` of `max_tokens` is a truncated but valid completion and its text is used as it is - `inst-ad-text-anthropic`
11. [x] - `p1` - **MATCH** the provider `kind` to read the usage, taking every count that is absent as 0 and treating an absent usage object as all counts 0 (the `token_estimate` rule of the summary response parsing algorithm then applies the byte-length fallback) - `inst-ad-usage-match`
    - [x] - `p1` - **CASE** `openai_responses` or `vllm_responses`: `usage.input_tokens`, `usage.output_tokens`, `usage.input_tokens_details.cached_tokens` as `cache_read_input_tokens`, `usage.output_tokens_details.reasoning_tokens` as `reasoning_tokens`, and 0 for `cache_write_input_tokens` - `inst-ad-usage-responses`
    - [x] - `p1` - **CASE** `openai_chat_completions`: `usage.prompt_tokens` as `input_tokens`, `usage.completion_tokens` as `output_tokens`, `usage.prompt_tokens_details.cached_tokens` as `cache_read_input_tokens`, `usage.completion_tokens_details.reasoning_tokens` as `reasoning_tokens`, and 0 for `cache_write_input_tokens` - `inst-ad-usage-chat-completions`
    - [x] - `p1` - **CASE** `anthropic_messages`: `usage.input_tokens`, `usage.output_tokens`, `usage.cache_read_input_tokens` and `usage.cache_creation_input_tokens` as `cache_write_input_tokens`, and 0 for `reasoning_tokens` - `inst-ad-usage-anthropic`
12. [x] - `p1` - Return an extracted text that is empty or only whitespace as a success with that empty text and the usage; it is not a call failure, and the summary response parsing algorithm turns it into the `empty_summary` outcome - `inst-ad-empty-text`
13. [x] - `p1` - Never log the request body, the prompt, the response text or the error body; log only the chat id, the provider kind, the status code and the failure class - `inst-ad-no-content-logs`
14. [x] - `p1` - **RETURN** the response text and the usage - `inst-ad-return`

### Summary Response Parsing

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing`

**Input**: the response text and the usage of the successful call

**Output**: the summary text to store and its `token_estimate`, or empty. Rules: DESIGN 3.6 stage 3 (`Response parsing`, stored `token_estimate`), 3.7 `Table: thread_summaries`.

**Steps**:
1. [x] - `p1` - Remove every `<analysis>...</analysis>` block from the text - `inst-rp-strip-analysis`
2. [x] - `p1` - **IF** a `<summary>...</summary>` block remains - `inst-rp-summary-block`
   1. [x] - `p1` - Take the text inside it - `inst-rp-take-summary`
3. [x] - `p1` - **ELSE IF** the remaining text still contains `<analysis` or `<summary` markup - `inst-rp-leftover-markup`
   1. [x] - `p1` - Take an empty result - `inst-rp-empty-markup`
4. [x] - `p1` - **ELSE** take the whole remaining text - `inst-rp-take-rest`
5. [x] - `p1` - Collapse runs of blank lines into one blank line and trim the result - `inst-rp-collapse`
6. [x] - `p1` - **IF** the result is empty - `inst-rp-empty`
   1. [x] - `p1` - **RETURN** empty - `inst-rp-return-empty`
7. [x] - `p1` - Set `token_estimate` to `output_tokens` minus `reasoning_tokens` when that difference is positive, otherwise to the UTF-8 byte length of the summary divided by 4, rounded up - `inst-rp-token-estimate`
8. [x] - `p1` - Apply no length or entropy validation (the quality gate is P2+) - `inst-rp-no-quality-gate`
9. [x] - `p1` - **RETURN** the summary text and `token_estimate` - `inst-rp-return`

### Summary CAS Commit

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-summary-commit`

**Input**: the work item, the summary text, its `token_estimate`, the usage of the call and the summary model id

**Output**: `committed`, `target deleted`, `base missing`, `frontier moved`, or a database or enqueue error. Rules: DESIGN 3.6 stage 4 (`CAS-protected commit`), `Frozen-range commit invariant`, 3.9 `Summary Interaction on Turn Mutation`, 3.7 `Table: thread_summaries` (initial state, committed state only).

**Steps**:
1. [x] - `p1` - Open one database transaction; on SQLite take the database write lock before the first read so the checks and the writes below are atomic against a mutation transaction - `inst-cm-begin`
2. [x] - `p1` - DB: SELECT the target frontier message by `tenant_id`, `chat_id` and `frozen_target_message_id`, with `FOR UPDATE` on PostgreSQL, so the row lock orders this commit and a retry, edit or delete that soft-deletes the message: whichever commits second sees the other's result - `inst-cm-lock-target`
3. [x] - `p1` - **IF** the target message is missing or soft-deleted - `inst-cm-target-deleted`
   1. [x] - `p1` - Roll back and **RETURN** `target deleted`; no summary, no `is_compressed` change and no usage event exist - `inst-cm-return-target-deleted`
4. [x] - `p1` - **IF** the base is empty - `inst-cm-first`
   1. [x] - `p1` - DB: INSERT a `thread_summaries` row with a new `id`, `tenant_id`, `chat_id`, `summary_text`, the frontier set to the frozen target, `token_estimate`, `created_at` and `updated_at` now, doing nothing on a conflict of the unique `chat_id` - `inst-cm-insert`
   2. [x] - `p1` - **IF** no row was inserted (another first summary committed), roll back and **RETURN** `frontier moved` - `inst-cm-insert-conflict`
5. [x] - `p1` - **ELSE** - `inst-cm-advance`
   1. [x] - `p1` - DB: UPDATE the chat's `thread_summaries` row setting `summary_text`, the frontier to the frozen target, `token_estimate` and `updated_at` now, where `tenant_id` and `chat_id` match and the stored frontier equals the base frontier (sort key on SQLite) - `inst-cm-update`
   2. [x] - `p1` - **IF** the update affected no row, roll back and **RETURN** `base missing` when the chat has no `thread_summaries` row, otherwise `frontier moved` - `inst-cm-update-conflict`
6. [x] - `p1` - DB: UPDATE `is_compressed` to true on the `messages` rows of the chat (filter `tenant_id` and `chat_id`) with `deleted_at IS NULL` and `is_compressed` false that are after the base frontier (when not empty) and at or before the frozen target, with the predicate and sort key of the summary range selection algorithm - `inst-cm-compress`
7. [x] - `p1` - Enqueue the system-task usage event through the system task usage algorithm inside this transaction - `inst-cm-usage`
8. [x] - `p1` - **TRY** - `inst-cm-try`
   1. [x] - `p1` - Commit the transaction; the summary, the frontier, the compressed range and the usage event commit together or not at all - `inst-cm-commit`
9. [x] - `p1` - **CATCH** a database failure or an enqueue error - `inst-cm-catch`
   1. [x] - `p1` - Roll back and **RETURN** the error; nothing of this attempt remains - `inst-cm-return-error`
10. [x] - `p1` - Make no second commit attempt for this frozen range after any non-committed result; at most one successful commit advances the frontier for a frozen range `(chat_id, base frontier, frozen target)` - `inst-cm-single-commit`
11. [x] - `p1` - **RETURN** `committed` with the usage outbox wake to fire after the commit - `inst-cm-return`

### System Task Usage and Isolation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-system-task-usage`

**Input**: the open commit transaction, the work item, the summary model id and the usage of the call

**Output**: one usage message on `outbox.queue_name` (default `mini-chat.usage_snapshot`) in the caller's transaction. Rules: DESIGN 3.2 `System Task Isolation Invariant (P1)`, `System Task Attribution Rules (Normative)`; the payload contract is the system-task branch `inst-event-system-task` of `cpt-cf-mini-chat-algo-quota-and-billing-usage-event`.

**Steps**:
1. [x] - `p1` - Build the `UsageEvent` with `tenant_id` and `chat_id` of the work item, `request_id` equal to the payload's `system_request_id`, `effective_model` and `selected_model` equal to the summary model id, `terminal_state` `completed`, `billing_outcome` `system_task`, `usage` with the token counts of the successful call, `actual_credits_micro` 0, `settlement_method` `none`, `policy_version_applied` 0, the three tool call counts 0, `timestamp` now in UTC, `requester_type` `system` and `system_task_type` `thread_summary_update`, and with `user_id` and `turn_id` absent (not null) - `inst-su-build`
2. [x] - `p1` - Set `dedupe_key` to `{tenant_id}/thread_summary_update/{system_request_id}` with both UUIDs in 32-character lowercase hex form - `inst-su-dedupe`
3. [x] - `p1` - Reuse the `system_request_id` from the payload unchanged on every retry and replay; never generate a new one in the handler - `inst-su-stable-id`
4. [x] - `p1` - Enqueue through the foundation's transactional enqueuer on the usage queue, partitioned by `tenant_id`, with the dedupe prefix validation - `inst-su-enqueue`
5. [x] - `p1` - Never insert or update a `chat_turns` row, never use `chat_turns.state` or the finalization CAS, never run a quota preflight, reserve or settlement and never change a `quota_usage` row - `inst-su-no-turn`
6. [x] - `p1` - Emit no audit event, check no kill switch and debit no tenant operational bucket (ADR-0008); put no provider identifier in the payload - `inst-su-no-audit`
7. [x] - `p1` - **RETURN** the enqueue wake - `inst-su-return`

### Chat Cleanup Payload Parsing

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload`

**Input**: the payload bytes of one chat-cleanup message

**Output**: `tenant_id`, `chat_id`, `system_request_id` and `chat_deleted_at`, or a malformed reason. Rule: DESIGN 3.6 `Outbox payload and execution semantics`.

**Steps**:
1. [x] - `p1` - Parse the bytes as a JSON object - `inst-cp-parse`
2. [x] - `p1` - Read `tenant_id`, `chat_id` and `system_request_id` as UUIDs and `chat_deleted_at` as RFC 3339 - `inst-cp-fields`
3. [x] - `p1` - Require `reason` equal to `chat_soft_delete` - `inst-cp-reason`
4. [x] - `p1` - **IF** a `dedupe_key` is present, require the 32-character lowercase hex `tenant_id` prefix followed by `/` - `inst-cp-dedupe`
5. [x] - `p1` - **IF** any check failed - `inst-cp-malformed`
   1. [x] - `p1` - **RETURN** the malformed reason naming the field - `inst-cp-return-malformed`
6. [x] - `p1` - **RETURN** the parsed message; the handler uses only `tenant_id` and `chat_id` to load the current rows, never a row list from the payload - `inst-cp-return`

### Chat Cleanup Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler`

**Input**: one leased chat-cleanup message and its `attempts`

**Output**: `Ok`, `Retry` or `Reject(reason)` and the updated cleanup state of the chat's attachment rows and `chat_vector_stores` row. Rules: DESIGN 3.6 `Cleanup on Chat Deletion` (all sub-headings), 4 `Cleanup on Chat Deletion`, B.9.2, B.9.3 (lease).

**Steps**:
1. [x] - `p1` - Start the delivery timer on the injectable monotonic timer at handler entry; the delivery time budget is 20 s and every provider call of this delivery has a deadline of 5 s (both fixed, below the shared 30 s lease); the timer is checked immediately before every provider-bound call of the delivery (the policy snapshot read, the primary file delete, the secondary file delete and the vector store delete), so no call starts after 20 s and the latest call, started at 20 s with a 5 s deadline, ends by 25 s, 5 s inside the lease, plus the DB writes that follow it - `inst-cc-timer`
2. [x] - `p1` - Parse the payload through the chat cleanup payload parsing algorithm - `inst-cc-parse`
3. [x] - `p1` - **IF** the payload is malformed - `inst-cc-malformed`
   1. [x] - `p1` - Log an error with the reason and **RETURN** `Reject` at once (dead letter) - `inst-cc-return-malformed`
4. [x] - `p1` - DB: SELECT the `chats` row by `tenant_id` and `chat_id` without the deleted filter, including its `model` (the model chosen at creation, locked for the life of the chat) - `inst-cc-load-chat`
5. [x] - `p1` - **IF** the query failed - `inst-cc-chat-db`
   1. [x] - `p1` - **RETURN** `Retry` (infrastructure failure, no limit) - `inst-cc-return-chat-db`
6. [x] - `p1` - **IF** the chat is missing or not soft-deleted (`deleted_at IS NULL`) - `inst-cc-active`
   1. [x] - `p1` - **RETURN** `Reject` with the reason `chat is not soft-deleted` (or `chat not found`); an active chat is never processed by this handler - `inst-cc-return-active`
7. [x] - `p1` - DB: SELECT the `attachments` rows of the chat (filter `tenant_id` and `chat_id`, soft-deleted attachments included) whose `cleanup_status` is `pending`, ordered by `created_at` then `id` - `inst-cc-load-pending`
8. [x] - `p1` - **IF** the query failed - `inst-cc-pending-db`
   1. [x] - `p1` - **RETURN** `Retry` - `inst-cc-return-pending-db`
9. [x] - `p1` - **FOR EACH** pending row - `inst-cc-each`
   1. [x] - `p1` - **IF** the delivery timer shows that the 20 s time budget has elapsed, stop the loop before this row: every row not yet attempted stays `pending` with `cleanup_attempts` unchanged, and the handler logs at info level the chat id and the number of rows left, without any file identifier - `inst-cc-budget-stop`
   2. [x] - `p1` - Run the chat attachment file cleanup algorithm on the row with the chat's `tenant_id`, `chat_id`, `model` and the delivery timer - `inst-cc-file`
   3. [x] - `p1` - **IF** it reported an infrastructure failure, **RETURN** `Retry` at once without incrementing any `cleanup_attempts`; rows already finished in this delivery stay finished - `inst-cc-file-db`
   4. [x] - `p1` - **IF** it reported `budget exhausted` (the budget ended before the next provider-bound call of the row could start: the primary delete, the policy snapshot read or the secondary delete), stop the loop; the row stays `pending` with `cleanup_attempts` unchanged and the next delivery deletes the primary file again (404 counts as success) before it reaches the secondary delete - `inst-cc-file-budget`
10. [x] - `p1` - DB: count the attachment rows of the chat still in `cleanup_status` `pending` - `inst-cc-count-pending`
11. [x] - `p1` - **IF** the count failed or at least one row is still `pending` - `inst-cc-still-pending`
    1. [x] - `p1` - **RETURN** `Retry`; the vector store is not touched while any row is `pending`, and every such row is bounded by its own `cleanup_attempts` budget - `inst-cc-return-still-pending`
12. [x] - `p1` - Account for a `Retry` that was caused by the time budget as follows: it counts as one delivery of the message in the delivery count (`attempts`) that the vector store reject rule uses, because the shared outbox counts every redelivery and the handler cannot prevent it; it never counts as a row attempt, so no `cleanup_attempts` value changes for a row that was not attempted; and it never starts the vector store provider call, so the ordering invariant (store deletion only after no attachment row is `pending`) holds on every delivery - `inst-cc-budget-accounting`
13. [x] - `p1` - Run the vector store cleanup algorithm with the delivery's `attempts` and the delivery timer - `inst-cc-vector-store`
14. [x] - `p1` - Keep every step idempotent: a redelivery, a replay or a concurrent delivery after a lease expiry re-reads the persisted state, skips `done` and `failed` rows and a missing `chat_vector_stores` row, and treats a provider 404 as success - `inst-cc-idempotent`
15. [x] - `p1` - Keep the delivery bounded: every row is attempted at most once per delivery, the first row of every delivery is always started (the timer shows no elapsed time yet) and runs to a recorded attempt or to `done`, so each budget-driven `Retry` makes progress and the per-row `cleanup_worker.max_attempts` budget ends the row loop - `inst-cc-bounded`
16. [x] - `p1` - **RETURN** the outcome of the vector store cleanup algorithm - `inst-cc-return`

### Chat Attachment File Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file`

**Input**: one `pending` attachment row of a soft-deleted chat (`id`, `provider_file_id`, `storage_backend`, `cleanup_attempts`, the secondary columns), the chat's `tenant_id`, `chat_id` and `model`, the delivery timer and `cleanup_worker.max_attempts` (default 5)

**Output**: `done`, `failed`, `still pending`, `skipped`, `budget exhausted` (before the primary delete, or from the secondary cleanup) or an infrastructure failure. Rules: DESIGN 3.6 `Attachment cleanup state machine`, `Additional invariants` 1; the transitions are those of `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status`.

**Steps**:
1. [x] - `p1` - **IF** the row has no `provider_file_id` (the upload never reached the provider) - `inst-af-no-file`
   1. [x] - `p1` - DB: UPDATE `cleanup_status` to `done` and `cleanup_updated_at` to now, guarded by `tenant_id`, `id` and `cleanup_status` `pending` (transition `inst-cs-done`), record `mini_chat_cleanup_completed{resource_type="file"}` and **RETURN** `done`; a file the provider stored for an upload still in flight is the provider-side orphan of DESIGN invariant 4 (P2 reconciliation) - `inst-af-return-no-file`
2. [x] - `p1` - Resolve the storage provider for the chat's tenant from the row's `storage_backend` through the storage dispatcher of `cpt-cf-mini-chat-feature-attachments-and-file-search`; an unresolvable label counts as a failed attempt with the cause `the storage backend maps to no provider` - `inst-af-target`
3. [x] - `p1` - **IF** the delivery timer shows that the 20 s time budget has elapsed - `inst-af-budget`
   1. [x] - `p1` - **RETURN** `budget exhausted` without a provider call and without any write; the row stays `pending` and no attempt is counted - `inst-af-return-budget`
4. [x] - `p1` - API: `DELETE /{alias}{prefix}/files/{provider_file_id}` through OAGW under the S2S context (prefix `/v1` for `storage_kind` `openai`, `/openai` with the `api-version` query parameter for `azure`), bounded by the fixed provider call deadline of 5 s; a call that hits the deadline is a failed call and is handled by `inst-af-failed` with the cause `the provider call exceeded its deadline` - `inst-af-delete`
5. [x] - `p1` - **IF** the delete returned 2xx or 404 - `inst-af-success`
   1. [x] - `p1` - Run the chat secondary file cleanup algorithm on the row; its outcome never blocks the next step, except that its `budget exhausted` result makes this algorithm **RETURN** `budget exhausted` at once without any write, so the row stays `pending` for the next delivery - `inst-af-secondary`
   2. [x] - `p1` - DB: UPDATE `cleanup_status` to `done` and `cleanup_updated_at` to now, guarded by `tenant_id`, `id` and `cleanup_status` `pending` (transition `inst-cs-done`), record `mini_chat_cleanup_completed{resource_type="file"}` and **RETURN** `done` - `inst-af-mark-done`
6. [x] - `p1` - **ELSE** (any other status, a gateway error, a hit deadline or a transport failure) - `inst-af-failed`
   1. [x] - `p1` - DB: UPDATE `cleanup_attempts` to its value plus 1, `last_cleanup_error` to a fixed cause text without provider body or identifiers and `cleanup_updated_at` to now, guarded by `cleanup_status` `pending` (transition `inst-cs-retry`) - `inst-af-record-attempt`
   2. [x] - `p1` - **IF** `cleanup_attempts` reached `cleanup_worker.max_attempts`, set `cleanup_status` to `failed` in the same guarded write (transition `inst-cs-failed`), record `mini_chat_cleanup_failed{resource_type="file"}` and **RETURN** `failed`; the handler continues with the other rows and the row is terminal provider-file debt - `inst-af-exhausted`
   3. [x] - `p1` - **ELSE** record `mini_chat_cleanup_retry{resource_type="file", reason="provider_error"}` and **RETURN** `still pending` - `inst-af-return-pending`
7. [x] - `p1` - **IF** a guarded write matched no row (the attachment cleanup handler finished the row first, which is harmless because both paths treat 404 as success) - `inst-af-guard-lost`
   1. [x] - `p1` - **RETURN** `skipped` - `inst-af-return-skipped`
8. [x] - `p1` - **IF** a database write failed - `inst-af-db`
   1. [x] - `p1` - **RETURN** an infrastructure failure; `cleanup_attempts` is not incremented and the next delivery repeats the idempotent delete - `inst-af-return-db`
9. [x] - `p1` - Never process a row whose `cleanup_status` is `done` or `failed`: `failed` rows of a deleted chat are not re-attempted by any automatic path or replay in P1 - `inst-af-no-terminal`
10. [x] - `p1` - **RETURN** the row outcome - `inst-af-return`

### Chat Secondary File Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file`

**Input**: an attachment row whose primary provider file is gone, the chat's `tenant_id`, `chat_id` and `model` (read with the chat row by the handler), the delivery timer and the per-delivery memo of the secondary target

**Output**: none, or `budget exhausted` (the timer is checked before the policy snapshot read and again before the secondary delete, so neither call starts after 20 s); the secondary delete is best effort. Rules: DESIGN 4 `Cleanup on Chat Deletion` (P1 behavior: the secondary Anthropic file is deleted when the upstream alias could be resolved at delete time), B.9.2, 3.6 observability (`mini_chat_secondary_cleanup_skipped`). The alias is resolved at handling time because the chat-cleanup payload carries no `secondary_ref` (the attachment cleanup handler of `cpt-cf-mini-chat-feature-attachments-and-file-search` uses the alias frozen at enqueue); the resolution has the semantics of the secondary-copy resolution of that feature (`cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy`: the Anthropic provider of the chat model's catalog entry, resolved for the chat's tenant through the provider registry).

**Steps**:
1. [x] - `p1` - **IF** `secondary_status` is not `uploaded` or `secondary_file_id` is empty - `inst-sf-none`
   1. [x] - `p1` - **RETURN** - `inst-sf-return-none`
2. [x] - `p1` - **IF** the delivery timer shows that the 20 s time budget has elapsed - `inst-sf-budget`
   1. [x] - `p1` - **RETURN** `budget exhausted`; no provider call starts after the budget - `inst-sf-return-budget`
3. [x] - `p1` - **IF** the secondary target of this delivery is not yet resolved, resolve it once and keep the result (a target or none) in the per-delivery memo, so the policy snapshot is read at most once per delivery - `inst-sf-resolve`
   1. [x] - `p1` - Read the current policy snapshot without a user id through `cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access`, bounded by the 5 s deadline - `inst-sf-snapshot`
   2. [x] - `p1` - Look up the chat's `model` in the snapshot's model catalog without the enabled filter (a disabled model still names its provider) and take its `provider_id` - `inst-sf-model`
   3. [x] - `p1` - Resolve that `provider_id` for the chat's `tenant_id` from the payload (tenant overrides applied) through `cpt-cf-mini-chat-algo-gear-foundation-provider-registry`; accept the provider only when its `kind` is `anthropic_messages` and take its `upstream_alias` as the target alias - `inst-sf-provider`
   4. [x] - `p1` - **IF** the snapshot could not be read, the model is no longer in the catalog, the provider cannot be resolved or its `kind` is not `anthropic_messages`, fall back to the provider entries of the registry: resolve every entry for the chat's `tenant_id` and, when exactly one of them has `kind` `anthropic_messages`, take its `upstream_alias`; when none or more than one has that kind, take no target (a guess between several Anthropic accounts is never made) - `inst-sf-fallback`
4. [x] - `p1` - **IF** no Anthropic Files client exists, or the resolution above produced no alias - `inst-sf-skip`
   1. [x] - `p1` - Record `mini_chat_secondary_cleanup_skipped{provider_kind}` with the row's `secondary_provider_kind`, log a warning with the chat id and the attachment id (no file id, no content) and **RETURN**; the primary row is never marked `failed` because of the secondary - `inst-sf-return-skip`
5. [x] - `p1` - **IF** the delivery timer shows that the 20 s time budget has elapsed (the policy snapshot read above may have used up to its 5 s deadline) - `inst-sf-budget-delete`
   1. [x] - `p1` - **RETURN** `budget exhausted`; the memo keeps the resolved target, no secondary delete starts after the budget, the row stays `pending` and no attempt is counted - `inst-sf-return-budget-delete`
6. [x] - `p1` - API: delete the secondary file through the Anthropic Files client on the resolved alias, bounded by the fixed provider call deadline of 5 s; a 404 is success, and any other failure (a hit deadline included) is logged without the file id and not retried - `inst-sf-delete`
7. [x] - `p1` - Never change the primary row, its `cleanup_attempts` or its `cleanup_status` because of the secondary outcome - `inst-sf-no-block`
8. [x] - `p1` - **RETURN** - `inst-sf-return`

### Vector Store Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup`

**Input**: `tenant_id`, `chat_id`, the delivery's `attempts`, the delivery timer and `cleanup_worker.max_attempts` (default 5)

**Output**: `Ok`, `Retry` or `Reject(reason)`. Rules: DESIGN 3.6 `Vector store cleanup ordering`, `Additional invariants` 3, 3.7 `Table: chat_vector_stores` (cleanup invariant, deletion race); this algorithm realizes `inst-vsm-chat-cleanup` of `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`.

**Steps**:
1. [x] - `p1` - Run only when no attachment row of the chat is `pending` (checked by the handler); every row is `done` or `failed` - `inst-vs-eligible`
2. [x] - `p1` - DB: SELECT the `chat_vector_stores` row by `tenant_id` and `chat_id` - `inst-vs-load`
3. [x] - `p1` - **IF** the query failed - `inst-vs-load-db`
   1. [x] - `p1` - **RETURN** `Retry` (no limit) - `inst-vs-return-load-db`
4. [x] - `p1` - **IF** no row exists (the chat never had a document, or a previous delivery finished) - `inst-vs-none`
   1. [x] - `p1` - **RETURN** `Ok`; the chat is provider-purged - `inst-vs-return-none`
5. [x] - `p1` - **IF** the row's `vector_store_id` is NULL (a creation placeholder) - `inst-vs-placeholder`
   1. [x] - `p1` - DB: DELETE the row by `tenant_id`, `chat_id` and `id`, guarded by `vector_store_id IS NULL`, make no provider call and **RETURN** `Ok` (`Retry` on a database error); an upload that creates the store afterwards loses its compare-and-set and deletes the new store itself (creation protocol of `cpt-cf-mini-chat-feature-attachments-and-file-search`) - `inst-vs-return-placeholder`
6. [x] - `p1` - **IF** the delivery timer shows that the 20 s time budget has elapsed - `inst-vs-budget`
   1. [x] - `p1` - **RETURN** `Retry` without a provider call, without the debt metric and without recording a store attempt; every attachment row is already terminal, so the next delivery goes straight to the store delete, and this delivery counts in the delivery count like any `Retry` but never produces a `Reject` because no delete failed - `inst-vs-return-budget`
7. [x] - `p1` - **IF** any attachment row of the chat has `cleanup_status` `failed` - `inst-vs-failed-rows`
   1. [x] - `p1` - Record `mini_chat_cleanup_vector_store_with_failed_attachments` before the delete; the store is released although provider-file debt remains - `inst-vs-record-debt`
8. [x] - `p1` - Resolve the storage provider from the row's `provider` label through the storage dispatcher; an unresolvable label counts as a failed delete - `inst-vs-target`
9. [x] - `p1` - API: `DELETE /{alias}{prefix}/vector_stores/{vector_store_id}` through OAGW under the S2S context, one call for the whole store, bounded by the fixed provider call deadline of 5 s; a call that hits the deadline is a failed delete and is handled by `inst-vs-failed` - `inst-vs-delete`
10. [x] - `p1` - **IF** the delete returned 2xx or 404 - `inst-vs-success`
    1. [x] - `p1` - DB: DELETE the `chat_vector_stores` row by `tenant_id`, `chat_id` and `id`; the deletion is the durable completion marker - `inst-vs-delete-row`
    2. [x] - `p1` - **IF** the row delete failed, **RETURN** `Retry`; the next delivery deletes the store again and gets 404 - `inst-vs-delete-row-db`
    3. [x] - `p1` - Record `mini_chat_cleanup_completed{resource_type="vector_store"}` and **RETURN** `Ok` - `inst-vs-return-ok`
11. [x] - `p1` - **ELSE** (any other status, a gateway error, a hit deadline or a transport failure) - `inst-vs-failed`
    1. [x] - `p1` - Keep the `chat_vector_stores` row unchanged and record `mini_chat_cleanup_retry{resource_type="vector_store", reason="vector_store_delete_failed"}` - `inst-vs-keep-row`
    2. [x] - `p1` - **IF** this delivery is the `cleanup_worker.max_attempts`-th or a later one (`attempts` at least `max_attempts` minus 1), counting every delivery of the message including those that returned `Retry` while rows were `pending` or because the time budget ended, record `mini_chat_cleanup_failed{resource_type="vector_store"}` and **RETURN** `Reject` with the reason `vector store delete: max attempts (N) reached`; a replay of the dead letter retries the delete - `inst-vs-return-reject`
    3. [x] - `p1` - **ELSE** **RETURN** `Retry` - `inst-vs-return-retry`
12. [x] - `p1` - Never rely on the store delete to remove provider files: deleting a vector store only removes its index references - `inst-vs-files-separate`

### Cleanup Responsibility Boundaries

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries`

**Input**: an attachment whose provider resources need cleanup and the state of its parent chat

**Output**: the single path that owns its provider cleanup. Rules: DESIGN 3.6 `Cleanup responsibility boundaries`, `Vector store cleanup ordering` (exclusive ownership).

**Steps**:
1. [x] - `p1` - **IF** the parent chat is active - `inst-cb-active`
   1. [x] - `p1` - Leave the attachment to the attachment-delete path (`cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler`); a failed attachment delete of an active chat is never picked up by chat cleanup, which rejects messages of active chats - `inst-cb-attachment-path`
2. [x] - `p1` - **ELSE** - `inst-cb-deleted`
   1. [x] - `p1` - Transfer ownership to chat cleanup at the commit of the chat soft-delete: from then on the chat-cleanup handler exclusively owns the provider files of every `pending` row of the chat and the chat's vector store, and the attachment cleanup handler acknowledges such messages without action (`inst-ch-return-chat-owned`) - `inst-cb-transfer`
3. [x] - `p1` - Accept that the two paths are not locked against each other: when the attachment handler checked the chat before the soft-delete, both may delete the same file; 404 counts as success on both paths and the terminal write is guarded by `cleanup_status` `pending`, so exactly one path writes it - `inst-cb-no-lock`
4. [x] - `p1` - Keep provider cleanup of a deleted chat on the outbox-driven path only: no request path, upload reaper or background indexing task deletes a deleted chat's provider resources (both skip rows whose `cleanup_status` is set) - `inst-cb-outbox-only`
5. [x] - `p1` - **RETURN** the owning path - `inst-cb-return`

### Retention Behavior

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-retention`

**Input**: a soft-deleted chat and its rows

**Output**: the P1 retention outcome. Rules: DESIGN 4 `Data Classification and Retention (P1)`, `Cleanup on Chat Deletion` (P1 behavior), B.11; PRD `cpt-cf-mini-chat-nfr-data-retention`; ADR-0009.

**Steps**:
1. [x] - `p1` - Keep the chat row and every child row (messages, turns, reactions, attachments with their thumbnails, the thread summary) in the database indefinitely; neither handler hard-deletes or soft-deletes them, and the only row the cleanup deletes is `chat_vector_stores` as its completion marker - `inst-rt-no-purge`
2. [x] - `p1` - Remove provider files and the vector store on a best-effort basis with the operational target of one hour under normal conditions and eventual completion with retry and backoff on provider errors; this is not a guaranteed SLA - `inst-rt-target`
3. [x] - `p1` - Treat the chat as provider-purged only when no attachment row is `pending` and no `chat_vector_stores` row remains, and as fully purged from provider file storage only when every attachment row is `done`; `failed` rows remain provider-file debt - `inst-rt-purged`
4. [x] - `p1` - Leave a turn that is running when the chat is deleted untouched: it is finalized normally and billed; if its `file_search` hits a store already deleted, the turn's own error handling applies - `inst-rt-running-turn`
5. [x] - `p1` - Emit no audit event for the chat deletion or its cleanup - `inst-rt-no-audit`
6. [x] - `p1` - Treat summary text as customer content: keep it tenant-scoped, never log it, never place it in a metric label, an outbox payload or a reject reason, and never send a deleted chat's content to the provider again - `inst-rt-content`
7. [x] - `p1` - **RETURN** - `inst-rt-return`

### Background Maintenance Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-metrics`

**Input**: the outcomes of the thread-summary and chat-cleanup handlers

**Output**: the series of DESIGN 4 `Metrics (OTLP) and Alerts (P1)` in the areas Thread summary and Cleanup and DESIGN 3.6 `Observability (P1)`; the instruments are registered by `cpt-cf-mini-chat-algo-gear-foundation-observability`.

**Steps**:
1. [x] - `p1` - Record `mini_chat_thread_summary_execution{result}` with exactly one of `success`, `provider_error`, `empty_summary`, `retry`, `model_unavailable`, `frontier_deleted` and `base_missing` per decided delivery; a delivery that ends in a CAS conflict, a deleted chat, an empty range or a malformed payload records no result - `inst-mt-execution`
2. [x] - `p1` - Increment `mini_chat_thread_summary_cas_conflicts` when a delivery loses the CAS precondition at the pre-check or at the commit - `inst-mt-cas`
3. [x] - `p1` - Increment `mini_chat_summary_fallback` exactly when the summary provider call fails (the `provider_error` cases), never for `empty_summary`, `model_unavailable`, other resolution failures or a failed commit - `inst-mt-fallback`
4. [x] - `p1` - Record `mini_chat_cleanup_completed{resource_type}`, `mini_chat_cleanup_retry{resource_type, reason}`, `mini_chat_cleanup_failed{resource_type}` with `resource_type` `file` or `vector_store`, `mini_chat_cleanup_vector_store_with_failed_attachments` and `mini_chat_secondary_cleanup_skipped{provider_kind}` from the chat-cleanup handler; the attachment cleanup handler records the same series for its own path - `inst-mt-cleanup`
5. [x] - `p1` - Leave `cleanup_backlog`, `summary_regen` and the `outbox_*` series declared but not recorded; read queue lag, retry backlog, lease churn and dead-letter depth from the shared outbox surface - `inst-mt-deferred`
6. [x] - `p1` - Leave `mini_chat_thread_summary_trigger` to the trigger of `cpt-cf-mini-chat-feature-message-streaming` - `inst-mt-trigger-elsewhere`
7. [x] - `p1` - Use only low-cardinality labels; never a tenant, user, chat, message, attachment, request, file or vector store identifier, and never free-form error text - `inst-mt-labels`
8. [x] - `p1` - **RETURN** - `inst-mt-return`

### Background Maintenance Configuration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-maintenance-configuration`

**Input**: the configuration validated by `cpt-cf-mini-chat-algo-gear-foundation-config-validation` and the policy snapshot

**Output**: the values this feature reads; this feature adds no configuration key. Rules: DESIGN B.9.2, B.9.3, B.9.4, B.5.5.

**Steps**:
1. [x] - `p1` - Read `thread_summary_worker.max_attempts` (3), `thread_summary_worker.summary_model_id` (empty means `gpt-4.1-mini`), `thread_summary_worker.summary_system_prompt` (built-in B.5.5 text), `thread_summary_worker.message_content_limit` (4000, 0 means no truncation) and `thread_summary_worker.claim_timeout_secs` (300, range 30 to 3600; also the queue lease set by the foundation) - `inst-cf-summary`
2. [x] - `p1` - Leave `thread_summary_worker.enabled` (true) and `thread_summary_worker.compression_threshold_pct` (80) to the trigger; treat `thread_summary_worker.reconcile_interval_secs` as deprecated with no effect - `inst-cf-summary-elsewhere`
3. [x] - `p1` - Read `cleanup_worker.max_attempts` (5); treat `cleanup_worker.enabled`, `poll_interval_secs`, `reconcile_interval_secs`, `stale_in_progress_timeout_secs` and `batch_size` as deprecated with no effect (ADR-0010) - `inst-cf-cleanup`
4. [x] - `p1` - Read `outbox.thread_summary_queue_name` (`mini-chat.thread_summary`), `outbox.chat_cleanup_queue_name` (`mini-chat.chat_cleanup`), `outbox.queue_name` (`mini-chat.usage_snapshot`) and `outbox.num_partitions` (4) through the foundation's queue registrations - `inst-cf-outbox`
5. [x] - `p1` - Read per delivery the summary model's catalog fields `provider_model_id`, `provider_id`, `enabled`, `context_window`, `max_output_tokens`, `max_input_tokens`, `estimation_budgets.bytes_per_token_conservative` and `thread_summary_prompt` - `inst-cf-catalog`
6. [x] - `p1` - Keep the fixed values out of configuration: at most 2 prompt-too-long retries, drops of one fifth of the kept messages (rounded up) with at least two kept, the `ceil(bytes / 4)` estimate fallback, the 10 s margin below the summary lease, the system subject id, the B.5.5 prompt texts, the read bounds of the summary call (16 KiB of an error body, 1 MiB of a success body) and, for chat cleanup, the 5 s deadline of every provider call and the 20 s time budget of one delivery (both below the shared 30 s lease) - `inst-cf-fixed`
7. [x] - `p1` - **RETURN** the values - `inst-cf-return`

## 4. States (CDSL)

### Thread Summary Frontier State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-background-maintenance-summary-frontier`

**States**: Empty, Committed

**Initial State**: Empty (no `thread_summaries` row for the chat; the frontier is empty)

**Transitions**:
1. [x] - `p1` - **FROM** Empty **TO** Committed **WHEN** a work item with an empty base wins the insert of the summary CAS commit, which creates the row with the frozen target as the frontier and marks the range compressed - `inst-fr-first-commit`
2. [x] - `p1` - **FROM** Committed **TO** Committed **WHEN** a work item whose base frontier equals the stored frontier wins the compare-and-set update, which moves the frontier forward to the frozen target and marks exactly the new range compressed - `inst-fr-advance`
3. [x] - `p1` - **FROM** Committed **TO** Empty **WHEN** a retry, edit or delete whose target turn is covered by the frontier deletes the row and clears `is_compressed` on all messages of the chat; this transition is owned by `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation`, never by this feature - `inst-fr-invalidated`
4. [x] - `p1` - **FROM** Committed **TO** Committed **WHEN** a delivery fails at the provider, parses an empty summary, finds its target deleted, finds its base missing or loses the CAS: the row, the frontier and `is_compressed` stay unchanged (the same holds from Empty to Empty) - `inst-fr-unchanged`

### Chat Provider Purge State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge`

**States**: FilesPending, StorePending, Purged, PurgedWithFileDebt

**Initial State**: FilesPending (the chat delete transaction committed with at least one attachment row `pending`); a deleted chat with no `pending` row starts in StorePending when it has a `chat_vector_stores` row and in Purged otherwise. The state is derived from the attachment rows and the `chat_vector_stores` row; it is not persisted, and no vector-store-specific row state exists (DESIGN 3.6).

**Transitions**:
1. [x] - `p1` - **FROM** FilesPending **TO** FilesPending **WHEN** a delivery leaves at least one attachment row `pending` (a failed delete below the budget, a row not attempted because the delivery time budget ended, or an infrastructure error) and returns `Retry` - `inst-pp-files-retry`
2. [x] - `p1` - **FROM** FilesPending **TO** StorePending **WHEN** every attachment row is `done` or `failed` and the `chat_vector_stores` row still exists - `inst-pp-files-done`
3. [x] - `p1` - **FROM** FilesPending **TO** Purged **WHEN** every attachment row is `done` and no `chat_vector_stores` row exists - `inst-pp-files-purged`
4. [x] - `p1` - **FROM** FilesPending **TO** PurgedWithFileDebt **WHEN** no row is `pending`, at least one row is `failed` and no `chat_vector_stores` row exists - `inst-pp-files-debt`
5. [x] - `p1` - **FROM** StorePending **TO** Purged **WHEN** the provider store delete returned 2xx or 404 (or the row was a NULL placeholder), the row was deleted and every attachment row is `done` - `inst-pp-store-purged`
6. [x] - `p1` - **FROM** StorePending **TO** PurgedWithFileDebt **WHEN** the store row was deleted as above and at least one attachment row is `failed` - `inst-pp-store-debt`
7. [x] - `p1` - **FROM** StorePending **TO** StorePending **WHEN** the store delete failed (a hit 5 s deadline included); the row is kept and the message is retried, or dead-lettered on the delivery that reaches `cleanup_worker.max_attempts` until a replay; a delivery whose time budget ended before the store call also keeps the row and returns `Retry` without a failed delete - `inst-pp-store-retry`
8. [x] - `p1` - **FROM** PurgedWithFileDebt **TO** PurgedWithFileDebt **WHEN** any later delivery or replay arrives: `failed` rows are terminal in P1 and only operator intervention outside the gear or a P2 recovery policy clears the debt - `inst-pp-debt-terminal`

## 5. Definitions of Done

### Outbox Handlers Replace the Placeholders

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring`

The system **MUST** serve `outbox.thread_summary_queue_name` and `outbox.chat_cleanup_queue_name` with the thread-summary and chat-cleanup handlers instead of `UnwiredHandler`, keeping the foundation's partitioning by `chat_id`, leases and global tuning, **MUST** return only `Ok`, `Retry` or `Reject(reason)`, **MUST NOT** add a gear-local poller, claim table, leader-elected worker or task state machine, and **MUST** run both handlers outside any user scope with every query filtered on the payload's `tenant_id` and `chat_id` and every provider call under the S2S context.

**Implementation guidance** (binding contracts of the implementation notes): add the two handler slots next to the existing audit, usage and attachment-cleanup slots of the foundation's queue handler registry; keep the handler logic in `domain/` behind ports (no `crate::infra`, `sea_orm`, `axum` or `http` imports there); repositories take `runner` so the summary upsert, the `is_compressed` update and the usage enqueue share one `DBProvider::transaction`, and the returned `Wake` fires only after the commit; use the fixed-width sort key of `infra/storage/chat_page.rs` for every timestamp order, range and equality on SQLite; treat `OutboxMessage::attempts` as 0-based.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-handler-wiring`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: none (outbox handlers only)
- DB: shared outbox tables (through the foundation enqueuer and processor)
- Entities: thread-summary work item, chat-cleanup outbox message

### Thread Summary Execution

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-summary-execution`

The system **MUST** process each thread-summary work item against the frozen range carried by its payload, loading exactly the non-deleted, non-compressed messages after the base frontier and at or before the frozen target in the order `(created_at, id)`, **MUST** resolve the summary model from `thread_summary_worker.summary_model_id` (empty means `gpt-4.1-mini`) with the enabled filter, **MUST** build the B.5.5 prompt with the catalog `thread_summary_prompt`, configured or built-in system prompt, the `<existing_summary>` block when a base exists, `User:` and `Assistant:` entries cut at `message_content_limit` characters with `...`, fit it to the summary model's input budget by dropping the oldest fifth while keeping two messages, **MUST** make one non-streaming call through OAGW with the summary model's `max_output_tokens`, the system identity and `request_type` `summary`, with at most two prompt-too-long retries, **MUST** build that call per adapter kind (`openai_responses` including the Azure variant, `openai_chat_completions`, `vllm_responses`, `anthropic_messages`) with no tools, `store` false on the Responses adapters and the URI `/{upstream_alias}{api_path}` with only `{model}` replaced, **MUST** extract the response text and the usage (input, output, cached and reasoning tokens where present) from the non-streaming response of that kind, **MUST** map a non-2xx answer, a gateway error, the deadline and a malformed body to the provider failure of the summary handler and an empty text to `empty_summary`, and **MUST** store only the `<summary>` content with the specified `token_estimate`.

**Implements**:
- `cpt-cf-mini-chat-flow-background-maintenance-long-conversation`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-payload`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-handler`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-prompt`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing`

**Implementation guidance**: reuse `provider_uri` and `composite_user` of `domain/streaming/provider_request.rs` and the `UsageTokens` shape of `domain/streaming/provider_events.rs`; add the non-streaming request builders and response parsers as new functions behind the same port as the streaming adapters, without changing the streaming ones; keep them in `domain/` free of `crate::infra`, `sea_orm`, `axum` and `http` imports.

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: none; outbound `POST /{upstream_alias}{api_path}` through OAGW
- DB: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-chats`
- Entities: `ThreadSummary`, thread-summary work item

### Stable Range and CAS Commit

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-summary-commit`

The system **MUST** commit a summary in one transaction that locks and re-checks the target frontier message (skipping the commit with `Ok` and `result="frontier_deleted"` when a concurrent retry, edit or delete soft-deleted it), inserts the first `thread_summaries` row or updates the row only when the stored frontier still equals the base frontier, marks exactly the frozen range `is_compressed` and enqueues the system-task usage event, **MUST** allow at most one successful commit per frozen range `(chat_id, base frontier, frozen target)`, **MUST** finish without another commit after a lost CAS (`mini_chat_thread_summary_cas_conflicts`) or a missing base (`result="base_missing"`), and **MUST** never let messages appended after the frozen target cancel, widen or invalidate a run.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-commit`
- `cpt-cf-mini-chat-state-background-maintenance-summary-frontier`

**Constraints**: none owned

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-messages`
- Entities: `ThreadSummary`

### Summary Failure, Retry and Dead Letter

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-summary-failure-retry`

The system **MUST** keep the previous summary, the frontier and `is_compressed` unchanged on every failed attempt, **MUST** return `Retry` for provider failures (`provider_error`, with `mini_chat_summary_fallback`), empty summaries (`empty_summary`) and infrastructure or resolution failures (`retry`), **MUST** return `Reject` instead on the `thread_summary_worker.max_attempts`-th delivery (default 3), **MUST** reject at once a malformed payload and a missing or disabled summary model (`model_unavailable`), **MUST** acknowledge without a provider call a work item of a deleted chat, and **MUST** reuse the payload's `system_request_id` on every retry and replay.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget`
- `cpt-cf-mini-chat-algo-background-maintenance-summary-handler`

**Constraints**: none owned

**Touches**:
- API: none
- DB: none beyond the summary tables
- Entities: thread-summary work item

### System Task Isolation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-system-task-isolation`

The system **MUST** run the summary as a system task that writes no `chat_turns` row, uses no turn finalization CAS, runs no quota preflight, reserve or settlement and changes no `quota_usage` row, **MUST** enqueue in the commit transaction exactly one usage event with `requester_type` `system`, `billing_outcome` `system_task`, `settlement_method` `none`, 0 credits, the summary call's token counts, `request_id` and `dedupe_key` built from the stable `system_request_id`, and no `user_id` or `turn_id`, and **MUST NOT** emit an audit event, check a kill switch or debit a tenant operational bucket.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-system-task-usage`

**Constraints**: none owned (system tasks are outside `cpt-cf-mini-chat-constraint-quota-before-outbound`, which applies to user turns)

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-chat-turns` (never written), `cpt-cf-mini-chat-dbtable-quota-usage` (never written), usage outbox queue
- Entities: `UsageEvent`

### Chat Deletion Cleanup Handler

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-chat-cleanup-handler`

The system **MUST** reject a malformed chat-cleanup message and a message for a missing or active chat, **MUST** load the current `pending` attachment rows of the soft-deleted chat from the payload's `tenant_id` and `chat_id`, **MUST** delete each provider file (and the Anthropic secondary copy best effort) through OAGW, treating 2xx and 404 as success, **MUST** apply the `cleanup_status` transitions of `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status` with guarded writes (`done` on success, `cleanup_attempts` plus 1 with `last_cleanup_error` and `cleanup_updated_at` on failure, `failed` at `cleanup_worker.max_attempts` while continuing with the other rows), **MUST** return `Retry` without incrementing on a database error and while any row stays `pending`, **MUST** bound every provider call (file delete, secondary delete, vector store delete) by a fixed 5 s deadline whose hit counts as a failed call of that resource, **MUST** start no provider call once 20 s of a delivery elapsed (re-checking the timer immediately before every provider-bound call, the policy snapshot read included, so the latest call ends by 25 s plus the DB writes) and return `Retry` while a row is `pending` or the store is still to be deleted (a budget-driven `Retry` counts as a delivery for the vector store reject rule, never as a row attempt), **MUST** resolve the Anthropic alias of a secondary delete at handling time from the chat's `model` through the policy snapshot catalog and the provider registry for the chat's tenant, with a fallback to the tenant's single `anthropic_messages` provider and otherwise `mini_chat_secondary_cleanup_skipped{provider_kind}` with a warning naming the chat id and the attachment id, never failing the primary row because of the secondary, and **MUST** keep every step idempotent under redelivery, replay and concurrent delivery.

**Implements**:
- `cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup`
- `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload`
- `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler`
- `cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file`
- `cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: none; outbound `DELETE /{alias}{prefix}/files/{provider_file_id}` and the Anthropic Files delete through OAGW
- DB: `cpt-cf-mini-chat-dbtable-chats` (read, including `model`), `cpt-cf-mini-chat-dbtable-attachments`
- Entities: chat-cleanup outbox message, `Attachment`

### Vector Store Cleanup Ordering

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-vector-store-ordering`

The system **MUST** delete the chat's provider vector store only when no attachment row of the chat is `pending`, **MUST** record `mini_chat_cleanup_vector_store_with_failed_attachments` before the delete when any row is `failed`, **MUST** treat 2xx and 404 as success and then delete the `chat_vector_stores` row as the completion marker, **MUST** keep the row on a failed delete and return `Retry` until the delivery that reaches `cleanup_worker.max_attempts`, which returns `Reject` with the reason `vector store delete: max attempts (N) reached`, **MUST** return `Retry` without a limit on database errors, **MUST** bound the store delete by the fixed 5 s provider call deadline (a hit deadline is a failed delete), and **MUST** return `Retry` without a provider call and without a `Reject` when the 20 s delivery budget ended before the store call, keeping the order that the store is deleted only when no attachment row is `pending`.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup`
- `cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: none; outbound `DELETE /{alias}{prefix}/vector_stores/{vector_store_id}` through OAGW
- DB: `cpt-cf-mini-chat-dbtable-chat-vector-stores`, `cpt-cf-mini-chat-dbtable-attachments`
- Entities: `ChatVectorStore`

### Cleanup Ownership Between the Two Paths

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-cleanup-boundaries`

The system **MUST** leave the attachments of an active chat to the attachment-delete path, **MUST** give the chat-cleanup handler exclusive ownership of the provider files of a soft-deleted chat's `pending` rows and of its vector store from the soft-delete commit on, and **MUST** keep a duplicate delete by both paths harmless through 404-as-success and the `pending` guard on the terminal write.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries`

**Constraints**: none owned

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`
- Entities: `Attachment`, `ChatVectorStore`

### P1 Retention

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-retention`

The system **MUST** keep soft-deleted chats and all their child rows without hard purge, **MUST** remove provider files and vector stores only through the outbox cleanup handlers with the best-effort one-hour target, **MUST** count a chat as provider-purged only with no `pending` row and no `chat_vector_stores` row and as fully purged from file storage only when every row is `done`, **MUST** leave a running turn of a deleted chat to finish and be billed, **MUST** emit no chat-deletion audit event, and **MUST** treat summary text as customer content that never reaches logs, labels or payloads.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-retention`

**Constraints**: none owned

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-thread-summaries`
- Entities: `Chat`, `ThreadSummary`

### Observability and Configuration

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-observability-config`

The system **MUST** record the thread-summary execution, CAS conflict and fallback series and the cleanup series with the bounded label values of the metrics algorithm and no identifiers or content in labels, **MUST** leave the declared-but-not-recorded series unrecorded, and **MUST** read only the configuration keys and catalog fields of the configuration algorithm without adding a key.

**Implements**:
- `cpt-cf-mini-chat-flow-background-maintenance-admin-observability`
- `cpt-cf-mini-chat-algo-background-maintenance-metrics`
- `cpt-cf-mini-chat-algo-background-maintenance-configuration`

**Constraints**: none owned

**Touches**:
- API: none
- DB: none
- Entities: none

### Deferred and Not-Implemented Scope Stays Unimplemented

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-deferred-scope`

The system **MUST NOT** implement hard purge of soft-deleted rows after a grace period, a chat-deletion audit event, cancelling a running turn on chat deletion (ADR-0009), the temporary chat 24-hour cleanup (P2, `cpt-cf-mini-chat-usecase-temporary-chat-cleanup`), document summary generation (ADR-0007), charging system tasks to a tenant operational bucket, auditing them or checking kill switches for them (ADR-0008), the summary quality gate with length and entropy validation and `summary_regen` (P2+), the provider file reconciliation job for provider-side orphan files (P2), an automatic re-attempt of `failed` attachment rows, or a summary trigger of its own, and **MUST** keep them recorded as deferred or not implemented.

**Implements**:
- `cpt-cf-mini-chat-algo-background-maintenance-retention`
- `cpt-cf-mini-chat-algo-background-maintenance-system-task-usage`

**Constraints**: none owned

**Touches**:
- API: none
- DB: none
- Entities: none

### Test Layering, Traceability and Feature-Local Deviations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-maintenance-tests-and-deviations`

The system **MUST** cover every acceptance-criteria item that this feature owns with tests in the layers below, and **MUST** record each feature-local deviation with its rationale and review owner.

**Test layering**:
- Unit tests live in the gear crate (`gears/mini-chat/mini-chat/`) and cover pure logic: both payload parsers (every field, base half-set, wrong `system_task_type` or `reason`, bad dedupe prefix), the summary prompt (no base, with base and `<existing_summary>`, system messages skipped, truncation at `message_content_limit` with `...` on a multi-byte character, limit 0), prompt fitting (budget formula, `max_input_tokens` 0 and set, `context_window` 0, drops of one fifth rounded up, two kept), the prompt-too-long retry bound, response parsing (analysis removed, summary extracted, blank lines collapsed, no summary block, leftover markup gives empty), the `token_estimate` rule (positive difference, zero and reasoning-only usage), the summary model resolution (empty id, missing, disabled, provider failure), the retry budget with 0-based `attempts`, the system-task usage event fields and omissions, the outcome and metric classification of both handlers, the vector store eligibility and the reject reasons, the per-kind request body of the summary adapter call (each of the four kinds with exact fields: `instructions` or `system` or the `system` message, `input` or `messages`, the output cap field, `store` false on the Responses kinds, `user` or `metadata.user_id`, `metadata` only on `openai_responses`, no tools, no `extra_body`), the URI with only `{model}` replaced and the Azure `api-version` kept, the text extraction fixtures per kind (a Responses body with a `reasoning` item and an `incomplete` status, a vLLM body with a `<think>` block, a Chat Completions body with null content and with `finish_reason` `length`, an Anthropic body with a `thinking` block and with `stop_reason` `max_tokens`), the usage reading per kind (cached, reasoning and cache-write counts, absent counts and an absent usage object), the failure mapping (non-2xx, 429, the context-length classification for each wording (an OpenAI `error.code`, an Anthropic message, a vLLM fixture with `type` `BadRequestError` and "This model's maximum context length is ..." nested under `error`, the same vLLM body with `message` at the top level, and a 400 with `type` `invalid_request_error` and an unrelated message that is not classified), a gateway 504, a malformed body, a Responses `failed` status, an empty text giving `empty_summary`), the chat-cleanup delivery budget and call deadline decisions on a controllable timer (including a timer re-check before each provider-bound call and the case primary finishes just before 20 s, snapshot slow: the secondary delete does not start, the row stays `pending` and no attempt is counted), and the secondary target resolution (model in the catalog with an Anthropic provider and a tenant override, model missing, provider not Anthropic, snapshot error, exactly one and several Anthropic providers).
- Repository tests with SQLite cover range selection with equal `created_at` and fractional seconds of different lengths (`.12` and `.123`), soft-deleted and compressed messages and messages after the target, the CAS commit (first insert, update with a matching base, insert conflict, moved frontier, missing base, deleted target) with the exact `is_compressed` range and exactly one usage outbox row, rollback leaving nothing on a forced enqueue failure, the guarded cleanup writes (`done`, attempt increment, `failed` at the budget, lost guard), the placeholder and bound `chat_vector_stores` row deletion, and that no `chat_turns` or `quota_usage` row changes.
- Concurrency tests over SQLite with a controlled interleaving: a summary commit against the mutation transaction of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` that soft-deletes the target (both orders: the worker skips with `frontier_deleted`, or the mutation invalidates the committed summary), two deliveries of work items with the same base (one commit, one CAS conflict), and the chat-cleanup and attachment-cleanup handlers deleting the same file (one terminal write, both outcomes success).
- In-process handler tests drive the real outbox pipeline over SQLite with the mock OAGW `ServiceGatewayClientV1` and the scripted storage provider of the test support, without network: a long conversation enqueues work, the handler commits, and the next stream reports `thread_summary_applied`; a failing provider retries and dead-letters on the third delivery with the previous summary intact; a disabled summary model rejects at once; a chat deletion deletes every file (404 included) and then the store; a file failing five times becomes `failed` while the others finish and the store is deleted with the debt metric; a store failing on every delivery is rejected on the fifth delivery with the row kept and is deleted by a replay; a scripted provider that never answers makes a file delete fail at the 5 s deadline as a counted attempt, makes the secondary delete a logged skip and the store delete a failed delete; a scripted slow provider ends a delivery at the 20 s budget with `Retry`, a primary delete that finishes just before 20 s followed by a slow policy snapshot starts no secondary delete and leaves the row `pending`, rows not attempted keep `cleanup_attempts`, the store is not called while a row is `pending` and a budget-driven `Retry` never rejects; a secondary copy is deleted on the alias of the chat model's Anthropic provider, on the single Anthropic provider after the model left the catalog, and skipped with the metric and a warning when several Anthropic providers exist; the summary call of each adapter kind is driven through the mock OAGW with the recorded non-streaming fixtures. PostgreSQL row-lock ordering (`FOR UPDATE` on the target message) is reviewed against the PostgreSQL variant of the repositories.
- No tests are added under `testing/e2e/suites/mini_chat/`.
- Every implemented CDSL step carries `@cpt-begin` and `@cpt-end` markers with its ID, phase token and `inst-*` ID (traceability mode FULL).

**Acceptance-criteria test mapping**:

| Acceptance-criteria item | Flows, algorithms and DoDs | Test layer |
|--------------------------|----------------------------|------------|
| Chat deletion triggers reliable background cleanup of provider-side resources | `cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup`, `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler`, `cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file`, `cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file`, `cpt-cf-mini-chat-algo-background-maintenance-vector-store-cleanup`, `cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries`, `cpt-cf-mini-chat-state-background-maintenance-chat-provider-purge`, `cpt-cf-mini-chat-dod-background-maintenance-chat-cleanup-handler`, `cpt-cf-mini-chat-dod-background-maintenance-vector-store-ordering`, `cpt-cf-mini-chat-dod-background-maintenance-cleanup-boundaries`, `cpt-cf-mini-chat-dod-background-maintenance-retention` | Unit (parsing, eligibility, outcomes), repository (guarded writes, row deletion), concurrency (duplicate delete), in-process handler with mock OAGW and scripted storage (files then store, 404, budgets, call deadline, delivery budget, secondary alias resolution, dead letter and replay) |
| Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly | `cpt-cf-mini-chat-flow-background-maintenance-long-conversation`, `cpt-cf-mini-chat-algo-background-maintenance-summary-handler`, `cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck`, `cpt-cf-mini-chat-algo-background-maintenance-summary-commit`, `cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget`, `cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call`, `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`, `cpt-cf-mini-chat-algo-background-maintenance-system-task-usage`, `cpt-cf-mini-chat-state-background-maintenance-summary-frontier`, `cpt-cf-mini-chat-dod-background-maintenance-summary-execution`, `cpt-cf-mini-chat-dod-background-maintenance-summary-commit`, `cpt-cf-mini-chat-dod-background-maintenance-summary-failure-retry`, `cpt-cf-mini-chat-dod-background-maintenance-system-task-isolation` (the mutation-side deletion is tested by `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`) | Unit (prompt, fitting, per-kind request body, response extraction, usage reading, failure mapping, parsing, retry budget), repository (range, CAS commit, `is_compressed`, usage row), concurrency (commit against mutation, two work items), in-process handler and SSE with mock OAGW (commit then `thread_summary_applied`, retry and dead letter, model unavailable) |

**Reliability and data integrity**: the summary commit is one atomic transaction guarded by the target-frontier lock and the frontier compare-and-set, so at most one commit advances a frozen range and a failed attempt leaves nothing; cleanup writes are guarded conditional updates, provider deletes are idempotent with 404 as success, and every retry is bounded (`thread_summary_worker.max_attempts`, the per-row `cleanup_attempts` budget and the per-message vector store limit) except database failures, which the shared outbox retries with backoff; every chat-cleanup provider call has a 5 s deadline and every chat-cleanup delivery starts no provider call after 20 s, so a delivery ends inside the 30 s lease. **Security**: both handlers run without a user scope, filter every query on the payload's tenant and chat, call providers only through OAGW under the S2S context with no credential in the gear, and never place content, summary text or provider identifiers in logs, labels, payloads or reject reasons. **Observability**: the series of the metrics algorithm plus the shared outbox surface for lag and dead letters. **Rollback**: a failed transaction leaves no partial state; provider deletions cannot be undone, which is why they only follow the soft-delete commit; schema rollback is owned by the foundation migrations. **Compile-time gates**: those of the foundation apply (workspace lints, the domain layering lint, the domain-model lint on new domain types, the example server build with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`).

**Feature-local deviations** (review owner for every item: the mini-chat gear owners who maintain `gears/mini-chat/docs/`):

| Deviation | Rationale | Source |
|-----------|-----------|--------|
| A malformed thread-summary or chat-cleanup payload is rejected at once (dead letter) and records no execution result | Such a message can never succeed on retry; the DESIGN names `Reject` only for a chat that is not soft-deleted and its `result` allowlist has no value for this case | DESIGN 3.6, B.9.3 |
| A summary work item of a missing or soft-deleted chat is acknowledged without a provider call and without an execution result | The DESIGN is silent; sending a deleted chat's content to the provider serves no later turn | DESIGN 3.6, 4 `Data Classification and Retention (P1)` |
| The pre-check also detects a missing base (`base_missing`) and a deleted target (`frontier_deleted`) before the provider call; the commit repeats both checks atomically | Saves provider calls that cannot commit; the commit-time check stays authoritative | DESIGN 3.6 stage 4, observability (`at the pre-check or at the commit`) |
| A work item whose frozen range holds no message is acknowledged without a provider call and without an execution result | Nothing to summarize; the allowlist has no value for it | DESIGN 3.6 `Range selection` |
| The provider phase of one summary delivery is bounded by `thread_summary_worker.claim_timeout_secs` minus 10 s (fixed); hitting the bound is `provider_error` | The DESIGN sizes the lease so the call is not redelivered mid-flight but fixes no call deadline; the margin keeps a hung call inside its lease | DESIGN 3.6 stage 3, B.9.4 |
| The usage event of a summary reports the token counts of the successful call only; rejected prompt-too-long attempts add nothing | The DESIGN carries one usage object per system task; a rejected context-length request returns no usage | DESIGN 3.2 `System Task Attribution Rules (Normative)` |
| The prompt-too-long retry drops one fifth of the kept messages (rounded up), keeps at least two, and is made at most twice per delivery | The DESIGN says `~20%` and `up to 2 retries`; a deterministic rule is testable | DESIGN 3.6 stage 3 |
| The whole frozen range is marked `is_compressed` even when prompt fitting dropped its oldest messages | The DESIGN marks exactly the frozen range; recorded so reviewers see that dropped messages leave the context | DESIGN 3.6 stage 4 |
| On SQLite the summary commit transaction takes the database write lock before its first read instead of `SELECT ... FOR UPDATE` | SQLite has no row locks; the write lock orders the commit and a mutation with the same outcome | DESIGN 3.9 `Summary Interaction on Turn Mutation` |
| The thread-summary handler stays wired and processes queued work when `thread_summary_worker.enabled` is false | The DESIGN defines the flag as a gate on the trigger only | DESIGN B.9.4 |
| A chat-cleanup message for a missing chat is rejected like one for an active chat (reason `chat not found`) | Chat rows are never hard-deleted in P1, so a missing chat means a corrupt message | DESIGN 3.6 `Outbox payload and execution semantics` |
| One chat-cleanup delivery attempts each `pending` row at most once, within the delivery time budget, then returns `Retry` if any row is still `pending` | The DESIGN requires continuing with the other attachments; one attempt per row per delivery keeps the per-row budget aligned with deliveries | DESIGN 3.6 `Attachment cleanup state machine`, B.9.2 |
| A `pending` row without `provider_file_id` is marked `done` and counted in `mini_chat_cleanup_completed{resource_type="file"}` | Same rule as the attachment cleanup handler; nothing exists at the provider to delete | FEATURE 0005 `inst-ch-return-no-file` |
| A `chat_vector_stores` placeholder row (NULL `vector_store_id`) is deleted without a provider call | The DESIGN covers only a bound row; the upload's creation protocol deletes a store whose compare-and-set lost to this deletion | DESIGN 3.7 `Table: chat_vector_stores` (creation protocol, deletion race) |
| The secondary Anthropic copy is deleted after the primary delete succeeded, best effort, with `mini_chat_secondary_cleanup_skipped` when no client exists or the alias cannot be resolved | The DESIGN deletes it only when the alias resolves at delete time and gives it no retry budget | DESIGN 4 `Cleanup on Chat Deletion`, 3.6 observability |
| `mini_chat_cleanup_vector_store_with_failed_attachments` is recorded before each vector store delete attempt of a chat with `failed` rows, so a retried delete counts again | The DESIGN requires the metric before proceeding with the deletion | DESIGN 3.6 `Vector store cleanup ordering` |
| `failed` attachment rows of a deleted chat are not re-attempted by a replay of the chat-cleanup message | The DESIGN makes `failed` terminal and leaves the debt to operator intervention or a future policy | DESIGN 3.6 `Additional invariants` 3 |
| Where `config/mini-chat.yaml` and the DESIGN differ, the DESIGN governs | `gears/mini-chat/docs/` is the source of truth | DECOMPOSITION 1 |
| Every provider call of the chat-cleanup handler (file delete, secondary file delete, vector store delete) has a fixed deadline of 5 s, and a hit deadline is a failed call of that resource: a counted file attempt (cause `the provider call exceeded its deadline`), a logged and not retried secondary delete, a failed store delete | The DESIGN gives the shared 30 s lease but no call deadline; without one, hung calls in a sequential delivery outlive the lease and the redelivery runs concurrently with the first | DESIGN B.9.3 (lease), 3.6 `Attachment cleanup state machine` |
| A chat-cleanup delivery starts no provider call once 20 s have elapsed on a monotonic timer and returns `Retry` while a row is `pending` or the store is still to be deleted; such a `Retry` counts as a delivery in the delivery count of the vector store reject rule (the shared outbox counts every redelivery) but never as a row attempt, and the store is never called while a row is `pending` | The timer is checked immediately before every provider-bound call (policy snapshot read, primary delete, secondary delete, store delete), so a latest call starts at or before 20 s, ends by 25 s (5 s call deadline), 5 s inside the lease, plus the DB writes after it; the DESIGN counts every delivery of the message for the store limit, so the count is kept rather than altered, and a failed store delete on a late delivery is dead-lettered and replayable | DESIGN B.9.2 (`all deliveries of the message count`), B.9.3 (lease), 3.6 `Vector store cleanup ordering` |
| The Anthropic alias of a secondary delete in chat cleanup is resolved at handling time (chat model, policy snapshot catalog, provider registry for the chat's tenant, accepted only for kind `anthropic_messages`), with a fallback to the tenant's single `anthropic_messages` provider and otherwise a skip, whereas the attachment cleanup handler uses the alias frozen at enqueue in `secondary_ref.upstream_alias` | The chat-cleanup payload carries no `secondary_ref` and the attachment row holds only `secondary_provider_kind`; the resolution uses the same provider semantics as the secondary copy of the upload; several Anthropic providers give no safe guess, so the copy is left and counted as skipped | DESIGN 4 `Cleanup on Chat Deletion` (alias resolved at delete time), FEATURE 0005 `inst-enq-secondary-ref`, `inst-sec-upload` |
| The non-streaming summary call is specified by this feature (request per adapter kind, OAGW call, text and usage extraction, failure mapping), not by FEATURE 0004, whose adapter kinds algorithm only states that each adapter provides it | The DESIGN requires the call through every adapter kind and `request_type` `summary` but no step exists upstream; keeping it here leaves the streaming adapters unchanged | DESIGN 3.2 `llm_provider`, 4 `Provider Request Metadata`, ADR-0005, FEATURE 0004 `inst-kind-match` |
| The summary request carries none of the catalog `general_config.api_params` and no `extra_body`; a 2xx body larger than 1 MiB or an error body read beyond 16 KiB is not read further; a truncated completion (`incomplete`, `length`, `max_tokens`) is used as it is and a missing closing `</summary>` then yields `empty_summary` | The DESIGN names no sampling parameter for the summary and tunes `api_params` for chat answers; the read bounds keep memory bounded; the truncated rule matches FEATURE 0004's truncated but valid completion | DESIGN 3.6 stage 3 (`Request format`, `Response parsing`), FEATURE 0004 `inst-tr-incomplete` |

**Implements**:
- `cpt-cf-mini-chat-flow-background-maintenance-long-conversation`
- `cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup`
- `cpt-cf-mini-chat-flow-background-maintenance-admin-observability`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`
- Entities: `ThreadSummary`, `Attachment`, `ChatVectorStore`, `UsageEvent`

## 6. Acceptance Criteria

The first two items are the DECOMPOSITION 2.7 "Acceptance Criteria Covered" and the matching items of `acceptance-criteria.md`; each is both implemented and tested. The remaining items pin the details behind them. The test layer of each item is given in parentheses.

- [ ] Chat deletion triggers reliable background cleanup of provider-side resources: after `DELETE /v1/chats/{id}` returns 204, the chat-cleanup handler deletes the provider file of every `pending` attachment row of the chat (documents and images, soft-deleted attachments included) through OAGW, treats 404 as success, sets each row `done`, and only then deletes the provider vector store and removes the `chat_vector_stores` row; a redelivery after a crash at any step finishes the remaining work without a duplicate effect; a file that keeps failing becomes `failed` after `cleanup_worker.max_attempts` attempts while the other rows finish and the store is still deleted with `mini_chat_cleanup_vector_store_with_failed_attachments` recorded; a store that keeps failing keeps its row and dead-letters the message on the delivery that reaches `cleanup_worker.max_attempts`, and a replay deletes it; a message for an active chat is rejected without any provider call (unit, repository, in-process handler with mock OAGW and scripted storage).
- [ ] Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly: a work item commits one summary over exactly its frozen range, marks exactly that range `is_compressed`, enqueues one `system_task` usage event and makes the next stream report `thread_summary_applied`; a provider failure keeps the previous summary and frontier, records `provider_error` and `mini_chat_summary_fallback`, retries, and dead-letters on the third delivery; an empty summary records `empty_summary` and retries; a disabled summary model is rejected at once with `model_unavailable`; when a retry, edit or delete soft-deletes the target frontier message before the commit, the handler commits nothing and records `frontier_deleted`; when a mutation deleted the base summary, it records `base_missing`; when a mutation commits after the summary, the mutation's invalidation removes it (unit, repository, concurrency over SQLite, in-process handler and SSE with mock OAGW).
- [ ] At most one commit per frozen range: two concurrent or replayed deliveries for the same range yield one committed summary, one usage event and one `mini_chat_thread_summary_cas_conflicts` increment; a delivery whose base differs from the stored frontier makes no provider call; messages appended after the frozen target are neither summarized nor marked (repository, concurrency).
- [ ] Range selection is exact on SQLite: messages with equal `created_at` and with fractional seconds `.12` and `.123` are ordered by the chronological key with the `id` tie-break, and soft-deleted and already compressed messages are excluded (repository).
- [ ] The summary request is as specified: the system prompt follows the catalog, configuration and built-in order; the user prompt holds `<existing_summary>` only when a base exists, one `User:` or `Assistant:` entry per non-system message with content cut at `message_content_limit` and `...`, and the analysis instruction; the request goes to `/{upstream_alias}{api_path}` with the summary model's `provider_model_id` and `max_output_tokens`, the system `user` value and, on the Responses adapter, `request_type` `summary` and `feature` `none`; an oversized prompt is fitted and a context-length rejection is retried at most twice (unit, in-process handler with mock OAGW).
- [ ] The non-streaming summary call is built and read per adapter kind: `openai_responses` (Azure included) sends `instructions`, an `input` of one `user` item, `max_output_tokens`, `stream` false, `store` false, `user` and `metadata` with `request_type` `summary`; `vllm_responses` sends the same without `metadata`; `openai_chat_completions` sends a `system` and a `user` message, `max_completion_tokens` and `user`; `anthropic_messages` sends `system`, one `user` message, `max_tokens` and `metadata.user_id`; none sends a tool; the text is read from the `output_text` parts, `choices[0].message.content` or the `text` blocks and the usage from the fields of its kind; a non-2xx answer, a gateway error, the deadline or a malformed body is a provider failure (`provider_error`, `mini_chat_summary_fallback`, retry budget), a context-length rejection is retried at most twice with a smaller prompt and an empty text is `empty_summary` (unit with recorded fixtures per kind, in-process handler with mock OAGW).
- [ ] The stored summary and estimate are as specified: only the `<summary>` content is stored with blank-line runs collapsed, `token_estimate` is `output_tokens` minus `reasoning_tokens` or the rounded-up quarter of the byte length, and no length or entropy check runs (unit, repository).
- [ ] The summary is a system task: no `chat_turns` row and no `quota_usage` row is written or changed, no audit event is enqueued, and the usage event has `requester_type` `system`, `billing_outcome` `system_task`, `settlement_method` `none`, 0 credits, the summary model as both models, `request_id` equal to the payload's `system_request_id`, the `{tenant_id}/thread_summary_update/{system_request_id}` key and no `user_id` or `turn_id`, identical across retries (unit, repository).
- [ ] Cleanup boundaries hold: the attachment cleanup handler acknowledges messages of a deleted chat without action while the chat-cleanup handler deletes those files; a duplicate delete of the same file by both paths ends with one terminal write and no error; the upload reaper and background indexing leave rows with a `cleanup_status` alone (concurrency, in-process handler).
- [ ] Chat cleanup stays inside its lease: every provider call is cut at 5 s and counts as a failed call of its resource (a file attempt, a not-retried secondary delete, a failed store delete); a delivery re-checks the timer immediately before every provider-bound call (policy snapshot read, primary delete, secondary delete, store delete), starts none after 20 s (the latest call starts at or before 20 s and ends by 25 s plus the DB writes) and returns `Retry` while a row is `pending` or the store remains, with no `cleanup_attempts` change for rows not attempted, the store never called while a row is `pending`, a budget-driven `Retry` counted only as a delivery for the store reject rule and never rejecting by itself (unit with a controllable timer, including the case primary finishes just before 20 s with a slow snapshot, in-process handler with a scripted slow and a scripted hanging provider).
- [ ] The secondary Anthropic file of a deleted chat is deleted on a deterministic alias: the chat's `model` is read with the chat row, its catalog entry in the current policy snapshot gives the `provider_id`, the registry resolves it for the chat's tenant and the alias is used only for kind `anthropic_messages`; when the model left the catalog, the provider is not Anthropic or cannot be resolved, or the snapshot cannot be read, the tenant's only `anthropic_messages` provider is used, and with none or several the delete is skipped with `mini_chat_secondary_cleanup_skipped{provider_kind}` and a warning naming the chat id and the attachment id; the primary row never becomes `failed` because of the secondary (unit, in-process handler with mock OAGW).
- [ ] Retention holds: after cleanup the chat, its messages, turns, reactions, attachments with thumbnails and its thread summary are still in the database, only the `chat_vector_stores` row is gone, no audit event exists, and a turn running at deletion time is finalized and billed (repository, in-process REST).
- [ ] The metrics are recorded with bounded labels: every execution `result` value, CAS conflicts, fallbacks and every cleanup series with `resource_type` `file` or `vector_store`, with no identifiers or content in labels (unit with a metrics recorder).
- [ ] The deferred scope stays unimplemented: no hard purge, no chat-deletion audit event, no temporary-chat cleanup, no document summary, no tenant operational bucket, audit or kill-switch check for system tasks, no summary quality gate, no provider file reconciliation and no automatic re-attempt of `failed` rows (review).
- [ ] Unit tests live in the gear crate, repository, concurrency and handler tests run in-process over SQLite with a mock OAGW `ServiceGatewayClientV1` and no network, no test is added under `testing/e2e/suites/mini_chat/`, and every implemented CDSL step has `@cpt-begin` and `@cpt-end` markers.
