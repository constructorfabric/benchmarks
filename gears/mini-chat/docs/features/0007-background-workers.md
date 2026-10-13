---
description: "Background workers feature of the mini-chat gear: the chat-cleanup outbox handler that deletes provider files, Anthropic secondary copies and the chat vector store of a soft-deleted chat with retries and dead-lettering, the leader-only orphan watchdog that finalizes stale running turns through the orphan CAS with bounded debit and usage and audit events, the thread-summary trigger in the finalization transaction and the outbox handler that summarizes as a system task and commits under a frontier CAS guarded against mutation-driven invalidation, the Kubernetes Lease leader elector, and the worker metrics."
---

# Feature: Background Workers

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-background-workers-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-background-workers`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat Deletion Cleanup](#chat-deletion-cleanup)
  - [Abandoned Turn Recovery](#abandoned-turn-recovery)
  - [Long Conversation Compression](#long-conversation-compression)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Chat Cleanup Handler](#chat-cleanup-handler)
  - [Chat Attachment Provider Cleanup](#chat-attachment-provider-cleanup)
  - [Chat Vector Store Cleanup](#chat-vector-store-cleanup)
  - [Orphan Watchdog Loop](#orphan-watchdog-loop)
  - [Orphan Turn Finalization](#orphan-turn-finalization)
  - [Summary Trigger Evaluation](#summary-trigger-evaluation)
  - [Thread Summary Handler](#thread-summary-handler)
  - [Summary Prompt Construction](#summary-prompt-construction)
  - [Summary Provider Call](#summary-provider-call)
  - [Summary Commit](#summary-commit)
  - [Kubernetes Lease Election](#kubernetes-lease-election)
  - [Worker Metrics Recording](#worker-metrics-recording)
- [4. States (CDSL)](#4-states-cdsl)
  - [Chat Provider Cleanup State Machine](#chat-provider-cleanup-state-machine)
  - [Orphan Candidate State Machine](#orphan-candidate-state-machine)
  - [Thread Summary Frontier State Machine](#thread-summary-frontier-state-machine)
  - [Worker Leadership State Machine](#worker-leadership-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Chat Deletion Provider Cleanup](#chat-deletion-provider-cleanup)
  - [Vector Store Release After Attachment Cleanup](#vector-store-release-after-attachment-cleanup)
  - [Orphan Watchdog Scheduling and Single Actor](#orphan-watchdog-scheduling-and-single-actor)
  - [Orphan Finalization and Bounded Debit](#orphan-finalization-and-bounded-debit)
  - [Summary Scheduling in the Finalization Transaction](#summary-scheduling-in-the-finalization-transaction)
  - [Thread Summary Execution](#thread-summary-execution)
  - [Guarded Summary Commit](#guarded-summary-commit)
  - [Leader Election for Leader-Only Workers](#leader-election-for-leader-only-workers)
  - [Background Worker Metrics](#background-worker-metrics)
  - [Deferred Lifecycle Capabilities](#deferred-lifecycle-capabilities)
  - [Cross-Cutting Quality and Verification](#cross-cutting-quality-and-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

This feature closes the lifecycle loops of the gear that run outside a request (DESIGN §3.6 `cpt-cf-mini-chat-seq-chat-deletion-cleanup` and `cpt-cf-mini-chat-seq-thread-summary`; §4 "Turn Lifecycle, Crash Recovery and Orphan Handling" `cpt-cf-mini-chat-design-turn-lifecycle`, "Orphan Turn Watchdog", "Watchdog Single-Actor Guarantee", "Cleanup on Chat Deletion", "Metrics (OTLP) and Alerts (P1)"; Appendix B.9). It owns four workers.

- **Chat cleanup handler**: it replaces the foundation placeholder of the queue `outbox.chat_cleanup_queue_name` and consumes the message that the chat soft delete of `cpt-cf-mini-chat-feature-chat-crud` enqueues. For every attachment of the deleted chat whose `cleanup_status` is `pending`, it deletes the provider file and the Anthropic secondary copy through OAGW, with per-attachment attempt counting up to `cleanup_worker.max_attempts`. Only when every attachment is terminal (`done` or `failed`) does it delete the chat's provider vector store and its `chat_vector_stores` row. The vector store delete is retried until the delivery that reaches the attempt limit, then the message is dead-lettered while the row is kept for a replay.
- **Orphan watchdog**: a leader-only periodic worker. It finds `running` turns whose durable progress time (`last_progress_at`, or `started_at` when NULL) is older than `orphan_watchdog.timeout_secs`. It finalizes each of them through the orphan variant of the CAS-guarded finalization of `cpt-cf-mini-chat-feature-quota-billing`: `failed` / `orphan_timeout`, a bounded estimated debit, and the usage event and the `turn_failed` audit event in the same transaction. A turn whose reserve fields are NULL is finalized without settlement.
- **Thread summary**: the trigger is evaluated in the finalization transaction of every completed turn and enqueues a durable work item to `outbox.thread_summary_queue_name`. The handler summarizes the frozen message range with the summary model as a system task, keeps the previous summary on failure, retries and dead-letters within `thread_summary_worker.max_attempts`, and commits the new summary under a frontier compare-and-set. The commit locks and re-checks the frozen target message, so it never resurrects content that a retry, edit or delete removed.
- **Leader election**: the gear-local Kubernetes Lease elector behind the cargo feature `k8s` (`kube` and `k8s-openapi` are already optional dependencies of the gear and present in `Cargo.lock`). It holds one Lease per role: `mini-chat-orphan-watchdog` and `mini-chat-upload-reaper`. Without the feature, the no-op elector of the foundation makes every process the leader.

It also records the worker metrics of DESIGN "Metric series" (orphan, cleanup and thread-summary groups).

**Implementation slices** (implement in this order; each slice is independently testable):

| # | Slice | CDSL elements |
|---|-------|---------------|
| 1 | Leader election with roles (the reaper adopts its role) | `cpt-cf-mini-chat-algo-background-workers-elect-leader`, `cpt-cf-mini-chat-state-background-workers-leadership` |
| 2 | Orphan watchdog | `cpt-cf-mini-chat-algo-background-workers-run-watchdog`, `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`, `cpt-cf-mini-chat-state-background-workers-orphan-candidate`, `cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery` |
| 3 | Chat cleanup handler | `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`, `cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment`, `cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store`, `cpt-cf-mini-chat-state-background-workers-chat-cleanup`, `cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup` |
| 4 | Summary trigger in the finalization transaction | `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger` |
| 5 | Summary handler, provider call and commit | `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`, `cpt-cf-mini-chat-algo-background-workers-build-summary-prompt`, `cpt-cf-mini-chat-algo-background-workers-call-summary-model`, `cpt-cf-mini-chat-algo-background-workers-commit-summary`, `cpt-cf-mini-chat-state-background-workers-summary-frontier`, `cpt-cf-mini-chat-flow-background-workers-long-conversation-compression` |
| 6 | Worker metrics | `cpt-cf-mini-chat-algo-background-workers-record-metrics` |

**Steps deferred to this feature by earlier features** (implemented here; the code markers use the owning state ID, and the checkbox of the transition in its own document is set when the marker exists):

| Owning element | Transition | Implemented by |
|----------------|------------|----------------|
| `cpt-cf-mini-chat-state-turn-mutations-replacement-setup` | `inst-unreserved-orphaned` (a replacement turn left `running` with NULL reserve fields is finalized without settlement) | `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`, `inst-orphan-unreserved` |
| `cpt-cf-mini-chat-state-attachments-chat-vector-store` | `inst-bound-to-absent` (the chat cleanup handler deletes the provider store of a soft-deleted chat and its row) | `cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store`, `inst-delete-store-row` |
| `cpt-cf-mini-chat-flow-gear-foundation-gear-start` | `inst-prepare-elector` (the Kubernetes Lease elector of `k8s` builds; the foundation shipped the no-op elector in every build) | `cpt-cf-mini-chat-algo-background-workers-elect-leader` |
| `cpt-cf-mini-chat-algo-gear-foundation-start-outbox` | `inst-placeholder-handlers` (the chat-cleanup and thread-summary placeholders are replaced) | `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`, `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary` |
| `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream` (with `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`) | Extension point of the thread-summary trigger (the hook is declared here; `cpt-cf-mini-chat-feature-streaming-turns` is not edited): (a) the ContextPlan outputs of `cpt-cf-mini-chat-algo-streaming-turns-assemble-context` (`inst-assembled-tokens`), namely `assembled_context_tokens`, `messages_truncated`, the effective input budget and `thread_summary_applied`, are carried by `run-provider-task` into the finalization input of `finalize-stream`, which lists only the turn, decision, chat, outcome, text, tool counts and connection state today; the effective budget carried is the `inst-input-limit` result of `assemble-context`, superseding the `min(...)` shorthand of `inst-assembled-tokens`; (b) a trigger-evaluation step is added inside the closure of `inst-run-finalize`, after `inst-enqueue-audit`, with the `system_request_id` precomputed in `inst-precompute`; (c) after the `won` commit (`inst-case-won`) the queue `outbox.thread_summary_queue_name` is woken and `mini_chat_thread_summary_trigger{result}` is recorded | `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`, `inst-return-trigger-result` |

**Out of scope** (owned by other features, or not implemented in P1): the chat soft delete, the `pending` marking of the chat's attachments and the chat-cleanup enqueue (`cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`). The attachment cleanup handler for attachments of live chats, the upload reaper scan and the vector store creation protocol (`cpt-cf-mini-chat-feature-attachments`; this feature only gives the reaper its Lease role). The orphan CAS, settlement, billing outcome derivation, usage event and system-task usage event (`cpt-cf-mini-chat-feature-quota-billing`, called by reference). The stream finalization transaction into which the trigger step is added, the turn audit event shape, the `stream_interrupted` terminal of a CAS loser, context assembly with `assembled_context_tokens` and `messages_truncated`, and the turn status API (`cpt-cf-mini-chat-feature-streaming-turns`). Summary invalidation on retry, edit and delete (`cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`). Not implemented per `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`: hard-purge of soft-deleted rows after a grace period, a chat-deletion audit event, and cancellation of a turn running when its chat is deleted. P2 or later: temporary chat auto-deletion (`cpt-cf-mini-chat-fr-temporary-chat`, `cpt-cf-mini-chat-usecase-temporary-chat-cleanup`), the provider file reconciliation job (DESIGN "Additional invariants" 4) and the summary quality gate with `mini_chat_summary_regen`. Future per `cpt-cf-mini-chat-adr-quota-policy-scope`: kill-switch checks, a tenant operational bucket and audit events for system tasks. The gauge `mini_chat_cleanup_backlog` stays declared but not recorded, and the outbox dead-letter series stay undefined (DESIGN "Metric series"); queue depth, lease churn and dead letters come from the shared outbox metrics surface.

### 1.2 Purpose

Deleting a chat must release the user's files and the chat vector store at the provider without making the user wait, and must keep trying through provider outages until the attempt budget is spent. A pod that crashes mid-stream must not leave a turn `running` forever: that would block the chat (one running turn per chat) and keep a quota reserve unsettled. A long conversation must keep fitting the model's context: older history must be replaced by a summary in the background, without blocking the turn, without billing the user, and without bringing back content that a retry, edit or delete removed. In a replicated deployment, the periodic workers must run on exactly one pod.

**Requirements**: `cpt-cf-mini-chat-fr-chat-deletion-cleanup`, `cpt-cf-mini-chat-fr-thread-summary`, `cpt-cf-mini-chat-fr-quota-billing-architecture`, `cpt-cf-mini-chat-nfr-data-retention`, `cpt-cf-mini-chat-nfr-resilience-recovery`, `cpt-cf-mini-chat-nfr-observability-supportability`

`cpt-cf-mini-chat-fr-chat-deletion-cleanup` is covered for its asynchronous part (provider files, secondary copies, vector store); the synchronous soft delete belongs to `cpt-cf-mini-chat-feature-chat-crud`, and hard-purge is not implemented. `cpt-cf-mini-chat-fr-thread-summary` is covered for the trigger, generation, failure handling and the guarded commit; the invalidation on mutation belongs to `cpt-cf-mini-chat-feature-turn-mutations`. `cpt-cf-mini-chat-fr-quota-billing-architecture` is covered for its orphan-watchdog facet (exactly one terminal billing event per reserved turn, also after a crash) and for the system-task rules of the summary task. `cpt-cf-mini-chat-nfr-observability-supportability` is covered for the worker metrics.

**Principles**: `cpt-cf-mini-chat-principle-summary-over-history`, `cpt-cf-mini-chat-principle-tenant-scoped`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-cleanup-scheduler` | Runs as the chat cleanup outbox handler of this feature: deletes the provider files, secondary copies and the vector store of a soft-deleted chat with retries |
| `cpt-cf-mini-chat-actor-chat-user` | Deletes a chat (through `cpt-cf-mini-chat-feature-chat-crud`); recovers from a dropped stream through the turn status API while the watchdog finalizes the abandoned turn; benefits from summaries in long conversations |
| Orphan watchdog (`cpt-cf-mini-chat-component-orphan-watchdog`) | The leader-only periodic worker of this feature; competes with the stream path for the CAS of a stalled `running` turn |
| ToolKit runtime (gear host) | Hosts the shared outbox that stores, partitions, leases, retries and dead-letters the chat-cleanup and thread-summary messages; runs the background task registry that starts and stops the watchdog and the Lease renew tasks |
| RAG provider (OpenAI or Azure OpenAI Files and Vector Stores APIs, behind OAGW) | Deletes files and vector stores; in tests an OpenAI-compatible mock behind OAGW, or a fake `StorageClient` |
| Anthropic Files API (behind OAGW) | Deletes the secondary copy of an image of an Anthropic chat |
| LLM provider of the summary model (behind OAGW) | Answers the non-streaming summary request; in tests an OpenAI-compatible mock behind OAGW, or a fake `ServiceGatewayClientV1` |
| Model policy plugin (via the model-policy gateway) | Supplies the summary model's catalog entry, the snapshot of `policy_version_applied` for orphan settlement, and the chat model's provider entry for the secondary-copy alias |
| Kubernetes API server (`k8s` builds only) | Stores the `coordination.k8s.io/v1` Lease objects; in tests a fake Lease API port, never a live cluster |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-mini-chat-fr-chat-deletion-cleanup`, `cpt-cf-mini-chat-fr-thread-summary`, `cpt-cf-mini-chat-fr-quota-billing-architecture` ("Background/system task billing rules"), `cpt-cf-mini-chat-nfr-data-retention`, `cpt-cf-mini-chat-nfr-resilience-recovery` ("Turn recovery contract"), `cpt-cf-mini-chat-nfr-observability-supportability`; UC-004 `cpt-cf-mini-chat-usecase-delete-chat` (step 4; step 5 is not implemented)
- **Design**: [DESIGN.md](../DESIGN.md) — §2.1 `cpt-cf-mini-chat-principle-summary-over-history`; §2.2 `cpt-cf-mini-chat-constraint-no-credentials`; §3.2 orphan watchdog `cpt-cf-mini-chat-component-orphan-watchdog`, upload reaper, `cpt-cf-mini-chat-component-llm-provider`, `cpt-cf-mini-chat-component-quota-service`, `cpt-cf-mini-chat-component-chat-store`, "System Task Isolation Invariant (P1)", "System Task Attribution Rules (Normative)", "Gear lifecycle"; §3.6 `cpt-cf-mini-chat-seq-thread-summary` (trigger timing, execution stages, "Stable Range and Commit Invariant"), `cpt-cf-mini-chat-seq-chat-deletion-cleanup` (responsibility boundaries, attachment cleanup state machine, payload and execution semantics, vector store cleanup ordering, additional invariants, observability); §3.7 `chat_turns` (`last_progress_at`, orphan scan index, preflight columns on retry/edit), `attachments`, `thread_summaries` (frontier semantics), `chat_vector_stores` (cleanup invariant); §3.9 "Summary Interaction on Turn Mutation"; §4 "Provider Request Metadata", "Metrics (OTLP) and Alerts (P1)", `cpt-cf-mini-chat-design-turn-lifecycle`, "Orphan Turn Watchdog", "Watchdog Single-Actor Guarantee", "Cleanup on Chat Deletion"; §5.7 "FinalizeTurn Invariant" (orphan watchdog row); §5.8 `cpt-cf-mini-chat-design-nonterminal-reconciliation`; Appendix B.5.5 (summary prompts), B.9.1 to B.9.5
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.7
- **Acceptance checklist**: [acceptance-criteria.md](../acceptance-criteria.md) — owned items: Cleanup & Recovery "Chat deletion triggers reliable background cleanup of provider-side resources" and "Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly" (the mutation side of the second item, the summary deletion in the mutation transaction, is delivered by `cpt-cf-mini-chat-feature-turn-mutations`; this feature delivers the guarded commit and owns the item). The orphan-watchdog facets of Settlement & Finalization "Every terminal outcome settles quota exactly once" (owned by `cpt-cf-mini-chat-feature-quota-billing`) and Turn Lifecycle "Turn state machine ... is consistent end to end" (owned by `cpt-cf-mini-chat-feature-streaming-turns`) are tested here as feature-local items.
- **ADRs**: `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`, `cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`, `cpt-cf-mini-chat-adr-llm-provider-as-library`, `cpt-cf-mini-chat-adr-internal-transport`, `cpt-cf-mini-chat-adr-multi-provider-adapters`
- **Foundation elements reused by reference** (from `cpt-cf-mini-chat-feature-gear-foundation`, not redefined here): gear start `cpt-cf-mini-chat-flow-gear-foundation-gear-start` (`inst-if-leader-workers`, `inst-prepare-elector`, `inst-start-outbox`, `inst-check-summary-model`, `inst-spawn-workers`); gear stop and the background task registry `cpt-cf-mini-chat-flow-gear-foundation-gear-stop`; outbox queues, partitions, leases and the placeholder handlers `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`; event encoding `cpt-cf-mini-chat-algo-gear-foundation-encode-events`; usage delivery `cpt-cf-mini-chat-algo-gear-foundation-handle-usage`; audit delivery `cpt-cf-mini-chat-algo-gear-foundation-handle-audit`; plugin resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`; provider resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`; S2S context from `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw` (`inst-s2s-exchange`); configuration `cpt-cf-mini-chat-algo-gear-foundation-load-config` (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker.max_attempts`, `outbox`); metric instruments `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`; schema `cpt-cf-mini-chat-dod-gear-foundation-schema`
- **Chat CRUD elements reused by reference** (from `cpt-cf-mini-chat-feature-chat-crud`): the chat soft delete and its chat-cleanup payload `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat` (`tenant_id`, `chat_id`, `system_request_id`, `reason` `chat_soft_delete`, `chat_deleted_at`; queue partitioned by `chat_id`)
- **Quota elements reused by reference** (from `cpt-cf-mini-chat-feature-quota-billing`): CAS-guarded finalization `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` (orphan variant: `inst-prefetch-snapshot`, `inst-cas-update`, `inst-if-no-requester`, `inst-if-orphan`, `inst-orphan-inputs`, `inst-if-no-reserve`, `inst-retry-finalize`); billing outcome `cpt-cf-mini-chat-algo-quota-billing-billing-outcome` (`inst-case-orphan`); settlement `cpt-cf-mini-chat-algo-quota-billing-settle` (estimated path and deterministic fallback); turn usage event `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`; system-task usage event `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`; quota metrics and the cost record `cpt-cf-mini-chat-algo-quota-billing-record-metrics`; billing state `cpt-cf-mini-chat-state-quota-billing-turn-billing` (`inst-to-aborted`)
- **Streaming elements reused by reference** (from `cpt-cf-mini-chat-feature-streaming-turns`): stream finalization `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream` (the closure of `inst-run-finalize`, after `inst-enqueue-audit`; `inst-case-lost`); turn audit event `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`; context assembly `cpt-cf-mini-chat-algo-streaming-turns-assemble-context` (`inst-assembled-tokens`); turn inputs `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs` (`inst-select-summary`, `inst-select-recent` with the `is_compressed` filter); provider error classification `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`; error sanitization `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`; turn status `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`; turn lifecycle `cpt-cf-mini-chat-state-streaming-turns-turn-lifecycle` (`inst-to-orphaned`); send flow `cpt-cf-mini-chat-flow-streaming-turns-send-message`
- **Attachment elements reused by reference** (from `cpt-cf-mini-chat-feature-attachments`): storage wire contract and `StorageClient` deletes (file, vector store, secondary copy) and `PROVIDER_DELETE_TIMEOUT` (§1.4 of that feature); attachment cleanup handler `cpt-cf-mini-chat-algo-attachments-handle-cleanup` (steps aside for a deleted chat, `inst-check-chat`); attachment cleanup state `cpt-cf-mini-chat-state-attachments-cleanup-status` (`inst-to-done`, `inst-to-failed` for the chat-cleanup path); vector store state `cpt-cf-mini-chat-state-attachments-chat-vector-store`; vector store creation protocol `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`; upload reaper `cpt-cf-mini-chat-algo-attachments-reap-abandoned` (`inst-scan-loop`)
- **Mutation elements reused by reference** (from `cpt-cf-mini-chat-feature-turn-mutations`): summary invalidation `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`; replacement setup state `cpt-cf-mini-chat-state-turn-mutations-replacement-setup`; setup failure marking `cpt-cf-mini-chat-algo-turn-mutations-fail-setup` (`inst-if-mark-failed-error`); replacement history `cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history`
- **Dependencies**: `cpt-cf-mini-chat-feature-turn-mutations`, `cpt-cf-mini-chat-feature-attachments`, `cpt-cf-mini-chat-feature-quota-billing`

**Source precedence**: DESIGN is authoritative for details. The gear docs win over `config/mini-chat.yaml` and platform docs (DECOMPOSITION task-level overrides). The overrides already recorded by the sibling FEATUREs are kept: the quota helper's handling of a turn without `requester_user_id` (`cpt-cf-mini-chat-feature-quota-billing` §1.4), the durable `attachment_orphan_file` cleanup and the S2S context for calls outside a request (`cpt-cf-mini-chat-feature-attachments` §1.4).

**Queues and payloads** (shared outbox; handlers answer `Ok`, `Retry` or `Reject`; the delivery attempt count `attempts` is 0 on the first delivery, so the delivery number is `attempts` plus 1):

| Queue (default name) | Partition key | Lease | Producer | Payload (JSON object with exactly these keys) | Handler |
|---|---|---|---|---|---|
| `outbox.chat_cleanup_queue_name` (`mini-chat.chat_cleanup`) | `chat_id` | shared default, 30 s | `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat` | `tenant_id`, `chat_id`, `system_request_id` (UUID strings), `reason` (`chat_soft_delete`), `chat_deleted_at` (RFC 3339 UTC) | `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup` |
| `outbox.thread_summary_queue_name` (`mini-chat.thread_summary`) | `chat_id` | `thread_summary_worker.claim_timeout_secs` (300 s) | `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger` | `tenant_id`, `chat_id`, `system_request_id` (UUID strings), `base_frontier_created_at` (RFC 3339 UTC with microseconds, or `null`), `base_frontier_message_id` (UUID string, or `null`; both `null` or both set), `frozen_target_created_at`, `frozen_target_message_id`, `system_task_type` (`thread_summary_update`) | `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary` |

The watchdog enqueues to the existing usage queue (`outbox.queue_name`) and audit queue (`outbox.audit_queue_name`), partitioned by `tenant_id`, through the quota helper and its own closure. The summary commit enqueues the system-task usage event to the usage queue.

**DB writes** (exact; background workers run without a request `SecurityContext`: every statement is bound to the `tenant_id` and `chat_id` of the payload or of the scanned row, through Secure ORM with a tenant scope; the watchdog CAS uses the `allow_all` system boundary of the quota helper, guarded by the turn id from its own scan):

| Worker | Transaction | Table and statement | Values |
|--------|-------------|---------------------|--------|
| Chat cleanup | Autocommit | UPDATE `attachments` (claim) | `cleanup_status` `pending`, `cleanup_updated_at` now WHERE `tenant_id`, `chat_id` match and `cleanup_status IS NULL`, soft-deleted rows included |
| Chat cleanup | Autocommit | UPDATE `attachments` (failed attempt) | `cleanup_attempts` plus 1, `last_cleanup_error` (`http <status>`, `timeout`, `gateway error`, `storage backend not configured`), `cleanup_updated_at` now WHERE the row is `pending` |
| Chat cleanup | Autocommit | UPDATE `attachments` (terminal) | `cleanup_status` `done` or `failed`, `cleanup_updated_at` now WHERE the row is `pending`; `deleted_at`, `status` and the provider ids stay unchanged |
| Chat cleanup | Autocommit | DELETE `chat_vector_stores` | the row WHERE `id`, `tenant_id`, `chat_id` match (after provider 2xx or 404; or a placeholder with `vector_store_id IS NULL`) |
| Orphan watchdog | Finalization (orphan variant) | UPDATE `chat_turns` (CAS) | `state` `failed`, `error_code` `orphan_timeout`, `completed_at` and `updated_at` now WHERE `id`, `state` `running`, `deleted_at IS NULL` and `COALESCE(last_progress_at, started_at)` at or before the cutoff |
| Orphan watchdog | Finalization | UPSERT `quota_usage`; enqueue usage | the estimated settlement and the `aborted` / `estimated` usage event of the quota helper; nothing on `quota_usage` for NULL reserve fields |
| Orphan watchdog | Finalization | Enqueue to `outbox.audit_queue_name` | one `turn_failed` `TurnAuditEvent` |
| Summary trigger | Stream finalization (closure) | Enqueue to `outbox.thread_summary_queue_name` | one work item |
| Summary commit | Summary commit | INSERT or UPDATE `thread_summaries` | new row (`id`, `tenant_id`, `chat_id`, `summary_text`, frontier, `token_estimate`, `created_at`, `updated_at`) or the CAS UPDATE of `summary_text`, frontier, `token_estimate`, `updated_at` WHERE the frontier equals the base |
| Summary commit | Summary commit | UPDATE `messages` | `is_compressed` `true` for the live, non-compressed messages of the frozen range |
| Summary commit | Summary commit | Enqueue to `outbox.queue_name` | one system-task `UsageEvent` |

No worker writes `messages` content, `chat_turns` outside the watchdog CAS, or `chats`.

**Transaction discipline** (normative): the watchdog uses the finalization transaction of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` (write-first CAS, canonical `quota_usage` order, 3 attempts with 20 ms / 100 ms backoff on `SQLITE_BUSY`, PostgreSQL `40001` / `40P01`). The settlement snapshot is fetched before it opens. The trigger statements run inside the finalization closure of `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`, after the audit enqueue, with the `system_request_id` precomputed outside the closure. The summary commit is write-first: on SQLite it is begun `IMMEDIATE`, and on PostgreSQL its first statement locks the frozen target message row (`SELECT ... FOR UPDATE`). It is retried like the finalization, with a deterministic closure (new summary id, `now`, usage payload captured by value). The chat cleanup writes are single autocommit statements guarded by `cleanup_status` `pending`, so a redelivery or a concurrent attachment cleanup is harmless. No provider, plugin or Kubernetes call runs inside any transaction.

**Feature constants**:

| Constant | Value | Use |
|----------|-------|-----|
| `ORPHAN_SCAN_BATCH` | 100 | Candidates per watchdog scan (DESIGN, not configurable) |
| `CHAT_CLEANUP_DELIVERY_BUDGET` | 25 s | Time after the delivery start during which the chat cleanup handler starts provider calls; below the 28 s cancel point of the 30 s lease with 2 s headroom |
| `CHAT_CLEANUP_CONCURRENCY` | 4 | Attachments of one chat processed concurrently within a delivery |
| `SUMMARY_COMMIT_RESERVE` | 10 s | Time kept free before the end of the thread-summary lease for the commit |
| `SUMMARY_RESPONSE_MAX_BYTES` | 1 MiB | Maximum summary response body read |
| `LEASE_DURATION` | 15 s | `leaseDurationSeconds` of each Lease (DESIGN, hardcoded) |
| `LEASE_RENEW_PERIOD` | 2 s | Renew and acquire cadence (DESIGN, hardcoded) |
| `LEASE_RENEW_DEADLINE` | 10 s | Leadership is reported only while the last successful renewal is younger than this |
| `LEASE_API_TIMEOUT` | 5 s | Each Kubernetes API call |
| `LEASE_PREFIX` | `mini-chat` | Lease names `mini-chat-orphan-watchdog`, `mini-chat-upload-reaper` (hardcoded) |

**External-call timeouts** (every call to a dependency that can stall is bounded):

| Constant | Value | Call in this feature | Outcome on timeout |
|----------|-------|----------------------|--------------------|
| `PROVIDER_DELETE_TIMEOUT` (attachments constant) | 10 s, lowered to the time left of `CHAT_CLEANUP_DELIVERY_BUDGET` | File, secondary and vector store deletes of the chat cleanup handler | A call cut by `PROVIDER_DELETE_TIMEOUT` is a failed attempt; a call cut by the delivery budget is not counted and the message is retried |
| `POLICY_PLUGIN_CALL_TIMEOUT`, `REGISTRY_QUERY_TIMEOUT` | 10 s each | Summary model resolution; secondary alias resolution of the chat cleanup handler; orphan settlement snapshot prefetch | Summary: `Retry`; secondary: the secondary delete is skipped and counted; orphan: the snapshot is `unavailable` and settlement takes the deterministic fallback |
| Summary call deadline | delivery start plus `thread_summary_worker.claim_timeout_secs` minus `SUMMARY_COMMIT_RESERVE` (290 s with defaults, 20 s at the 30 s minimum); OAGW upstream timeouts apply as well | The summary provider call, including the context-length retries | `provider_error`, previous summary kept, `Retry` |
| `LEASE_API_TIMEOUT` | 5 s | Kubernetes Lease GET, CREATE, REPLACE | The round counts as not renewed; leadership lapses after `LEASE_RENEW_DEADLINE` |

**Feature-local deviations and interpretations** (review owner: mini-chat gear maintainers):

| Topic | Deviation or interpretation | Rationale |
|-------|-----------------------------|-----------|
| Leader election implementation | The Kubernetes Lease elector uses the Lease API of the `kube` client already declared as an optional dependency behind the `k8s` feature (`kube`, `k8s-openapi` are in `Cargo.lock`; no dependency is added). Expiry is judged by the observed-time rule: a pod takes over only when the Lease record has not changed for `LEASE_DURATION` on its own monotonic clock. Updates use the Lease `resourceVersion` (optimistic concurrency) | DESIGN fixes the Lease names, 15 s / 2 s and runtime creation but not the expiry rule; the observed-time rule is immune to clock skew between pods. The decision logic sits behind a Lease API port, so it is tested without a cluster and without network |
| Leadership per role | The elector answers per role (`orphan-watchdog`, `upload-reaper`), one Lease and one renew task per enabled role; the reaper asks for its role in `inst-scan-loop` of `cpt-cf-mini-chat-algo-attachments-reap-abandoned` | DESIGN B.9.1: leadership is held per role with separate Leases; the foundation trait had no role |
| Missing pod identity | In a `k8s` build with a leader-only worker enabled, a missing or empty `POD_NAMESPACE` or `POD_NAME`, or a failed in-cluster client build, fails `start` naming the cause | DESIGN says the elector requires both variables; failing fast prevents a silent multi-leader setup |
| Kubernetes API errors | A failed or timed-out Lease call makes the round unrenewed; the pod keeps leadership only until `LEASE_RENEW_DEADLINE` (10 s) since its last successful renewal, which is shorter than the 15 s another pod waits | Prevents two leaders without a fencing token; double processing is prevented by the CAS guards in any case (`cpt-cf-mini-chat-adr-runtime-consistency-limitations`) |
| Chat cleanup delivery budget | A delivery starts provider calls only within `CHAT_CLEANUP_DELIVERY_BUDGET` (25 s) and runs at most `CHAT_CLEANUP_CONCURRENCY` (4) attachments at a time; pending attachments not reached are left for the next delivery (`Retry`) without counting an attempt | The chat cleanup queue keeps the shared 30 s lease (DESIGN B.9.3); a handler cancelled at the lease's cancel point would leave its outcome unrecorded |
| Straggler attachments | The handler first sets `cleanup_status` `pending` on attachments of the deleted chat that still have none (an upload that inserted its row while the chat was being deleted) | DESIGN: chat cleanup exclusively owns provider cleanup of a deleted chat; the upload's CAS (`cleanup_status IS NULL`) then fails and its durable `attachment_orphan_file` message deletes a file it stored meanwhile |
| Placeholder vector store row | A `chat_vector_stores` row whose `vector_store_id` is NULL is deleted without a provider call | No provider store is recorded; a creator still running loses its CAS and deletes its new store best effort (`cpt-cf-mini-chat-algo-attachments-ensure-vector-store`) |
| Secondary alias resolution | The Anthropic alias is resolved at handling time from the current snapshot: the chat model's catalog entry, its provider entry of kind `anthropic_messages`, and its alias for the tenant. When that fails, the secondary delete is skipped, logged and counted in `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}` | DESIGN deletes the secondary copy "when the upstream alias could be resolved at delete time"; the chat-cleanup payload carries no alias, and the chat delete of `cpt-cf-mini-chat-feature-chat-crud` resolves none |
| Unknown or active chat | A chat-cleanup message whose chat row does not exist is `Reject` (reason `chat not found`), like an active chat (reason `chat is not soft-deleted`) | DESIGN names only the active chat; a missing row cannot become eligible later |
| Failed-attachment debt metric | `mini_chat_cleanup_vector_store_with_failed_attachments` is recorded before each vector store delete call made while at least one attachment of the chat is `failed` | DESIGN: "MUST emit ... before proceeding with vector-store deletion"; a retried delete is counted again |
| Summary for a deleted chat | A thread-summary work item of a soft-deleted or missing chat ends `Ok` without a provider call and without a metric | The chat and its summary are unreachable; a provider call would only cost money |
| Non-streaming summary call | The summary request is a non-streaming request in the wire format of the summary model's provider kind (one system prompt, one user prompt, no tools); each adapter kind gains a request builder and a response reader for it | DESIGN §3.6 sequence: "non-streaming summary call"; the turn adapters only stream |
| Prompt fitting and the compressed range | Messages dropped from the prompt by the budget fitting or the context-length retries are still marked `is_compressed` by the commit, because the commit covers exactly the frozen range `(base, target]` | DESIGN: the commit marks "exactly that summarized range" and advances the frontier to the frozen target |
| Summary commit lock | The commit transaction locks the frozen target message first (`IMMEDIATE` on SQLite, `SELECT ... FOR UPDATE` on PostgreSQL) and checks `deleted_at IS NULL` before the summary write | DESIGN §3.9 "Summary Interaction on Turn Mutation"; the mutation soft-deletes its messages first, so whichever commits second sees the other's result |
| Trigger inputs | The "summary exists" condition uses the summary loaded for context assembly of the causing turn; the base frontier written to the payload is re-read inside the finalization transaction; the frozen target is the latest live message of the chat whose `request_id` is not the causing turn's | DESIGN: trigger from the assembled context of the current turn; scheduling in the transaction that makes the turn durable |
| Unreserved orphan | A turn left `running` with NULL reserve fields gets no settlement but still its usage event (`aborted`, `estimated`, 0 credits, `usage` `null`, empty models, `policy_version_applied` 0) and its `turn_failed` audit event with empty model fields | DESIGN §3.7 "Preflight columns on retry/edit" and §5.7; `inst-if-no-reserve` of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` |
| Orphan audit event fields | `selected_model` and `effective_model` are the persisted `effective_model` (empty string when NULL), `usage` is 0 and 0, `latency_ms` runs from `started_at` to the finalization time, `tool_calls` come from `web_search_completed_count` and `file_search_completed_count`, `policy_decisions.quota.decision` is `unknown` | DESIGN §3.2 "selected_model + effective_model" and `cpt-cf-mini-chat-adr-runtime-consistency-limitations` "Separate finalization paths"; no in-memory turn state survives a crash |
| Snapshot fetch per scan | The watchdog fetches the snapshot of each distinct `(tenant_id, requester_user_id, policy_version_applied)` at most once per scan; a failed or timed-out fetch is not remembered, so the next candidate of the same key fetches again (one bounded plugin call per candidate) | The scan crosses tenants and the plugin call is made for `(user_id, policy_version)`, so a version alone never identifies a snapshot (DESIGN 5.2: identical only within a tenant for a given version); bounds plugin calls of a scan of 100 candidates; a snapshot is immutable, a failure is transient |
| Large chats and the vector store attempt budget (residual risk) | All deliveries of a chat-cleanup message count toward `cleanup_worker.max_attempts` for the vector store delete (DESIGN), including deliveries spent on attachments; a chat with many attachments can reach the vector store step with few or no retries left, so one failed store delete dead-letters the message. Owner: mini-chat gear maintainers | DESIGN fixes the counting rule; the row is kept and a dead-letter replay retries the delete |
| Startup window of the gear | The gear binds its runtime (background task registry and S2S context) before the outbox pipeline starts delivering. Should a handler still run unbound, the chat-cleanup and thread-summary handlers answer a plain `Retry` after their chat-state checks, with no metric, no attachment attempt and no dead letter | Every delivery spends an outbox attempt, so an unbound delivery must not be a counted failure |

**Audit**: the orphan watchdog enqueues one `turn_failed` `TurnAuditEvent` per won orphan finalization, in the finalization transaction. The summary task emits no audit event, and chat cleanup emits none (`cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`, `cpt-cf-mini-chat-adr-quota-policy-scope`).

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-mini-chat-usecase-delete-chat` (step 4: the cleanup worker deletes the chat's vector store and provider files with idempotent retries; steps 1 to 3 belong to `cpt-cf-mini-chat-feature-chat-crud`; step 5 is not implemented)

### Chat Deletion Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup`

**Actor**: `cpt-cf-mini-chat-actor-cleanup-scheduler`

**Success Scenarios**:
- After `DELETE /v1/chats/{id}` committed, the shared outbox delivers the chat-cleanup message. Every `pending` attachment of the chat gets its provider file (and its Anthropic secondary copy) deleted and its `cleanup_status` set to `done`, then the chat vector store is deleted at the provider and its row removed. The message is acknowledged, and the provider holds nothing of the chat any more.
- A provider answering 404 for an already deleted file or store counts as success; a redelivery after completion makes no provider call.

**Error Scenarios**:
- A provider file delete fails: the attachment stays `pending` with `cleanup_attempts` incremented and the message is retried with backoff; after `cleanup_worker.max_attempts` failed attempts the attachment becomes `failed` (provider-file debt) and the store is still released.
- The vector store delete fails: the message is retried until the delivery that reaches `cleanup_worker.max_attempts`, then dead-lettered with the row kept; a replay retries the delete.
- A database error: `Retry` without counting an attempt. A malformed payload, a missing chat or an active chat: `Reject` (dead letter).

**Steps**:
1. [x] - `p1` - The shared outbox delivers one message of `outbox.chat_cleanup_queue_name` to its partition (`chat_id`) under the 30 s lease; messages of one chat are processed one after the other - `inst-deliver-chat-cleanup`
2. [x] - `p1` - Run the chat cleanup handler (see `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`) - `inst-run-chat-cleanup`
3. [x] - `p1` - **MATCH** the handler result - `inst-match-chat-cleanup-result`
   - [x] - `p1` - **CASE** `Ok`: the outbox acknowledges the message; the chat's provider resources are released (fully purged unless an attachment ended `failed`) - `inst-case-cleanup-ok`
   - [x] - `p1` - **CASE** `Retry`: the outbox redelivers the same message later with its backoff; the same `system_request_id` is kept - `inst-case-cleanup-retry`
   - [x] - `p1` - **CASE** `Reject`: the outbox dead-letters the message; an operator can replay it after fixing the cause, which re-runs the handler against the persisted attachment states and the kept `chat_vector_stores` row - `inst-case-cleanup-reject`
4. [x] - `p1` - **RETURN** the outcome; the user saw 204 at delete time and is never blocked by this flow - `inst-return-chat-cleanup`

### Abandoned Turn Recovery

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The pod serving a stream crashes; the client sees the SSE connection drop without `done` or `error`. Turn status reports `running` until the watchdog finalizes the turn. From then on it reports `error` with `error_code` `orphan_timeout`; the reserve is settled with the bounded estimate and exactly one usage event and one `turn_failed` audit event exist. The user sends a new message with a new `request_id`, and it is accepted.

**Error Scenarios**:
- Before the watchdog finalizes the turn, a new send to the chat gets 409 `turn_already_running`, and a mutation of the turn gets 400 `turn_state`.
- Reusing the abandoned `request_id` after finalization gets 409 `request_id_conflict`.

**Steps**:
1. [x] - `p1` - The pod serving the stream stops; the `chat_turns` row stays `running` with its last `last_progress_at` and its reserve unsettled, and the client receives no terminal event - `inst-stream-lost`
2. [x] - `p1` - User calls `GET /v1/chats/{id}/turns/{request_id}` (see `cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`); it returns `state` `running`; the client shows "still generating" and does not retry automatically - `inst-poll-running`
3. [x] - `p1` - Within `orphan_watchdog.timeout_secs` plus at most one `orphan_watchdog.scan_interval_secs` after the last progress (plus at most one leader hand-over of `LEASE_DURATION` when the crashed pod was the leader), the leader's watchdog scan finds the turn and finalizes it (see `cpt-cf-mini-chat-algo-background-workers-run-watchdog`) - `inst-watchdog-finalizes`
4. [x] - `p1` - User calls the turn status again; it returns `state` `error` with `error_code` `orphan_timeout` and no `assistant_message_id` - `inst-poll-error`
5. [x] - `p1` - User sends a new message with a new `request_id` (see `cpt-cf-mini-chat-flow-streaming-turns-send-message`); it is accepted because no `running` turn remains - `inst-send-after-recovery`
6. [x] - `p1` - **RETURN** the recovered conversation: no partial assistant message was persisted, the turn was billed `aborted` / `estimated` once, and the chat is unblocked - `inst-return-recovered`

### Long Conversation Compression

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-background-workers-long-conversation-compression`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A user's conversation grows until context assembly reaches the compression threshold (or truncates). The completed turn's `done` is sent as usual, and a summary work item is committed with the turn. The handler summarizes the older messages, marks them compressed and advances the frontier. The next turn's context starts with the summary item followed by the uncompressed recent messages, and `stream_started.thread_summary_applied` is present.

**Error Scenarios**:
- The summary call fails or returns no summary: the previous context is used unchanged; the work item is retried and dead-lettered after `thread_summary_worker.max_attempts` deliveries.
- The user retries, edits or deletes a turn the summary covers: the mutation deletes the summary (see `cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`), and a summary commit that races it does not bring the removed content back.

**Steps**:
1. [x] - `p1` - User sends a message (see `cpt-cf-mini-chat-flow-streaming-turns-send-message`); context assembly computes `assembled_context_tokens`, `messages_truncated` and the effective budget (`inst-assembled-tokens`) - `inst-send-long-turn`
2. [x] - `p1` - The turn completes; in its finalization transaction the trigger is evaluated and, when it fires, the work item is enqueued (see `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`); `done` is sent after the commit, unchanged by the trigger - `inst-trigger-on-finalize`
3. [x] - `p1` - The shared outbox delivers the work item to the thread summary handler (see `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`), which summarizes and commits asynchronously - `inst-summary-runs`
4. [x] - `p1` - User sends the next message; turn inputs read the committed summary and only the non-compressed messages after its frontier (`inst-select-summary`, `inst-select-recent`), and context assembly prepends the summary item - `inst-next-turn-uses-summary`
5. [x] - `p1` - **RETURN** the stream of the next turn with `stream_started.thread_summary_applied` set to the summary's `token_estimate`; the compressed messages stay visible in the message list - `inst-return-compressed-turn`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Chat Cleanup Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`

**Input**: One message of `outbox.chat_cleanup_queue_name` (payload and `attempts`), the delivery start instant, the S2S security context, `cleanup_worker.max_attempts`, the validated provider entries and the storage directory

**Output**: `Ok`, `Retry` or `Reject` for the shared outbox

**Steps**:
1. [x] - `p1` - At `start`, register this handler for `outbox.chat_cleanup_queue_name` in place of the foundation placeholder (see `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`, `inst-placeholder-handlers`); messages held by the placeholder before this feature are processed now, none is lost - `inst-register-chat-cleanup-handler`
2. [x] - `p1` - Set the delivery number to `attempts` plus 1 and the delivery deadline to the delivery start plus `CHAT_CLEANUP_DELIVERY_BUDGET` - `inst-delivery-budget`
3. [x] - `p1` - Decode the payload as the chat-cleanup payload of `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`; **IF** a key is missing or unknown, an id is not a UUID, `chat_deleted_at` is not RFC 3339 or `reason` is not `chat_soft_delete`, log the partition and the decode error and **RETURN** `Reject` - `inst-decode-chat-cleanup`
4. [x] - `p1` - DB: SELECT `chats` WHERE `tenant_id` and `id` match the payload, soft-deleted rows included (system read with the payload tenant scope); **IF** the read fails, **RETURN** `Retry` (no limit) - `inst-load-deleted-chat`
5. [x] - `p1` - **IF** no row exists, log the ids and **RETURN** `Reject` with reason `chat not found` - `inst-if-chat-missing`
6. [x] - `p1` - **IF** the chat's `deleted_at` is NULL, log the ids and **RETURN** `Reject` with reason `chat is not soft-deleted`; nothing of an active chat is touched - `inst-if-chat-active`
7. [x] - `p1` - DB: UPDATE `attachments` SET `cleanup_status` `pending`, `cleanup_updated_at` now WHERE `tenant_id` and `chat_id` match and `cleanup_status IS NULL`, soft-deleted rows included; **IF** it fails, **RETURN** `Retry` - `inst-claim-stragglers`
8. [x] - `p1` - DB: SELECT `attachments` WHERE `tenant_id` and `chat_id` match and `cleanup_status` is `pending`, soft-deleted rows included, ordered by `created_at` and then `id`; **IF** the read fails, **RETURN** `Retry` - `inst-select-pending-attachments`
9. [x] - `p1` - **IF** a selected row has `secondary_status` `uploaded` and a `secondary_file_id`, resolve the Anthropic secondary target once for the delivery: read the current policy snapshot through the model-policy gateway, find `chats.model` in `model_catalog`, take its provider entry and, when its kind is `anthropic_messages`, that entry's alias for the payload tenant; each call is bounded by `POLICY_PLUGIN_CALL_TIMEOUT` and by the delivery deadline; **IF** any part fails or the kind differs, there is no secondary target and a warning names the chat - `inst-resolve-secondary-target`
10. [x] - `p1` - **FOR EACH** selected row in that order, starting no new row after the delivery deadline and with at most `CHAT_CLEANUP_CONCURRENCY` rows in flight, clean the row (see `cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment`) - `inst-for-each-pending`
11. [x] - `p1` - **IF** a database statement of step 10 failed, **RETURN** `Retry` (no attempt is counted for the database failure) - `inst-if-attachment-db-error`
12. [x] - `p1` - DB: SELECT whether any attachment of the chat (soft-deleted rows included) is still `pending`, and whether any is `failed`; **IF** the read fails, **RETURN** `Retry` - `inst-recheck-terminal`
13. [x] - `p1` - **IF** one is still `pending` (a failed attempt below the limit, or a row not reached before the delivery deadline), **RETURN** `Retry`; the vector store is not touched while any attachment of the chat is `pending` - `inst-if-still-pending`
14. [x] - `p1` - Clean the chat vector store with the delivery number, the `failed` flag and the delivery deadline (see `cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store`) and **RETURN** its result - `inst-run-vector-store-cleanup`

### Chat Attachment Provider Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment`

**Input**: One `pending` attachment row of the deleted chat, the secondary target or none, the S2S security context, `cleanup_worker.max_attempts`, the delivery deadline

**Output**: `done`, `failed`, `still_pending`, `deferred` (cut by the delivery deadline), or a database error

**Steps**:
1. [x] - `p1` - **IF** the row's `provider_file_id` is NULL (the upload never reached the provider), **GO TO** step 6 - `inst-if-no-provider-file`
2. [x] - `p1` - Find the storage target whose storage backend label equals the row's `storage_backend` (configuration order) with its alias for the row's tenant; API: `proxy_request` with the S2S context and the file delete request `DELETE /{alias}{prefix}/files/{provider_file_id}`, bounded by `PROVIDER_DELETE_TIMEOUT` lowered to the time left until the delivery deadline - `inst-delete-chat-file`
3. [x] - `p1` - **IF** the call was cut by the delivery deadline before `PROVIDER_DELETE_TIMEOUT` elapsed, **RETURN** `deferred`; the row stays `pending` and no attempt is counted - `inst-if-cut-by-deadline`
4. [x] - `p1` - **IF** no entry carries the label, or the call fails, times out or answers neither 2xx nor 404 (any 5xx and any other 4xx alike) - `inst-if-chat-file-failed`
   1. [x] - `p1` - DB: UPDATE `attachments` SET `cleanup_attempts` plus 1, `last_cleanup_error` the short sanitized cause, `cleanup_updated_at` now WHERE `id`, `tenant_id` and `chat_id` match and `cleanup_status` is `pending`; **IF** it fails, **RETURN** database error - `inst-record-chat-file-attempt`
   2. [x] - `p1` - **IF** `cleanup_attempts` reached `cleanup_worker.max_attempts`, DB: UPDATE `cleanup_status` `failed` and `cleanup_updated_at` now WHERE the row is `pending`, record `mini_chat_cleanup_failed{resource_type="file"}` and **RETURN** `failed`; the handler continues with the other rows (terminal provider-file debt) - `inst-if-chat-file-exhausted`
   3. [x] - `p1` - **ELSE** record `mini_chat_cleanup_retry{resource_type="file",reason="provider_error"}` and **RETURN** `still_pending` - `inst-chat-file-retry`
5. [x] - `p1` - **IF** the row has `secondary_status` `uploaded` and a `secondary_file_id`: **IF** a secondary target exists, API: `proxy_request` with the secondary delete request `DELETE /{alias}/v1/files/{secondary_file_id}` and the headers `anthropic-version: 2023-06-01` and `anthropic-beta: files-api-2025-04-14`, bounded like step 2, where 2xx and 404 are success; **IF** there is no target or the call fails, log the attachment id with the sanitized cause and record `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}`; the secondary result never blocks step 6 - `inst-delete-chat-secondary`
6. [x] - `p1` - DB: UPDATE `attachments` SET `cleanup_status` `done`, `cleanup_updated_at` now WHERE `id`, `tenant_id` and `chat_id` match and `cleanup_status` is `pending`; **IF** it fails, **RETURN** database error (the repeated delete of a redelivery is harmless, 404 counts as success) - `inst-mark-chat-file-done`
7. [x] - `p1` - Record `mini_chat_cleanup_completed{resource_type="file"}` when a provider file was deleted and **RETURN** `done` - `inst-return-chat-file-done`

### Chat Vector Store Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store`

**Input**: The payload tenant and chat, the delivery number, whether any attachment of the chat is `failed`, `cleanup_worker.max_attempts`, the S2S security context, the delivery deadline

**Output**: `Ok`, `Retry` or `Reject`

**Steps**:
1. [x] - `p1` - DB: SELECT `chat_vector_stores` WHERE `tenant_id` and `chat_id` match (never by `vector_store_id` alone); **IF** the read fails, **RETURN** `Retry` - `inst-select-chat-store`
2. [x] - `p1` - **IF** no row exists, **RETURN** `Ok`; the provider holds no store of the chat (a redelivery after completion lands here without any provider call) - `inst-if-no-store`
3. [x] - `p1` - **IF** the row's `vector_store_id` is NULL (a placeholder), DB: DELETE the row WHERE `id`, `tenant_id`, `chat_id` match and `vector_store_id IS NULL`, and **RETURN** `Ok` (or `Retry` when the delete fails); no provider call is made - `inst-delete-placeholder-row`
4. [x] - `p1` - **IF** less than `PROVIDER_DELETE_TIMEOUT` is left until the delivery deadline, **RETURN** `Retry` without a delete call - `inst-if-no-time-for-store`
5. [x] - `p1` - **IF** an attachment of the chat is `failed`, record `mini_chat_cleanup_vector_store_with_failed_attachments`; the store is released anyway, but the chat is not fully purged from provider file storage - `inst-record-failed-debt`
6. [x] - `p1` - Find the storage target whose label equals the row's `provider` with its alias for the tenant; API: `proxy_request` with the S2S context and `DELETE /{alias}{prefix}/vector_stores/{vector_store_id}`, bounded by `PROVIDER_DELETE_TIMEOUT` - `inst-delete-provider-store`
7. [x] - `p1` - **IF** no entry carries the label, or the call fails, times out or answers neither 2xx nor 404 - `inst-if-store-delete-failed`
   1. [x] - `p1` - Log the chat id with the sanitized cause and record `mini_chat_cleanup_retry{resource_type="vector_store",reason="vector_store_delete_failed"}` - `inst-record-store-retry`
   2. [x] - `p1` - **IF** the delivery number is at least `cleanup_worker.max_attempts`, record `mini_chat_cleanup_failed{resource_type="vector_store"}` and **RETURN** `Reject` with reason `vector store delete: max attempts (N) reached` (N the configured limit); the row is kept so a dead-letter replay retries the delete - `inst-if-store-exhausted`
   3. [x] - `p1` - **ELSE** **RETURN** `Retry`; the row is kept - `inst-return-store-retry`
8. [x] - `p1` - DB: DELETE the `chat_vector_stores` row WHERE `id`, `tenant_id` and `chat_id` match; this is the durable completion marker and the transition `inst-bound-to-absent` of `cpt-cf-mini-chat-state-attachments-chat-vector-store`; **IF** it fails, **RETURN** `Retry` (the repeated provider delete answers 404, which is success) - `inst-delete-store-row`
9. [x] - `p1` - Record `mini_chat_cleanup_completed{resource_type="vector_store"}` and **RETURN** `Ok` - `inst-return-store-ok`

### Orphan Watchdog Loop

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-run-watchdog`

**Input**: `orphan_watchdog` (`enabled`, `timeout_secs`, `scan_interval_secs`, read once at `init`), the leader elector, the gear stop token, the application UTC clock

**Output**: Stale `running` turns finalized as `failed` / `orphan_timeout`; nothing returned

**Steps**:
1. [x] - `p1` - At `start`, **IF** `orphan_watchdog.enabled` is `true`, spawn the watchdog as the leader-only worker `orphan-watchdog` on the gear's background task registry after the outbox pipeline (see `cpt-cf-mini-chat-flow-gear-foundation-gear-start`, `inst-spawn-workers`); it ends when the stop token is cancelled and is joined by `cpt-cf-mini-chat-flow-gear-foundation-gear-stop` - `inst-spawn-watchdog`
2. [x] - `p1` - **WHILE** the stop token is not cancelled, wait `scan_interval_secs` (the wait ends early on cancellation), then **IF** the elector reports leadership for the role `orphan-watchdog` (Lease `mini-chat-orphan-watchdog`), run one scan; otherwise skip the round - `inst-watchdog-loop`
3. [x] - `p1` - Start the scan timer, take `now` from the application clock and set the cutoff to `now` minus `timeout_secs`; the same cutoff is used by the scan and by every CAS of this scan - `inst-watchdog-cutoff`
4. [x] - `p1` - DB: SELECT at most `ORPHAN_SCAN_BATCH` `chat_turns` rows across tenants (system read through the partial index on `last_progress_at` WHERE `state` is `running` and `deleted_at IS NULL`) WHERE `state` is `running`, `deleted_at IS NULL` and either `last_progress_at` is at or before the cutoff or `last_progress_at` is NULL and `started_at` is at or before the cutoff; ordered by `last_progress_at` ascending with NULL first, then `started_at`, then `id`; reading `id`, `tenant_id`, `chat_id`, `request_id`, `requester_user_id`, `effective_model`, `policy_version_applied`, `started_at`, `web_search_completed_count` and `file_search_completed_count`; **IF** the read fails, log it and end the scan - `inst-select-orphans`
5. [x] - `p1` - **FOR EACH** candidate in that order - `inst-for-each-orphan`
   1. [x] - `p1` - Record `mini_chat_orphan_detected{reason="stale_progress"}` - `inst-record-detected`
   2. [x] - `p1` - **IF** the stop token is cancelled or the elector no longer reports leadership for `orphan-watchdog`, end the scan; the remaining candidates are left to the next leader scan - `inst-if-leadership-lost`
   3. [x] - `p1` - Finalize the candidate (see `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`); an error is logged and the scan continues with the next candidate - `inst-finalize-candidate`
6. [x] - `p1` - Record `mini_chat_orphan_scan_duration_seconds`; the candidates beyond the batch are picked up by later scans - `inst-record-scan-duration`
7. [x] - `p1` - The candidate list is advisory and never authorizes a finalization by itself; only the orphan CAS does, and the scan never blocks a user-visible request - `inst-scan-advisory`

### Orphan Turn Finalization

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`

**Input**: One candidate row of the scan, the scan's cutoff, the per-scan snapshot cache, the application clock

**Output**: `won`, `lost` or an error

**Transaction discipline** (normative): see §1.4. The transaction is the finalization transaction of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn` in its `orphan` variant, opened through `SeaQuotaRepository::finalize_turn_with`; its first statement is the orphan CAS, and the audit enqueue below runs in its closure after the settlement and the usage enqueue.

**Steps**:
1. [x] - `p1` - **IF** the candidate's `policy_version_applied` is not NULL and the scan holds no snapshot of that tenant, user and version yet, fetch it before any transaction opens (see `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`, `inst-prefetch-snapshot`), at most once per distinct `(tenant, user, version)` per scan; a failed or timed-out fetch marks this candidate's snapshot `unavailable`, which selects the deterministic estimated fallback of the settlement, and is not cached for the rest of the scan (the next candidate of the same key fetches again) - `inst-prefetch-orphan-snapshot`
2. [x] - `p1` - Build the orphan finalization request: the turn id, variant `orphan` with the scan's cutoff, classification `failed` with `orphan_timeout`, terminal columns `state` `failed`, `error_code` `orphan_timeout`, `error_detail`, `assistant_message_id` and `provider_response_id` NULL, no usage, the snapshot, and `now` - `inst-build-orphan-request`
3. [x] - `p1` - Precompute the `TurnAuditEvent` from the candidate row (see `cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event` for the shape and the empty content fields): `event_type` `turn_failed`, `tenant_id`, `requester_user_id`, `chat_id`, `turn_id`, `request_id`, `selected_model` and `effective_model` the persisted `effective_model` (empty string when NULL), `usage` 0 and 0, `latency_ms` from `started_at` to `now`, `tool_calls` `web_search` the `web_search_completed_count` and `file_search` the `file_search_completed_count`, `policy_decisions.quota.decision` `unknown`, `timestamp` `now`; the closure captures it by value - `inst-precompute-orphan-audit`
4. [x] - `p1` - Run the CAS-guarded finalization with the closure (see `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`): the orphan CAS WHERE `id` is the turn, `state` is `running`, `deleted_at IS NULL` and `last_progress_at` (or `started_at` when NULL) is at or before the cutoff, then the turn load, the derivation `failed` / `aborted` / `estimated` and the orphan inputs (`inst-orphan-inputs`) - `inst-run-orphan-finalize`
   1. [x] - `p1` - **IF** the six preflight columns are set, the helper settles on the estimated path: charged tokens the smaller of `reserve_tokens` and `estimated_input_tokens` plus `minimal_generation_floor_applied`, credits from `estimated_input_tokens` and the floor (or the deterministic fallback), released reserve, and enqueues the usage event with `billing_outcome` `aborted` and `settlement_method` `estimated` - `inst-orphan-bounded-debit`
   2. [x] - `p1` - **IF** any preflight column is NULL (a retry or edit turn left `running` before its reserve), the helper skips settlement, changes no `quota_usage` row and enqueues the usage event with `aborted`, `estimated`, `actual_credits_micro` 0, `usage` `null`, empty models and `policy_version_applied` 0 (`inst-if-no-reserve`); this is the transition `inst-unreserved-orphaned` of `cpt-cf-mini-chat-state-turn-mutations-replacement-setup` - `inst-orphan-unreserved`
   3. [x] - `p1` - In the closure, **IF** the helper enqueued a usage event, DB: enqueue the precomputed audit payload to `outbox.audit_queue_name` partitioned by `tenant_id` - `inst-enqueue-orphan-audit`
   4. [x] - `p1` - **IF** the helper took its corrupt-data branch (`requester_user_id` NULL, `inst-if-no-requester`), enqueue no audit event either: the turn is still finalized so it is not rescanned forever, and the case is logged at `error` by the helper - `inst-orphan-no-requester`
   5. [x] - `p1` - Insert or update no `messages` row and evaluate no summary trigger: the turn is not completed and its partial output is lost (DESIGN "Non-Goals (P1)") - `inst-orphan-no-messages`
5. [x] - `p1` - **MATCH** the result - `inst-match-orphan-result`
   - [x] - `p1` - **CASE** `lost` (already finalized by the stream path or the setup-failure mark, soft-deleted, or progress refreshed after the scan): no settlement, no outbox message and no `mini_chat_orphan_finalized`; **RETURN** `lost` - `inst-case-orphan-lost`
   - [x] - `p1` - **CASE** `won`: after the commit the usage and audit queues are woken; record `mini_chat_orphan_finalized{reason="stale_progress"}`, `mini_chat_streams_aborted{trigger="orphan_timeout"}` and the settlement metrics and cost record of `cpt-cf-mini-chat-algo-quota-billing-record-metrics`; log `turn finalized by orphan watchdog` at `warn` with the chat, turn and request ids; **RETURN** `won` - `inst-case-orphan-won`
   - [x] - `p1` - **CASE** error (database error, or the 3 conflict attempts of `inst-retry-finalize` exhausted): log at `error` with the ids; nothing is committed and the turn stays `running` for the next scan; **RETURN** the error - `inst-case-orphan-error`
6. [x] - `p1` - A stream path of the same turn that finishes later loses its CAS (`inst-case-lost` of `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`) and its relay ends with `stream_interrupted`; a late provider result never reopens or overwrites the turn - `inst-late-result`

### Summary Trigger Evaluation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`

**Input**: The open finalization transaction of a turn whose CAS was won (the closure of `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream`, after `inst-enqueue-audit`), the committed classification, the chat tenant and id, the causing turn's `request_id`, and the four ContextPlan outputs `assembled_context_tokens`, `messages_truncated`, the effective budget and `thread_summary_applied` (the summary-loaded flag). These four come from the ContextPlan produced by `cpt-cf-mini-chat-algo-streaming-turns-assemble-context` (`inst-assembled-tokens`, fed by `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs` or `cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history`) and are carried by `cpt-cf-mini-chat-algo-streaming-turns-run-provider-task` into the finalization input of `cpt-cf-mini-chat-algo-streaming-turns-finalize-stream` (see the §1.1 deferral table); the trigger reads them by value and never recomputes them. The effective budget has the semantics of `inst-input-limit`: `context_window` minus `max_output_tokens_applied`, lowered to `max_input_tokens` only when that is greater than 0, so `max_input_tokens` 0 means no separate input limit and never a zero budget. Also `thread_summary_worker` (`enabled`, `compression_threshold_pct`), the `system_request_id` generated before the transaction

**Output**: `scheduled`, `not_needed` or `not_evaluated`

**Steps**:
1. [x] - `p1` - **IF** `thread_summary_worker.enabled` is `false` or the classification is not `completed`, **RETURN** `not_evaluated` - `inst-if-trigger-off`
2. [x] - `p1` - Set `urgent` when `messages_truncated` is set; set `threshold_reached` when the effective budget is greater than 0 and `assembled_context_tokens` multiplied by 100 is at least the effective budget multiplied by `compression_threshold_pct` (integer arithmetic; tool surcharges and `fixed_overhead_tokens` are not subtracted from the budget) - `inst-trigger-conditions`
3. [x] - `p1` - **IF** neither `urgent` nor `threshold_reached` holds, **RETURN** `not_evaluated` - `inst-if-below-threshold`
4. [x] - `p1` - **IF** a summary was loaded for the turn's context and `urgent` does not hold, **RETURN** `not_needed`; the existing summary is still effective - `inst-if-summary-effective`
5. [x] - `p1` - DB: SELECT `thread_summaries` WHERE `tenant_id` and `chat_id` match; its frontier is the base frontier, none when no row exists - `inst-read-base-frontier`
6. [x] - `p1` - DB: SELECT the chat's latest `messages` row WHERE `tenant_id` and `chat_id` match, `deleted_at IS NULL` and `request_id` is not the causing turn's `request_id`, ordered by `created_at` descending and then `id` descending, limited to 1 row; its `(created_at, id)` is the frozen target frontier; the causing turn is never summarized - `inst-select-frozen-target`
7. [x] - `p1` - **IF** no such message exists, **RETURN** `not_needed` - `inst-if-no-target`
8. [x] - `p1` - **IF** a base frontier exists and the target is at or before it in the order `(created_at ASC, id ASC)`, **RETURN** `not_needed` - `inst-if-frontier-at-target`
9. [x] - `p1` - Build the work item payload with exactly the keys of the §1.4 "Queues and payloads" table: `tenant_id`, `chat_id`, the precomputed `system_request_id`, the base frontier pair (both `null` when none), the frozen target pair and `system_task_type` `thread_summary_update` - `inst-build-summary-payload`
10. [x] - `p1` - DB: enqueue the payload to `outbox.thread_summary_queue_name` partitioned by `chat_id`; an enqueue failure fails the finalization transaction like any statement of the closure - `inst-enqueue-summary-task`
11. [x] - `p1` - **RETURN** `scheduled`; after the commit the caller wakes the queue and records `mini_chat_thread_summary_trigger{result}` with `scheduled` or `not_needed` (nothing for `not_evaluated`); the in-flight turn's `ContextPlan` and its `done` event are never changed by the trigger - `inst-return-trigger-result`

### Thread Summary Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`

**Input**: One message of `outbox.thread_summary_queue_name` (payload and `attempts`), the delivery start instant, the S2S security context, `thread_summary_worker` (`max_attempts`, `summary_model_id`, `summary_system_prompt`, `message_content_limit`, `claim_timeout_secs`)

**Output**: `Ok`, `Retry` or `Reject` for the shared outbox

**Steps**:
1. [x] - `p1` - At `start`, register this handler for `outbox.thread_summary_queue_name` in place of the foundation placeholder (see `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`, `inst-placeholder-handlers`), with the lease `thread_summary_worker.claim_timeout_secs` - `inst-register-summary-handler`
2. [x] - `p1` - Set the delivery number to `attempts` plus 1; every `Retry` returned below becomes `Reject` (dead letter) when the delivery number is at least `thread_summary_worker.max_attempts`, with the same metric `result` - `inst-retry-budget`
3. [x] - `p1` - Decode the payload; **IF** a key is missing or unknown, an id is not a UUID, a timestamp is not RFC 3339, only one half of the base pair is `null` or `system_task_type` is not `thread_summary_update`, log the decode error and **RETURN** `Reject` - `inst-decode-summary`
4. [x] - `p1` - DB: SELECT `chats` WHERE `tenant_id` and `id` match the payload, soft-deleted rows included; **IF** the read fails, record `mini_chat_thread_summary_execution{result="retry"}` and **RETURN** `Retry` - `inst-load-summary-chat`
5. [x] - `p1` - **IF** the chat does not exist or is soft-deleted, **RETURN** `Ok` without a provider call - `inst-if-summary-chat-gone`
6. [x] - `p1` - DB: SELECT `thread_summaries` WHERE `tenant_id` and `chat_id` match (database reads failing in steps 6 to 9 are handled as in step 4) - `inst-load-current-summary`
7. [x] - `p1` - **IF** the payload has a base frontier and no row exists (a retry, edit or delete invalidated the summary the task was based on), record `result="base_missing"` and **RETURN** `Ok` - `inst-if-base-missing`
8. [x] - `p1` - **IF** the stored frontier differs from the payload's base frontier (a row exists while the base is `null`, or the frontiers differ), record `mini_chat_thread_summary_cas_conflicts` and **RETURN** `Ok`; another commit already advanced the frontier - `inst-if-frontier-moved`
9. [x] - `p1` - DB: SELECT the frozen target `messages` row WHERE `tenant_id`, `chat_id` and `id` match; **IF** it does not exist or has `deleted_at` set, record `result="frontier_deleted"` and **RETURN** `Ok` - `inst-if-target-deleted`
10. [x] - `p1` - DB: SELECT the range: `messages` WHERE `tenant_id` and `chat_id` match, `deleted_at IS NULL`, `is_compressed` is `false`, `(created_at, id)` is greater than the base frontier (when present) and at most the frozen target, ordered by `created_at` and then `id` ascending; messages created after the frozen target are never read - `inst-select-range`
11. [x] - `p1` - **IF** the range is empty, log a warning with the chat id and **RETURN** `Ok` - `inst-if-range-empty`
12. [x] - `p1` - Resolve the summary model `thread_summary_worker.summary_model_id` (empty means `gpt-4.1-mini`) in the current snapshot with the enabled filter, then its provider entry and alias for the payload tenant (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`); each plugin call is bounded by `POLICY_PLUGIN_CALL_TIMEOUT` - `inst-resolve-summary-model`
    1. [x] - `p1` - **IF** the model is missing from `model_catalog` or not enabled, log an error naming the model id, record `result="model_unavailable"` and **RETURN** `Reject` (no retry) - `inst-if-model-unavailable`
    2. [x] - `p1` - **IF** another resolution error occurs (plugin error or timeout, unresolvable provider entry), record `result="retry"` and **RETURN** `Retry` - `inst-if-model-resolution-error`
13. [x] - `p1` - Build the prompt (see `cpt-cf-mini-chat-algo-background-workers-build-summary-prompt`) with the range and, when a base frontier exists, the stored `summary_text` - `inst-build-prompt`
14. [x] - `p1` - Call the summary model (see `cpt-cf-mini-chat-algo-background-workers-call-summary-model`) with the call deadline of §1.4 - `inst-call-summary`
15. [x] - `p1` - **MATCH** the call result - `inst-match-summary-call`
    - [x] - `p1` - **CASE** provider failure (error, timeout, unreadable response, context length still exceeded after the retries): record `result="provider_error"` and `mini_chat_summary_fallback`; the previous summary, the frontier and `is_compressed` stay unchanged; **RETURN** `Retry` - `inst-case-provider-error`
    - [x] - `p1` - **CASE** empty summary: record `result="empty_summary"`; nothing changes; **RETURN** `Retry` - `inst-case-empty-summary`
    - [x] - `p1` - **CASE** summary text with usage: continue - `inst-case-summary-text`
16. [x] - `p1` - Set `token_estimate` to the provider's `output_tokens` minus `reasoning_tokens` when that difference is greater than 0, otherwise to the UTF-8 byte length of the summary text divided by 4, rounded up - `inst-token-estimate`
17. [x] - `p1` - Commit (see `cpt-cf-mini-chat-algo-background-workers-commit-summary`) and **MATCH** its result - `inst-commit-outcome`
    - [x] - `p1` - **CASE** `committed`: record `result="success"`; **RETURN** `Ok` - `inst-case-committed`
    - [x] - `p1` - **CASE** `frontier_deleted`: record `result="frontier_deleted"`; **RETURN** `Ok` (no summary, no `is_compressed` change, no usage event) - `inst-case-commit-frontier-deleted`
    - [x] - `p1` - **CASE** `base_missing`: record `result="base_missing"`; **RETURN** `Ok` - `inst-case-commit-base-missing`
    - [x] - `p1` - **CASE** `cas_conflict`: record `mini_chat_thread_summary_cas_conflicts`; **RETURN** `Ok` without another commit for this frozen range - `inst-case-commit-conflict`
    - [x] - `p1` - **CASE** database error: record `result="retry"`; **RETURN** `Retry` - `inst-case-commit-error`
18. [x] - `p1` - The handler never creates or changes a `chat_turns` row, never reads or writes `quota_usage`, checks no kill switch and emits no audit event (`cpt-cf-mini-chat-adr-quota-policy-scope`); the summary text is never logged - `inst-system-task-isolation`

### Summary Prompt Construction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-build-summary-prompt`

**Input**: The range messages in order, the stored summary text or none, the summary model's catalog entry (`thread_summary_prompt`, `context_window`, `max_input_tokens`, `max_output_tokens`, `estimation_budgets.bytes_per_token_conservative`), `thread_summary_worker.summary_system_prompt`, `thread_summary_worker.message_content_limit`

**Output**: The system prompt, the user prompt, the kept messages

**Steps**:
1. [x] - `p1` - Set the system prompt to the catalog `thread_summary_prompt` when it is not empty, otherwise `thread_summary_worker.summary_system_prompt` when it is not empty, otherwise the built-in default of DESIGN B.5.5 - `inst-select-system-prompt`
2. [x] - `p1` - Start the user prompt with `Summarize the following conversation:` when there is no stored summary; otherwise with the merge instruction of B.5.5, the stored summary inside an `<existing_summary>` block, and `New messages to incorporate:` - `inst-prompt-opening`
3. [x] - `p1` - **FOR EACH** kept message in chronological order, append one entry `User: <content>` or `Assistant: <content>` by its role (messages of any other role are skipped), entries separated by a blank line; **IF** `message_content_limit` is greater than 0 and the content is longer than that many characters (Unicode scalar values), cut it to that length and end it with `...` - `inst-prompt-entries`
4. [x] - `p1` - End the user prompt with the analysis instruction of B.5.5 - `inst-prompt-analysis`
5. [x] - `p1` - Set the input budget to `context_window` minus `max_output_tokens`, lowered to `max_input_tokens` when that is greater than 0 and smaller; **IF** `context_window` is 0, no fitting is applied - `inst-summary-budget`
6. [x] - `p1` - **WHILE** the estimate of the system prompt plus the user prompt (their UTF-8 bytes divided by `bytes_per_token_conservative`, rounded up) exceeds the input budget and more than 2 messages are kept, drop the oldest `ceil(n / 5)` kept messages (n the kept count, keeping at least 2) and rebuild steps 2 to 4 - `inst-fit-prompt`
7. [x] - `p1` - **RETURN** the prompts and the kept messages; the commit still covers the whole frozen range - `inst-return-prompt`

### Summary Provider Call

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-call-summary-model`

**Input**: The summary model's catalog entry and provider entry (kind, alias, `api_path`), the prompts and the kept messages, the chat tenant and id, the S2S security context, the call deadline

**Output**: The summary text and the provider usage; or a provider failure; or an empty summary

**Steps**:
1. [x] - `p1` - Build a non-streaming request in the wire format of the provider entry's kind (`openai_responses`, `chat_completions`, `anthropic_messages`, `vllm_responses`): the provider model id, the system prompt as instructions, one user input with the user prompt, the maximum output tokens set to the catalog `max_output_tokens` (not capped by `streaming.max_output_tokens`), no tools, the `user` value of the chat tenant and the system subject `11111111-6a88-4768-9dfc-6bcd5187d9ed` (DESIGN "Provider Request Metadata"), and for `openai_responses` the `metadata` with `request_type` `summary`, `feature` `none`, the tenant, the system user id and the chat id; no credential header is set, OAGW injects it (`cpt-cf-mini-chat-constraint-no-credentials`) - `inst-build-summary-request`
2. [x] - `p1` - API: `proxy_request` `POST /{alias}{api_path}` with the S2S context, bounded by the call deadline and the OAGW upstream timeouts; read the response body up to `SUMMARY_RESPONSE_MAX_BYTES` - `inst-send-summary-request`
3. [x] - `p1` - **IF** the response is a context-length error (classified as in `cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`), fewer than 2 such retries were made and more than 2 messages are kept, drop the oldest `ceil(n / 5)` kept messages (keeping at least 2), rebuild the prompt (see `cpt-cf-mini-chat-algo-background-workers-build-summary-prompt`, steps 2 to 4) and **GO TO** step 1 - `inst-ptl-retry`
4. [x] - `p1` - **IF** the call fails, times out, answers a non-2xx status, exceeds the body bound or has a body that the adapter cannot read, log the chat id with the sanitized cause (see `cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`) and **RETURN** provider failure - `inst-return-summary-failure`
5. [x] - `p1` - Read the output text and the usage (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens`; a missing field is 0) in the adapter's format - `inst-extract-summary`
6. [x] - `p1` - Remove every `<analysis>...</analysis>` block; **IF** a `<summary>...</summary>` block exists, keep its inside; **ELSE** keep the remaining text unless it still contains `<analysis` or `<summary`, in which case the result is empty; collapse runs of blank lines and trim the result - `inst-parse-summary`
7. [x] - `p1` - **IF** the result is empty, **RETURN** empty summary - `inst-if-summary-empty`
8. [x] - `p1` - **RETURN** the summary text and the usage - `inst-return-summary-text`

### Summary Commit

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-commit-summary`

**Input**: The tenant and chat, the base frontier or none, the frozen target frontier, the summary text, `token_estimate`, the provider usage, the `system_request_id`, the summary model id, `now`

**Output**: `committed`, `frontier_deleted`, `base_missing`, `cas_conflict` or a database error

**Transaction discipline** (normative): see §1.4. Write-first: `IMMEDIATE` on SQLite; on PostgreSQL the first statement is the row lock of step 2. A conflict re-runs the closure, at most 3 attempts in total with the 20 ms / 100 ms backoff.

**Steps**:
1. [x] - `p1` - Before the transaction, generate the new summary row id (UUID v4) and build the system-task usage event (see `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`, `inst-build-system-event` to `inst-system-dedupe-key`) with the `system_request_id`, the summary model id and the usage; the closure captures them by value - `inst-precompute-commit`
2. [x] - `p1` - DB: SELECT the frozen target `messages` row WHERE `tenant_id`, `chat_id` and `id` match, with a row lock (`FOR UPDATE` on PostgreSQL); this orders the commit against a mutation that soft-deletes that message - `inst-lock-frontier`
3. [x] - `p1` - **IF** the row does not exist or has `deleted_at` set, roll back and **RETURN** `frontier_deleted` - `inst-if-frontier-gone`
4. [x] - `p1` - **IF** there is no base frontier, DB: INSERT `thread_summaries` with the new id, `tenant_id`, `chat_id`, `summary_text`, `summarized_up_to_created_at` and `summarized_up_to_message_id` the frozen target, `token_estimate`, `created_at` and `updated_at` `now`; **IF** it fails with a unique violation on `chat_id` (another commit created the row), roll back and **RETURN** `cas_conflict` - `inst-insert-summary`
5. [x] - `p1` - **ELSE** DB: UPDATE `thread_summaries` SET `summary_text`, the frontier to the frozen target, `token_estimate` and `updated_at` `now` WHERE `tenant_id` and `chat_id` match and the stored frontier equals the base frontier - `inst-cas-summary`
   1. [x] - `p1` - **IF** 0 rows are updated, DB: SELECT the chat's `thread_summaries` row; **IF** none exists, roll back and **RETURN** `base_missing`; **ELSE** roll back and **RETURN** `cas_conflict` - `inst-if-cas-summary-lost`
6. [x] - `p1` - DB: UPDATE `messages` SET `is_compressed` `true` WHERE `tenant_id` and `chat_id` match, `deleted_at IS NULL`, `is_compressed` is `false`, and `(created_at, id)` is greater than the base frontier (when present) and at most the frozen target - `inst-mark-compressed`
7. [x] - `p1` - DB: enqueue the system-task usage event to `outbox.queue_name` partitioned by `tenant_id` (see `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`, `inst-enqueue-system-event`); `user_id` and `turn_id` are absent and no `quota_usage` row is touched - `inst-enqueue-summary-usage`
8. [x] - `p1` - Commit and wake the usage queue; the new summary, the advanced frontier, the compressed range and the usage event become visible together; **RETURN** `committed` - `inst-commit-summary`
9. [x] - `p1` - **IF** the transaction fails otherwise (database error, conflict attempts exhausted, an encoding failure of the usage event), **RETURN** database error; nothing was committed - `inst-return-commit-error`

### Kubernetes Lease Election

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-elect-leader`

**Input**: The enabled leader-only roles (`orphan-watchdog` when `orphan_watchdog.enabled`, `upload-reaper` when `upload_reaper.enabled`), whether the gear was built with the cargo feature `k8s`, the environment variables `POD_NAMESPACE` and `POD_NAME`, the gear stop token, the monotonic clock

**Output**: A leader elector that answers, per role, whether this process currently holds the leadership

**Steps**:
1. [x] - `p1` - At `start`, **IF** no leader-only worker is enabled, prepare no elector (`inst-if-leader-workers`); **ELSE IF** the gear was built without `k8s`, prepare the no-op elector, which reports leadership for every role (single-process mode) - `inst-noop-elector`
2. [x] - `p1` - Read `POD_NAMESPACE` and `POD_NAME`; **IF** either is missing or empty, **RETURN** `start` failure naming the variable - `inst-read-pod-identity`
3. [x] - `p1` - Build the in-cluster Kubernetes client from the pod's service account; **IF** it fails, **RETURN** `start` failure naming the cause - `inst-build-kube-client`
4. [x] - `p1` - **FOR EACH** enabled role, spawn the renew task `lease-{role}` on the background task registry, for the Lease `mini-chat-{role}` in `POD_NAMESPACE` with holder identity `POD_NAME`, `LEASE_DURATION` and `LEASE_RENEW_PERIOD` - `inst-spawn-lease-task`
5. [x] - `p1` - **WHILE** the stop token is not cancelled, once every `LEASE_RENEW_PERIOD` - `inst-lease-loop`
   1. [x] - `p1` - API: GET the Lease, bounded by `LEASE_API_TIMEOUT` - `inst-get-lease`
   2. [x] - `p1` - **IF** the Lease does not exist, API: CREATE it with `holderIdentity` `POD_NAME`, `leaseDurationSeconds` 15, `acquireTime` and `renewTime` now and `leaseTransitions` 0; a 409 conflict means another pod created it first and this round is not renewed - `inst-create-lease`
   3. [x] - `p1` - **ELSE IF** `holderIdentity` is `POD_NAME`, API: REPLACE it with `renewTime` now, carrying the read `resourceVersion`; a 409 conflict means another pod took it and this round is not renewed - `inst-renew-lease`
   4. [x] - `p1` - **ELSE IF** `holderIdentity` is empty, or the record (`holderIdentity`, `renewTime`, `resourceVersion`) has not changed for `LEASE_DURATION` as observed on the local monotonic clock, API: REPLACE it taking over (`holderIdentity` `POD_NAME`, `acquireTime` and `renewTime` now, `leaseTransitions` plus 1) with the read `resourceVersion`; a 409 conflict means another pod won - `inst-take-over-lease`
   5. [x] - `p1` - **ELSE** this pod is a follower; remember the observed record and the monotonic instant at which it was first seen unchanged - `inst-observe-other`
   6. [x] - `p1` - On a successful CREATE, renewal or take-over, set the role's last successful renewal to the current monotonic instant - `inst-record-renewal`
   7. [x] - `p1` - **IF** a call fails or times out, the round is not renewed; log a warning for the role at most once per minute; never fail the gear - `inst-lease-api-error`
6. [x] - `p1` - Report leadership for a role only while its last successful renewal is younger than `LEASE_RENEW_DEADLINE`; a leader that cannot renew stops reporting leadership before any other pod may take over - `inst-report-leadership`
7. [x] - `p1` - When the stop token is cancelled, **IF** this pod holds the Lease, API: REPLACE it with an empty `holderIdentity` and the read `resourceVersion`, bounded by `LEASE_API_TIMEOUT`, so another pod can take over without waiting; a failure is logged - `inst-release-lease`
8. [x] - `p1` - Each worker asks for its own role before each scan: the watchdog for `orphan-watchdog` (`inst-watchdog-loop`) and the upload reaper for `upload-reaper` (`inst-scan-loop` of `cpt-cf-mini-chat-algo-attachments-reap-abandoned`); leadership decides who runs a worker, never how candidates are judged - `inst-role-per-worker`

### Worker Metrics Recording

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-background-workers-record-metrics`

**Input**: Watchdog scan and finalization outcomes, chat cleanup outcomes, summary trigger and execution outcomes

**Output**: Recorded instruments with bounded labels only

**Steps**:
1. [x] - `p1` - Watchdog: `mini_chat_orphan_detected{reason="stale_progress"}` per candidate, `mini_chat_orphan_finalized{reason="stale_progress"}` and `mini_chat_streams_aborted{trigger="orphan_timeout"}` only after a won CAS commits, `mini_chat_orphan_scan_duration_seconds` per scan - `inst-watchdog-metrics`
2. [x] - `p1` - Chat cleanup: `mini_chat_cleanup_completed{resource_type}` (`file`, `vector_store`), `mini_chat_cleanup_failed{resource_type}` (`file` at the attachment limit, `vector_store` on the dead-lettering delivery), `mini_chat_cleanup_retry{resource_type,reason}` (`file` with `provider_error`, `vector_store` with `vector_store_delete_failed`), `mini_chat_cleanup_vector_store_with_failed_attachments` and `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}` - `inst-cleanup-metrics`
3. [x] - `p1` - Thread summary: `mini_chat_thread_summary_trigger{result}` (`scheduled`, `not_needed`) after the finalization commit, `mini_chat_thread_summary_execution{result}` (`success`, `provider_error`, `empty_summary`, `retry`, `model_unavailable`, `frontier_deleted`, `base_missing`), `mini_chat_thread_summary_cas_conflicts` and `mini_chat_summary_fallback` (provider failures only) - `inst-summary-metrics`
4. [x] - `p1` - Never use a tenant, user, chat, turn, request, attachment, file or vector store id, a model prompt or an error string as a label; leave `mini_chat_cleanup_backlog` and `mini_chat_summary_regen` unrecorded; queue lag, retries and dead letters come from the shared outbox metrics - `inst-worker-label-rules`
5. [x] - `p1` - **RETURN** - `inst-return-worker-metrics`

## 4. States (CDSL)

### Chat Provider Cleanup State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-background-workers-chat-cleanup`

**States**: `files_outstanding`, `store_outstanding`, `released`, `dead_lettered`

**Initial State**: `files_outstanding`

The state of a soft-deleted chat's provider resources is derived, not stored: `files_outstanding` while any attachment of the chat is `pending`; `store_outstanding` when none is `pending` and a `chat_vector_stores` row exists; `released` when neither exists; `dead_lettered` when the chat-cleanup message was rejected with the store row kept. The chat is fully purged from the provider only when `released` and no attachment is `failed`. A chat with nothing to clean passes straight to `released` on the first delivery.

**Transitions**:
1. [x] - `p1` - **FROM** `files_outstanding` **TO** `files_outstanding` **WHEN** a delivery leaves an attachment `pending` (a failed attempt below `cleanup_worker.max_attempts`, a row not reached before the delivery budget, a database error); the message is retried - `inst-files-retry`
2. [x] - `p1` - **FROM** `files_outstanding` **TO** `store_outstanding` **WHEN** every attachment of the chat is `done` or `failed` and a `chat_vector_stores` row exists - `inst-to-store-outstanding`
3. [x] - `p1` - **FROM** `files_outstanding` **TO** `released` **WHEN** every attachment is `done` or `failed` and the chat has no `chat_vector_stores` row - `inst-files-to-released`
4. [x] - `p1` - **FROM** `store_outstanding` **TO** `released` **WHEN** the provider store delete answered 2xx or 404 and the row was deleted, or the row was a placeholder with NULL `vector_store_id` - `inst-store-to-released`
5. [x] - `p1` - **FROM** `store_outstanding` **TO** `dead_lettered` **WHEN** the store delete fails on a delivery whose number is at least `cleanup_worker.max_attempts`; the row is kept - `inst-to-dead-lettered`
6. [x] - `p1` - **FROM** `dead_lettered` **TO** `store_outstanding` **WHEN** an operator replays the dead-lettered message - `inst-replay-dead-letter`
7. [x] - `p1` - **FROM** `released` **TO** `released` **WHEN** the message is redelivered; the handler finds no `pending` attachment and no store row and returns `Ok` without a provider call - `inst-released-redelivery`

### Orphan Candidate State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-background-workers-orphan-candidate`

**States**: `progressing`, `stale`, `orphan_failed`, `finalized_elsewhere`

**Initial State**: `progressing`

The watchdog's view of a `running`, non-deleted turn: `progressing` while `last_progress_at` (or `started_at` when NULL) is after `now` minus `orphan_watchdog.timeout_secs`, `stale` once it is at or before that cutoff. `orphan_failed` is the lifecycle state `failed` with `error_code` `orphan_timeout`; `finalized_elsewhere` is any terminal state written by another finalizer.

**Transitions**:
1. [x] - `p1` - **FROM** `progressing` **TO** `stale` **WHEN** no progress was written for `orphan_watchdog.timeout_secs` (crash, lost provider callback, a failed finalization or setup-failure mark) - `inst-to-stale`
2. [x] - `p1` - **FROM** `stale` **TO** `progressing` **WHEN** a text delta or tool event refreshes `last_progress_at` before the watchdog CAS; the CAS then updates no row and nothing is settled - `inst-stale-to-progressing`
3. [x] - `p1` - **FROM** `stale` **TO** `orphan_failed` **WHEN** the leader's watchdog wins the orphan CAS; in the same transaction the bounded debit (or no settlement for NULL reserve fields), the usage event and the `turn_failed` audit event are committed - `inst-to-orphan-failed`
4. [x] - `p1` - **FROM** `progressing` or `stale` **TO** `finalized_elsewhere` **WHEN** the stream path, a client disconnect or the setup-failure mark finalizes the turn first; the watchdog's CAS finds no `running` row and it skips the turn - `inst-to-finalized-elsewhere`
5. [x] - `p1` - **FROM** `orphan_failed` **TO** `orphan_failed` **WHEN** a late provider result, the stream path or a later scan tries to finalize the turn; every CAS loses and nothing changes - `inst-orphan-terminal`

### Thread Summary Frontier State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-background-workers-summary-frontier`

**States**: `absent`, `committed`

**Initial State**: `absent`

`absent` means the chat has no `thread_summaries` row (nothing summarized yet, or a mutation invalidated the summary); `committed` means a row with a frontier exists. Pending work lives only in the shared outbox, never in `thread_summaries`.

**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `committed` **WHEN** a work item without a base frontier commits its INSERT with the frozen target as the frontier - `inst-first-summary`
2. [x] - `p1` - **FROM** `committed` **TO** `committed` **WHEN** a work item whose base frontier equals the stored frontier wins the CAS; the frontier advances to its frozen target and the range is marked compressed - `inst-advance-summary`
3. [x] - `p1` - **FROM** `committed` **TO** `absent` **WHEN** a retry, edit or delete of a covered turn deletes the row and clears `is_compressed` (`cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary`, not written by this feature) - `inst-invalidated-by-mutation`
4. [x] - `p1` - **FROM** `absent` or `committed` **TO** the same state **WHEN** the work item ends with a provider error, an empty summary, an unavailable model, a deleted frozen target, a missing base, a CAS conflict or a database error; the frontier, the summary text and `is_compressed` are unchanged - `inst-summary-unchanged`

### Worker Leadership State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-background-workers-leadership`

**States**: `follower`, `leader`

**Initial State**: `follower`

One instance per process and role in `k8s` builds. The no-op elector of builds without `k8s` is always `leader`.

**Transitions**:
1. [x] - `p1` - **FROM** `follower` **TO** `leader` **WHEN** this pod creates the missing Lease or takes over an empty or expired one - `inst-to-leader`
2. [x] - `p1` - **FROM** `leader` **TO** `leader` **WHEN** a renewal succeeds within `LEASE_RENEW_DEADLINE` of the previous one - `inst-stay-leader`
3. [x] - `p1` - **FROM** `leader` **TO** `follower` **WHEN** no renewal succeeded for `LEASE_RENEW_DEADLINE`, a renewal conflict shows another holder, or the gear stops (the Lease is released best effort) - `inst-to-follower`
4. [x] - `p1` - **FROM** `follower` **TO** `follower` **WHEN** another pod keeps renewing the Lease, or a create or take-over loses with 409 - `inst-stay-follower`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Chat Deletion Provider Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-chat-cleanup-handler`

The system **MUST** replace the foundation placeholder of `outbox.chat_cleanup_queue_name` with the chat cleanup handler of `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup` and `cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment`, implementing the asynchronous half of `cpt-cf-mini-chat-seq-chat-deletion-cleanup` (`cpt-cf-mini-chat-fr-chat-deletion-cleanup`, `cpt-cf-mini-chat-nfr-data-retention`). The handler decodes the five-key payload and rejects a malformed payload, a missing chat and an active chat. It claims attachments of the deleted chat that have no cleanup status. For each `pending` attachment, soft-deleted ones included, it deletes the provider file and the Anthropic secondary copy through OAGW with the S2S context, bounded by `PROVIDER_DELETE_TIMEOUT` and the delivery budget. 2xx and 404 are success. A failed attempt increments `cleanup_attempts` and records `last_cleanup_error`; at `cleanup_worker.max_attempts` the attachment becomes `failed` and the handler continues with the rest of the chat. A database error is `Retry` without counting an attempt. Attachments of live chats remain owned by the attachment cleanup handler. Retries, backoff, leases and dead letters are the shared outbox's; no gear-local polling worker exists.

**Implements**:
- `cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup`
- `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`
- `cpt-cf-mini-chat-algo-background-workers-clean-chat-attachment`
- `cpt-cf-mini-chat-state-background-workers-chat-cleanup`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Sequence: `cpt-cf-mini-chat-seq-chat-deletion-cleanup`
- DB Table: `cpt-cf-mini-chat-dbtable-chats` (read), `cpt-cf-mini-chat-dbtable-attachments` (`cleanup_status`, `cleanup_attempts`, `last_cleanup_error`, `cleanup_updated_at`)
- DB: shared outbox tables (queue `mini-chat.chat_cleanup`)
- Components: `cpt-cf-mini-chat-component-llm-provider`, `cpt-cf-mini-chat-component-chat-store`
- Entities: Chat, Attachment

### Vector Store Release After Attachment Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-vector-store-cleanup`

The system **MUST** delete a soft-deleted chat's provider vector store only when no attachment of the chat is `pending`, as specified in `cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store` (DESIGN "Vector store cleanup ordering"). Before each delete made while an attachment is `failed`, it records `mini_chat_cleanup_vector_store_with_failed_attachments`. After provider 2xx or 404 it deletes the `chat_vector_stores` row, which is the durable completion marker and the transition `inst-bound-to-absent` of `cpt-cf-mini-chat-state-attachments-chat-vector-store`. A placeholder row without `vector_store_id` is deleted without a provider call. On a failed delete it keeps the row and returns `Retry` until the delivery whose number reaches `cleanup_worker.max_attempts`, which returns `Reject` (`vector store delete: max attempts (N) reached`); a dead-letter replay retries the delete.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-clean-chat-vector-store`
- `cpt-cf-mini-chat-state-background-workers-chat-cleanup`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-vector-stores`
- Entities: ChatVectorStore

### Orphan Watchdog Scheduling and Single Actor

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-orphan-watchdog`

The system **MUST** run the orphan watchdog of `cpt-cf-mini-chat-algo-background-workers-run-watchdog` when `orphan_watchdog.enabled` is `true`, every `orphan_watchdog.scan_interval_secs`, only while the elector reports leadership for `orphan-watchdog`, and stop it with the gear. Each scan takes at most 100 `running`, non-deleted turns whose `last_progress_at`, or `started_at` when NULL, is at or before the application-clock cutoff `now` minus `orphan_watchdog.timeout_secs`. Turns with recent progress are never candidates, whatever their `started_at`. The scan is advisory; finalization is decided only by the orphan CAS, which re-checks the stale-progress predicate, so a renewed turn, a finalized turn and a duplicate scan on a second replica change nothing (`cpt-cf-mini-chat-component-orphan-watchdog`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`).

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-run-watchdog`
- `cpt-cf-mini-chat-state-background-workers-orphan-candidate`
- `cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns` (orphan scan index)
- Design: `cpt-cf-mini-chat-design-turn-lifecycle`
- Components: `cpt-cf-mini-chat-component-orphan-watchdog`

### Orphan Finalization and Bounded Debit

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-orphan-finalization`

The system **MUST** finalize each orphan candidate through the orphan variant of `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`, as specified in `cpt-cf-mini-chat-algo-background-workers-finalize-orphan` (`cpt-cf-mini-chat-design-nonterminal-reconciliation`, `cpt-cf-mini-chat-fr-quota-billing-architecture`, `cpt-cf-mini-chat-nfr-resilience-recovery`). One transaction writes the CAS to `failed` / `orphan_timeout` with `completed_at`, the bounded estimated debit, the usage event (`aborted`, `estimated`) and the `turn_failed` audit event. The audit event has `selected_model` equal to the effective model and quota decision `unknown`. A turn with NULL reserve fields is finalized without settlement, but its 0-credit usage event and its audit event are still enqueued (`inst-unreserved-orphaned` of `cpt-cf-mini-chat-state-turn-mutations-replacement-setup`). A turn without `requester_user_id` is finalized with neither event. The snapshot is fetched before the transaction; no message row is written. A lost CAS settles, enqueues and counts nothing, and the stream path that loses afterwards ends with `stream_interrupted`.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`
- `cpt-cf-mini-chat-state-background-workers-orphan-candidate`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chat-turns`, `cpt-cf-mini-chat-dbtable-quota-usage` (through the quota helper)
- DB: shared outbox tables (queues `mini-chat.usage_snapshot`, `mini-chat.audit`)
- Components: `cpt-cf-mini-chat-component-quota-service`
- Entities: Chat turn, AuditEvent

### Summary Scheduling in the Finalization Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-summary-trigger`

The system **MUST** evaluate the thread summary trigger in the finalization transaction of every completed turn when `thread_summary_worker.enabled` is `true`, as specified in `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger` (DESIGN "Trigger timing", `cpt-cf-mini-chat-fr-thread-summary`, `cpt-cf-mini-chat-principle-summary-over-history`). Send, retry and edit turns are all covered through the shared stream finalization. The trigger fires when context assembly truncated history, or when no summary existed and `assembled_context_tokens` reached `compression_threshold_pct` percent of the effective budget. It enqueues one durable work item, partitioned by `chat_id`, with the base frontier re-read in the transaction, the frozen target (the latest live message outside the causing turn) and a stable `system_request_id`. It enqueues nothing when no earlier message exists or the frontier is already at the target. Message-count heuristics are not used, and the in-flight turn's context and `done` are unchanged.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`
- `cpt-cf-mini-chat-flow-background-workers-long-conversation-compression`

**Constraints**: `cpt-cf-mini-chat-constraint-context-budget`

**Touches**:
- Sequence: `cpt-cf-mini-chat-seq-thread-summary`
- DB Table: `cpt-cf-mini-chat-dbtable-thread-summaries` (read), `cpt-cf-mini-chat-dbtable-messages` (read)
- DB: shared outbox tables (queue `mini-chat.thread_summary`)

### Thread Summary Execution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-summary-handler`

The system **MUST** replace the foundation placeholder of `outbox.thread_summary_queue_name` with the handler of `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`, `cpt-cf-mini-chat-algo-background-workers-build-summary-prompt` and `cpt-cf-mini-chat-algo-background-workers-call-summary-model`. The handler runs as a system task: no `chat_turns` row, no `quota_usage` debit, no audit event, `requester_type` `system`. It binds itself to the frozen range of the payload and reads exactly the live, non-compressed messages in `(base, target]`. It resolves the summary model with the enabled filter (missing or disabled: `Reject`, `model_unavailable`) and builds and fits the B.5.5 prompt. It makes one non-streaming call through OAGW with the S2S context, with up to 2 context-length retries that drop the oldest fifth of the messages. It parses the `<summary>` block. A failed call or an empty summary keeps the previous summary, the frontier and `is_compressed` unchanged and returns `Retry`; on the `thread_summary_worker.max_attempts`-th delivery it returns `Reject`, and the message is dead-lettered.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`
- `cpt-cf-mini-chat-algo-background-workers-build-summary-prompt`
- `cpt-cf-mini-chat-algo-background-workers-call-summary-model`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Sequence: `cpt-cf-mini-chat-seq-thread-summary`
- DB Table: `cpt-cf-mini-chat-dbtable-chats` (read), `cpt-cf-mini-chat-dbtable-messages` (read), `cpt-cf-mini-chat-dbtable-thread-summaries` (read)
- Components: `cpt-cf-mini-chat-component-llm-provider`
- Entities: ThreadSummary

### Guarded Summary Commit

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-summary-commit`

The system **MUST** commit a generated summary in the single write-first transaction of `cpt-cf-mini-chat-algo-background-workers-commit-summary`. The transaction locks the frozen target message and skips the commit when it is soft-deleted (`frontier_deleted`). It inserts the first summary, or updates the stored summary only when its frontier equals the base frontier; a concurrent or replayed commit gets `cas_conflict`, and a summary deleted by a mutation gets `base_missing`. It marks exactly the frozen range `is_compressed` and enqueues the system-task usage event (`system_task`, `settlement_method` `none`, 0 credits, dedupe key `{tenant}/thread_summary_update/{system_request_id}`) in the same transaction. At most one commit advances the frontier for a given frozen range; duplicate provider calls under retry or replay are accepted. A mutation and a commit that race are ordered by the frozen target's row lock, so a removed turn never reappears in a summary (DESIGN §3.9 "Summary Interaction on Turn Mutation").

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-commit-summary`
- `cpt-cf-mini-chat-state-background-workers-summary-frontier`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-thread-summaries`, `cpt-cf-mini-chat-dbtable-messages` (`is_compressed`)
- DB: shared outbox tables (queue `mini-chat.usage_snapshot`)
- Entities: ThreadSummary, Message

### Leader Election for Leader-Only Workers

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-leader-election`

The system **MUST** provide the leader elector of `cpt-cf-mini-chat-algo-background-workers-elect-leader` with leadership per role. In builds with the cargo feature `k8s`, a gear-local Kubernetes Lease elector holds the Lease `mini-chat-orphan-watchdog` or `mini-chat-upload-reaper` in `POD_NAMESPACE` as `POD_NAME`. It uses a 15 s lease duration and a 2 s renew period, creates a missing Lease at runtime, and updates it under `resourceVersion` concurrency. It takes over only after the record stayed unchanged for the lease duration on its own clock, reports leadership only within the 10 s renew deadline, and releases the Lease best effort on stop. In builds without `k8s`, the no-op elector reports leadership for every role. The upload reaper asks for its own role. The Lease client uses only the `kube` and `k8s-openapi` crates already declared as optional dependencies and present in `Cargo.lock`; the decision logic is behind a Lease API port, so it is tested without a cluster.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-elect-leader`
- `cpt-cf-mini-chat-state-background-workers-leadership`

**Touches**:
- Kubernetes: `coordination.k8s.io/v1` Lease objects (`k8s` builds)
- Components: `cpt-cf-mini-chat-component-orphan-watchdog`
- Design: `cpt-cf-mini-chat-design-turn-lifecycle` ("Watchdog Single-Actor Guarantee")

### Background Worker Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-metrics`

The system **MUST** record the orphan watchdog, cleanup and thread-summary instruments of DESIGN "Metric series" exactly as specified in `cpt-cf-mini-chat-algo-background-workers-record-metrics` (`cpt-cf-mini-chat-nfr-observability-supportability`). Labels are limited to `reason`, `resource_type`, `trigger`, `provider_kind` and `result` with the listed values. `mini_chat_orphan_finalized` is recorded only after a won CAS, and `mini_chat_summary_fallback` only for provider failures. No identifier or free-form string is ever a label. The instruments are those registered by `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-record-metrics`

**Touches**:
- Observability: `mini_chat_*` meter

### Deferred Lifecycle Capabilities

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-deferred`

The system **MUST NOT** implement the capabilities this feature traces as not implemented or deferred: hard-purge of soft-deleted rows, a chat-deletion audit event and cancellation of a running turn on chat delete (`cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`); temporary chat auto-deletion (P2); the provider file reconciliation job (P2); the summary quality gate and `mini_chat_summary_regen` (P2+); and kill-switch checks, operational billing and audit events for system tasks (`cpt-cf-mini-chat-adr-quota-policy-scope`). The deprecated keys `cleanup_worker.enabled`, `poll_interval_secs`, `reconcile_interval_secs`, `stale_in_progress_timeout_secs`, `batch_size` and `thread_summary_worker.reconcile_interval_secs` keep having no effect; no worker reads them.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`
- `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`

**Touches**:
- Configuration: deprecated `cleanup_worker.*` and `thread_summary_worker.reconcile_interval_secs` keys (read by no worker)

### Cross-Cutting Quality and Verification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-background-workers-quality-gates`

The system **MUST** meet these cross-cutting requirements and completion signals:

- **Security**: no worker runs with a user's `SecurityContext`. Provider calls use the S2S context obtained at `start`, and OAGW injects every credential (`cpt-cf-mini-chat-constraint-no-credentials`). Every database statement is bound to the `tenant_id` and `chat_id` of the payload or of the scanned row (`cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-nfr-tenant-isolation`). The watchdog's cross-tenant scan and its `allow_all` CAS are the system boundary documented by `cpt-cf-mini-chat-feature-quota-billing`. No message content, summary text, provider identifier or provider error body is written to a log, an outbox payload or a metric label; provider causes are sanitized.
- **Reliability**: every external call is bounded (§1.4 "External-call timeouts"). Each handler finishes within its queue lease. Every provider delete is idempotent (404 is success). Every outbox outcome is `Ok`, `Retry` or `Reject` with bounded retries and dead letters, except database errors, which are retried without a limit. No worker blocks a request path. A pod crash is recovered by the watchdog, and a crashed leader is replaced within `LEASE_DURATION`.
- **Data integrity**: the orphan CAS, the summary frontier CAS with the target-row lock, and the `pending` guards on attachment writes make every worker safe under duplicate delivery, replay and a second replica. Each frozen range has at most one summary commit. Every reserved turn gets exactly one usage event, also after a crash, and soft-deleted rows are never purged.
- **Observability**: the instruments of `cpt-cf-mini-chat-algo-background-workers-record-metrics`; logs at `warn` for orphan finalizations, at `error` for corrupt data, an unavailable summary model and dead-lettering decisions, with chat, turn, request or attachment ids only.
- **Rollback**: the feature adds no migration (the tables, columns and indexes come from `cpt-cf-mini-chat-dod-gear-foundation-schema`). Removing the handlers brings back the foundation placeholders, which retry and lose no message. `orphan_watchdog.enabled` and `thread_summary_worker.enabled` switch the watchdog and the trigger off, and a build without `k8s` falls back to the no-op elector.
- **Deterministic time**: the chat cleanup delivery deadline (`CHAT_CLEANUP_DELIVERY_BUDGET`), the per-call deadline (`PROVIDER_DELETE_TIMEOUT` lowered to the time left), the summary call deadline and `SUMMARY_COMMIT_RESERVE`, and the watchdog and Lease timers are measured on an injectable monotonic clock (tokio paused time, or an injected clock trait passed to the handlers). Mock provider delays use the same clock. The tests of the deadline, deferral and reserve paths advance the paused clock and therefore need no real multi-second wait. Service-level tests on SQLite or PostgreSQL use the injected clock (or a manual `advance`) and do not rely on paused-time auto-advance.
- **Test layering**: unit tests cover:
  - the trigger decision (threshold, truncation, existing summary, no earlier message, frontier at target);
  - the prompt building and fitting, the response parsing and the `token_estimate` rule;
  - the chat-cleanup decisions (attempt limit, deadline cut, failed debt, delivery-number limit);
  - the orphan request and audit payload;
  - the Lease decisions with a fake Lease API and a fake monotonic clock (create, renew, conflict, observed-time take-over, renew deadline, release).

  Service-level tests run on SQLite and PostgreSQL with fake `StorageClient`, `ServiceGatewayClientV1` and policy plugin, a controllable clock and an in-process outbox. They check the exact rows of the §1.4 "DB writes" table, the outbox messages and the quota buckets. They race the watchdog against a stream finalization, and a summary commit against a delete and a retry. End-to-end tests drive `DELETE /v1/chats/{id}` and long conversations against the OpenAI-compatible mock behind OAGW. No live provider or Kubernetes cluster is called.
- **Compile-time gates**: the worker domain logic depends on ports (turn repository, attachment repository, storage client, quota service, Lease API), not on SeaORM entities or `kube` types. `cargo clippy -p cf-gears-mini-chat --all-targets -- -D warnings` passes, and so does `cargo check -p cf-gears-mini-chat --features k8s --offline` (no crate added to `Cargo.lock`). `make dylint` passes.
- **Completion signals**: automated tests exist and pass for every owned acceptance item listed in §1.4, and `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass. Every implemented CDSL instruction of this feature, and the deferred transitions listed in §1.1, is wrapped in `@cpt-begin:{cpt-id}:p1:inst-{id}` / `@cpt-end:{cpt-id}:p1:inst-{id}` code markers (FULL traceability), and every flow, algorithm, state and definition-of-done ID of this feature appears in code. The checkboxes of this document and the featstatus are set to `[x]` only when their markers and tests exist.

**Implements**:
- `cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`
- `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`
- `cpt-cf-mini-chat-algo-background-workers-commit-summary`
- `cpt-cf-mini-chat-algo-background-workers-elect-leader`

**Touches**:
- Build: `make gear-ci GEAR=mini-chat`, `make dylint`, `cfs validate`

## 6. Acceptance Criteria

- [ ] Cleanup & Recovery — "Chat deletion triggers reliable background cleanup of provider-side resources" (happy path): a chat with two ready documents (one in the vector store), one XLSX and one image with an uploaded Anthropic secondary copy is deleted with `DELETE /v1/chats/{id}`. The response is 204 before any provider call. The handler then sends exactly one `DELETE /files/{id}` per provider file and one secondary delete with the two Anthropic headers, and every attachment ends `cleanup_status` `done`. Then it sends exactly one `DELETE /vector_stores/{id}`, after the last file delete, and the `chat_vector_stores` row is gone. The message is acknowledged, `mini_chat_cleanup_completed` counts 4 `file` and 1 `vector_store`, no `messages`, `chat_turns` or `chats` row changes, and no audit event is enqueued. A provider answering 404 for files and the store gives the same end state. A redelivery of the acknowledged message makes no provider call and returns `Ok`.
- [ ] Cleanup & Recovery — chat deletion under failure: with the mock failing one file delete with 500 twice, that attachment shows `cleanup_attempts` 2 and `last_cleanup_error` `http 500` while `pending`, `mini_chat_cleanup_retry{resource_type="file",reason="provider_error"}` counts 2, and the store is not deleted while it is `pending`; the third delivery deletes it and releases the store. With the file delete failing every time and `cleanup_worker.max_attempts` 3, the attachment ends `failed` with `mini_chat_cleanup_failed{resource_type="file"}` 1. The other attachments end `done`, `mini_chat_cleanup_vector_store_with_failed_attachments` is recorded, and the store is still deleted. With the store delete failing every time, the deliveries return `Retry` until the delivery whose number reaches the limit. That delivery returns `Reject` with `vector store delete: max attempts (3) reached` and records `mini_chat_cleanup_failed{resource_type="vector_store"}`, and the row stays; replaying the dead letter with the mock fixed deletes the store and the row. A database failure returns `Retry` without changing `cleanup_attempts`. A message for an active chat, for an unknown chat, or with a malformed payload is `Reject` and touches nothing. Rows of another tenant or chat are never read or changed. An attachment deleted by the user before the chat deletion, whose own cleanup was still `pending`, is cleaned by the chat handler, and the attachment cleanup handler steps aside for it.
- [ ] Cleanup & Recovery — chat deletion edge cases (feature-local): a chat whose vector store row is a placeholder (`vector_store_id` NULL) gets its row deleted without a vector store call. An attachment row without a cleanup status in the deleted chat (inserted by a racing upload) is claimed and cleaned, and the racing upload's later CAS fails. The delivery budget is really exceeded: a chat (non-Anthropic provider, so one delete per attachment) with 30 pending attachments and a mock delaying each delete by 8 s (below `PROVIDER_DELETE_TIMEOUT`, so no delay is a failed attempt), run under the injectable clock of the "Deterministic time" requirement of §5, needs about 60 s of work at `CHAT_CLEANUP_CONCURRENCY` 4 (30 rows at 8 s each, 4 at a time). Deliveries 1 and 2 each finish 12 rows (calls start at 0 s, 8 s and 16 s of a delivery; the call that would start at 24 s has 1 s left, is cut by the deadline and is deferred). Delivery 3 has 6 rows left (4 start at 0 s, 2 at 8 s) and ends its last row at 16 s with 9 s left, below `PROVIDER_DELETE_TIMEOUT`, so `inst-if-no-time-for-store` returns `Retry` without a store call. Delivery 4 therefore makes the store call, so the run needs at least 4 deliveries, within the default `cleanup_worker.max_attempts` of 5. Every delivery that leaves a row `pending` returns `Retry`. No provider call is started later than 25 s after its delivery start, rows cut or not reached keep `cleanup_attempts` unchanged and `pending`, no `mini_chat_cleanup_retry` or `mini_chat_cleanup_failed` is recorded for them, and the vector store delete is never called in a delivery that leaves a row `pending`; it is called only after every row is `done`, which here is delivery 4 (delivery 3 returns `Retry` without a store call, as above). When the secondary alias cannot be resolved, the primary file is still deleted, the attachment ends `done` and `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}` is recorded.
- [ ] Cleanup & Recovery — "Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly" (generation): a chat whose catalog makes the effective budget small reaches 80% `assembled_context_tokens` on a completed turn without a summary. The finalization transaction enqueues one `mini-chat.thread_summary` message with `tenant_id`, `chat_id`, a UUID `system_request_id`, `null` base, the frozen target equal to the latest message before the causing turn and `system_task_type` `thread_summary_update`, and `mini_chat_thread_summary_trigger{result="scheduled"}` is recorded. The turn's `done` and context are unchanged. The handler sends one non-streaming request with the B.5.5 prompt (`User:` / `Assistant:` entries, content cut to `message_content_limit` with `...`, the summary model's `max_output_tokens`, no tools, `request_type` `summary`). It stores the `<summary>` content as `summary_text` with the frozen target as the frontier and `token_estimate` `output_tokens` minus `reasoning_tokens`. It marks exactly the messages of the range `is_compressed`, enqueues one usage event with `requester_type` `system`, `billing_outcome` `system_task`, `settlement_method` `none`, 0 credits and dedupe key `{tenant}/thread_summary_update/{system_request_id}`, and records `result="success"`. No `chat_turns` or `quota_usage` row changes and no audit event is enqueued. The next send's provider request starts with the summary preamble and contains no compressed message. `stream_started.thread_summary_applied` equals the stored `token_estimate`. With a summary present, a turn that reached the threshold without truncation records `not_needed` and enqueues nothing; a truncating turn enqueues a work item with the stored frontier as its base, and the commit replaces the summary through the base CAS. A failed or cancelled turn and a disabled `thread_summary_worker.enabled` enqueue nothing and record nothing.
- [ ] Cleanup & Recovery — thread summary failure and retry: with the mock answering 500 or timing out, the handler records `provider_error` and `mini_chat_summary_fallback`, the summary row, the frontier and `is_compressed` are unchanged, and the message is retried. On the third delivery (default `max_attempts` 3) it is dead-lettered (`Reject`). A response without a usable `<summary>` records `empty_summary` and retries. A context-length error is retried twice within the delivery with the oldest fifth of the messages dropped each time, and a success on the second retry commits the whole frozen range. A summary model missing from the catalog or disabled records `model_unavailable` and is rejected without a provider call. Two deliveries of the same work item that both reach the provider produce exactly one committed summary. The other records `mini_chat_thread_summary_cas_conflicts` and changes nothing.
- [ ] Cleanup & Recovery — mutation-driven invalidation and the guarded commit: with the mock holding the summary response, a delete of the turn that contains the frozen target commits first; the handler then records `frontier_deleted` and writes no summary, no `is_compressed` and no usage event. With a stored summary, a work item with that base whose summary row is deleted by a covering retry before the handler runs records `base_missing`. If the deletion happens between the call and the commit, the commit's base CAS finds no row and records `base_missing` as well, and no stale summary reappears. On PostgreSQL a commit and a covering delete started concurrently end in one of the two consistent states: summary deleted and flags cleared, or no summary written. A summary whose frontier is before the mutated turn survives the mutation and is used by the replacement turn.
- [ ] Orphan watchdog (feature-local; watchdog facets of Settlement & Finalization and Turn Lifecycle): a `running` turn with a reserve whose `last_progress_at` is older than `orphan_watchdog.timeout_secs` (controllable clock) is finalized by the next leader scan. It ends `failed` with `error_code` `orphan_timeout` and `completed_at` set. The buckets are settled once with `min(reserve_tokens, estimated_input_tokens + minimal_generation_floor_applied)`. One usage event (`aborted`, `estimated`, dedupe key `{tenant}/{turn}/{request}`) and one `turn_failed` audit event (`selected_model` equal to `effective_model`, quota decision `unknown`, usage 0) are enqueued. `GET /turns/{request_id}` returns `error` / `orphan_timeout`, and a new send is accepted. No message row is written. `mini_chat_orphan_detected` and `mini_chat_orphan_finalized{reason="stale_progress"}`, `mini_chat_streams_aborted{trigger="orphan_timeout"}` and one `mini_chat_orphan_scan_duration_seconds` sample are recorded. A turn with an old `started_at` and recent `last_progress_at`, a soft-deleted turn and a terminal turn are not touched. A turn whose progress is refreshed between the scan and the CAS is not finalized and records detected but not finalized. When the watchdog wins while the mock still holds the stream, the stream ends with `stream_interrupted` and the stream path writes no second state, usage event or audit event. When the stream path finalizes first, the watchdog changes nothing. A turn of a soft-deleted chat is finalized and billed like any other.
- [ ] Orphan watchdog, unreserved and corrupt turns (feature-local; `inst-unreserved-orphaned`): a replacement turn left `running` with NULL preflight columns (crash between the mutation commit and the reserve, simulated by a test hook) is finalized `failed` / `orphan_timeout`. No `quota_usage` row changes, and one usage event with `actual_credits_micro` 0, `usage` `null`, empty models and `policy_version_applied` 0 is enqueued with one `turn_failed` audit event. A stale turn with NULL `requester_user_id` is finalized with no usage and no audit event and is not rescanned. A snapshot fetch failure finalizes the turn with the deterministic estimated fallback. Two tenants at the same `policy_version_applied` settle each with its own tenant's snapshot (one fetch per tenant); a failed fetch is retried by the next candidate; the same tenant, user and version fetch once per scan.
- [ ] Leader election (feature-local): with the no-op elector, every process scans and the CAS keeps a turn finalized once when two watchdog instances scan the same database concurrently (one usage event, one audit event). With the Lease elector against a fake Lease API, the first pod creates `mini-chat-orphan-watchdog` with `leaseDurationSeconds` 15 and becomes leader. A second pod stays follower while the first renews every 2 s. It takes over only after the record is unchanged for 15 s on its own clock. The first pod stops reporting leadership 10 s after its last successful renewal. A 409 on renew or take-over leaves the pod follower. On stop the holder clears `holderIdentity`, and the other pod takes over within one renew period. The upload reaper scans only while leader of `mini-chat-upload-reaper`, independently of the watchdog role. A `k8s` build without `POD_NAMESPACE` or `POD_NAME` fails `start` naming the variable. `cargo check --features k8s --offline` succeeds without changing `Cargo.lock`.
- [ ] Worker metrics (feature-local): every instrument of `cpt-cf-mini-chat-algo-background-workers-record-metrics` is recorded with exactly the listed label values in the scenarios above, and no label carries a tenant, user, chat, turn, request, attachment, file or vector store id.
- [ ] Quality gates: `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass, and every implemented CDSL ID of this feature (and the deferred transitions `inst-unreserved-orphaned` and `inst-bound-to-absent`) has `@cpt-begin` / `@cpt-end` markers.
